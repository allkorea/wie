mod guest;

pub use guest::FramebufferCanvas;

use alloc::{boxed::Box, format, vec, vec::Vec};

use bytemuck::{Pod, cast_slice_mut};

use wipi_types::wipic::{WIPICFramebuffer, WIPICIndirectPtr, WIPICWord};

use wie_backend::canvas::{ArgbPixel, Color, Image, PixelType, Rgb8Pixel, Rgb565Pixel, VecImageBuffer};
use wie_util::{Result, WieError};

use crate::context::WIPICContext;

// same 256MB as wie_core_arm's HEAP_SIZE; not referenced directly to avoid the dependency
const MAX_FRAMEBUFFER_BYTES: u32 = 0x1000_0000;

fn buffer_size(width: u32, height: u32, bytes_per_pixel: u32) -> Result<(u32, u32)> {
    let bpl = width.checked_mul(bytes_per_pixel).ok_or(WieError::AllocationFailure)?;
    let size = bpl.checked_mul(height).ok_or(WieError::AllocationFailure)?;
    if size > MAX_FRAMEBUFFER_BYTES {
        return Err(WieError::AllocationFailure);
    }

    Ok((size, bpl))
}

pub struct FrameBuffer(pub WIPICFramebuffer);

impl FrameBuffer {
    pub fn empty() -> Self {
        Self(WIPICFramebuffer {
            width: 0,
            height: 0,
            bpl: 0,
            bpp: 0,
            buf: WIPICIndirectPtr(0),
        })
    }

    pub fn new(context: &mut dyn WIPICContext, width: WIPICWord, height: WIPICWord, bpp: WIPICWord) -> Result<Self> {
        let bytes_per_pixel = bpp / 8;

        let (size, bpl) = buffer_size(width, height, bytes_per_pixel)?;
        let buf = context.alloc(size)?;

        Ok(Self(WIPICFramebuffer {
            width,
            height,
            bpl,
            bpp: bytes_per_pixel * 8,
            buf,
        }))
    }

    pub fn from_image(context: &mut dyn WIPICContext, image: &dyn Image) -> Result<Self> {
        let (size, bpl) = buffer_size(image.width(), image.height(), image.bytes_per_pixel())?;
        let buf = context.alloc(size)?;

        context.write_bytes(context.data_ptr(buf)?, &image.raw())?;

        Ok(Self(WIPICFramebuffer {
            width: image.width(),
            height: image.height(),
            bpl,
            bpp: image.bytes_per_pixel() * 8,
            buf,
        }))
    }

    fn layout(&self, context: &dyn WIPICContext) -> Result<(u32, u32, u32)> {
        if !matches!(self.0.bpp, 16 | 32) {
            return Err(WieError::FatalError(format!("Unsupported pixel format: {}", self.0.bpp)));
        }
        let (_, row_bytes) = buffer_size(self.0.width, self.0.height, self.0.bpp / 8)?;
        let size = self.0.bpl.checked_mul(self.0.height).ok_or(WieError::AllocationFailure)?;
        if self.0.bpl < row_bytes
            || size > MAX_FRAMEBUFFER_BYTES
            || self.0.width > i32::MAX as u32
            || self.0.height > i32::MAX as u32
        {
            return Err(WieError::AllocationFailure);
        }
        let address = context.data_ptr(self.0.buf)?;
        if address as u64 + size as u64 > 1u64 << 32 {
            return Err(WieError::InvalidMemoryAccess(address));
        }
        Ok((address, size, row_bytes))
    }

    fn data<T: Pod>(&self, context: &dyn WIPICContext) -> Result<Vec<T>> {
        let (address, _, row_bytes) = self.layout(context)?;
        let size = row_bytes as usize * self.0.height as usize;
        let mut buf = vec![T::zeroed(); size / size_of::<T>()];
        if row_bytes != 0 {
            for (y, row) in cast_slice_mut(&mut buf).chunks_mut(row_bytes as usize).enumerate() {
                let address = address + y as u32 * self.0.bpl;
                if context.read_bytes(address, row)? != row.len() {
                    return Err(WieError::InvalidMemoryAccess(address));
                }
            }
        }
        Ok(buf)
    }

    pub fn image(&self, context: &mut dyn WIPICContext) -> Result<Box<dyn Image>> {
        Ok(match self.0.bpp {
            16 => Box::new(VecImageBuffer::<Rgb565Pixel>::from_raw(
                self.0.width as _,
                self.0.height as _,
                self.data(context)?,
            )),
            32 => Box::new(VecImageBuffer::<ArgbPixel>::from_raw(
                self.0.width as _,
                self.0.height as _,
                self.data(context)?,
            )),
            _ => return Err(WieError::FatalError(format!("Unsupported pixel format: {}", self.0.bpp))),
        })
    }

    pub fn canvas<'a>(&self, context: &'a mut dyn WIPICContext) -> Result<FramebufferCanvas<'a>> {
        FramebufferCanvas::new(self, context)
    }

    pub fn write(&self, context: &mut dyn WIPICContext, data: &[u8]) -> Result<()> {
        context.write_bytes(context.data_ptr(self.0.buf)?, data)
    }

    pub fn pixel_to_color(&self, pixel: WIPICWord) -> Color {
        match self.0.bpp {
            16 => Rgb565Pixel::to_color(pixel as u16),
            _ => Rgb8Pixel::to_color(pixel),
        }
    }
}

#[cfg(test)]
mod test {
    use wie_util::WieError;

    use crate::context::test::TestContext;

    use super::FrameBuffer;

    #[test]
    fn test_new_overflow_returns_error() {
        let mut context = TestContext::new();

        assert!(matches!(
            FrameBuffer::new(&mut context, 0x10000, 0x10000, 32),
            Err(WieError::AllocationFailure)
        ));
    }

    #[test]
    fn test_new_over_heap_limit_returns_error() {
        let mut context = TestContext::new();

        assert!(matches!(
            FrameBuffer::new(&mut context, 0x4000, 0x4000, 32),
            Err(WieError::AllocationFailure)
        ));
    }

    #[test]
    fn test_new_zero_height_bpl_overflow_returns_error() {
        let mut context = TestContext::new();

        assert!(matches!(
            FrameBuffer::new(&mut context, 0xffff_ffff, 0, 32),
            Err(WieError::AllocationFailure)
        ));
    }

    #[test]
    fn test_new_normal_size_ok() {
        let mut context = TestContext::new();

        for bpp in [16, 32] {
            let framebuffer = FrameBuffer::new(&mut context, 100, 100, bpp).unwrap();
            assert_eq!(framebuffer.0.width, 100);
            assert_eq!(framebuffer.0.height, 100);
            assert_eq!(framebuffer.0.bpl, 100 * bpp / 8);
            assert_eq!(framebuffer.0.bpp, bpp);
            let pixels = (0..100 * 100 * bpp / 8).map(|i| i as u8).collect::<alloc::vec::Vec<_>>();
            framebuffer.write(&mut context, &pixels).unwrap();
            assert_eq!(&*framebuffer.image(&mut context).unwrap().raw(), pixels.as_slice());
            let canvas = framebuffer.canvas(&mut context).unwrap();
            assert_eq!(&*canvas.image().raw(), pixels.as_slice());
            canvas.flush().unwrap();
            assert_eq!(&*framebuffer.image(&mut context).unwrap().raw(), pixels.as_slice());
        }
    }

    #[test]
    fn guest_canvas_matches_snapshot_rasterization_with_padded_rows() {
        use alloc::{boxed::Box, vec};
        use wie_backend::canvas::{ArgbPixel, Canvas, Clip, Color, ImageBufferCanvas, Rgb565Pixel, VecImageBuffer};
        use wie_util::{ByteRead, ByteWrite};
        use wipi_types::wipic::WIPICFramebuffer;
        use crate::context::WIPICContext;

        for bpp in [16, 32] {
            let mut context = TestContext::new();
            let row_bytes = 65 * bpp / 8;
            let stride = row_bytes + 1;
            let buf = context.alloc(stride * 3).unwrap();
            let address = context.data_ptr(buf).unwrap();
            context.write_bytes(address, &vec![0; (stride * 3) as usize]).unwrap();
            for y in 0..3 {
                context.write_bytes(address + y * stride + row_bytes, &[0xa5]).unwrap();
            }
            let framebuffer = FrameBuffer(WIPICFramebuffer { width: 65, height: 3, bpl: stride, bpp, buf });
            let mut reference: Box<dyn Canvas> = if bpp == 16 {
                Box::new(ImageBufferCanvas::new(VecImageBuffer::<Rgb565Pixel>::new(65, 3)))
            } else {
                Box::new(ImageBufferCanvas::new(VecImageBuffer::<ArgbPixel>::new(65, 3)))
            };
            let clip = Clip { x: 0, y: 0, width: 65, height: 3 };
            let color = Color { a: 255, r: 231, g: 123, b: 67 };
            let source = VecImageBuffer::<ArgbPixel>::from_raw(2, 1, vec![0x80123456, 0xfffedcba]);
            let draw = |canvas: &mut dyn Canvas| {
                canvas.fill_rect(-1, -1, 70, 5, color, clip);
                canvas.draw_line(0, 2, 64, 0, Color { a: 255, r: 0, g: 0, b: 0 }, clip);
                canvas.set_xor_mode(true);
                canvas.fill_rect(62, 1, 3, 2, color, clip);
                canvas.set_xor_mode(false);
                canvas.draw(63, 1, 2, 1, &source, 0, 0, clip);
                canvas.copy_area(1, 1, 0, 0, 64, 2, clip);
                canvas.put_pixel(64, 2, color, clip);
            };
            draw(&mut *reference);
            let mut canvas = framebuffer.canvas(&mut context).unwrap();
            draw(&mut *canvas);
            canvas.flush().unwrap();
            assert_eq!(&*framebuffer.image(&mut context).unwrap().raw(), &*reference.image().raw());
            for y in 0..3 {
                let mut padding = [0];
                context.read_bytes(address + y * stride + row_bytes, &mut padding).unwrap();
                assert_eq!(padding, [0xa5]);
            }
        }
    }
}
