use alloc::{borrow::Cow, vec, vec::Vec};
use core::{
    cell::RefCell,
    ops::{Deref, DerefMut, Range},
};

use wie_backend::canvas::{ArgbPixel, Canvas, Color, Image, ImageBuffer, ImageBufferCanvas, PixelType, Rgb565Pixel};
use wie_util::{Result, WieError};

use crate::context::WIPICContext;

use super::FrameBuffer;

const CACHE_BYTES: usize = 256;

// Scratch exists only during the synchronous, exclusive context borrow. It is
// flushed before that borrow ends; no framebuffer state survives between calls.
struct Pixels<'a> {
    context: &'a mut dyn WIPICContext,
    address: u32,
    size: usize,
    bytes: [u8; CACHE_BYTES],
    start: usize,
    len: usize,
    dirty: Range<usize>,
    error: Option<WieError>,
}

impl Pixels<'_> {
    fn flush_block(&mut self) -> Result<()> {
        if !self.dirty.is_empty() {
            self.context.write_bytes(
                self.address + (self.start + self.dirty.start) as u32,
                &self.bytes[self.dirty.clone()],
            )?;
            self.dirty = 0..0;
        }
        Ok(())
    }

    fn load(&mut self, offset: usize) -> Result<usize> {
        let start = offset / CACHE_BYTES * CACHE_BYTES;
        if start != self.start {
            self.flush_block()?;
            let len = (self.size - start).min(CACHE_BYTES);
            let address = self.address + start as u32;
            if self.context.read_bytes(address, &mut self.bytes[..len])? != len {
                return Err(WieError::InvalidMemoryAccess(address));
            }
            self.start = start;
            self.len = len;
        }
        Ok(offset - start)
    }

    fn read(&mut self, mut offset: usize, mut output: &mut [u8]) {
        if self.error.is_some() {
            output.fill(0);
            return;
        }
        while !output.is_empty() {
            let index = match self.load(offset) {
                Ok(index) => index,
                Err(error) => {
                    self.error = Some(error);
                    output.fill(0);
                    return;
                }
            };
            let len = (self.len - index).min(output.len());
            output[..len].copy_from_slice(&self.bytes[index..index + len]);
            output = &mut output[len..];
            offset += len;
        }
    }

    fn write(&mut self, mut offset: usize, mut input: &[u8]) {
        if self.error.is_some() {
            return;
        }
        while !input.is_empty() {
            let index = match self.load(offset) {
                Ok(index) => index,
                Err(error) => {
                    self.error = Some(error);
                    return;
                }
            };
            let len = (self.len - index).min(input.len());
            self.bytes[index..index + len].copy_from_slice(&input[..len]);
            if self.dirty.is_empty() {
                self.dirty = index..index + len;
            } else {
                self.dirty.start = self.dirty.start.min(index);
                self.dirty.end = self.dirty.end.max(index + len);
            }
            input = &input[len..];
            offset += len;
        }
    }

    fn finish(&mut self) -> Result<()> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        self.flush_block()
    }
}

struct GuestImage<'a> {
    width: u32,
    height: u32,
    stride: usize,
    bpp: usize,
    pixels: RefCell<Pixels<'a>>,
    finished: bool,
}

impl GuestImage<'_> {
    fn offset(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
            return None;
        }
        Some(y as usize * self.stride + x as usize * self.bpp)
    }

    fn decode(&self, bytes: [u8; 4]) -> Color {
        if self.bpp == 2 {
            Rgb565Pixel::to_color(u16::from_le_bytes([bytes[0], bytes[1]]))
        } else {
            ArgbPixel::to_color(u32::from_le_bytes(bytes))
        }
    }

    fn finish(mut self) -> Result<()> {
        self.finished = true;
        self.pixels.get_mut().finish()
    }
}

impl Drop for GuestImage<'_> {
    fn drop(&mut self) {
        if !self.finished && let Err(error) = self.pixels.get_mut().finish() {
            tracing::error!("Failed to flush framebuffer canvas: {error}");
        }
    }
}

impl Image for GuestImage<'_> {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn bytes_per_pixel(&self) -> u32 {
        self.bpp as u32
    }

    fn get_pixel(&self, x: i32, y: i32) -> Color {
        let mut bytes = [0; 4];
        if let Some(offset) = self.offset(x, y) {
            self.pixels.borrow_mut().read(offset, &mut bytes[..self.bpp]);
        }
        self.decode(bytes)
    }

    fn raw(&self) -> Cow<'_, [u8]> {
        let row_bytes = self.width as usize * self.bpp;
        let mut bytes = vec![0; row_bytes * self.height as usize];
        if row_bytes != 0 {
            let mut pixels = self.pixels.borrow_mut();
            for (y, row) in bytes.chunks_mut(row_bytes).enumerate() {
                pixels.read(y * self.stride, row);
            }
        }
        Cow::Owned(bytes)
    }

    fn colors(&self) -> Vec<Color> {
        (0..self.height)
            .flat_map(|y| (0..self.width).map(move |x| self.get_pixel(x as i32, y as i32)))
            .collect()
    }
}

impl ImageBuffer for GuestImage<'_> {
    fn put_pixel(&mut self, x: i32, y: i32, color: Color) {
        if let Some(offset) = self.offset(x, y) {
            let bytes = if self.bpp == 2 {
                (Rgb565Pixel::from_color(color) as u32).to_le_bytes()
            } else {
                ArgbPixel::from_color(color).to_le_bytes()
            };
            self.pixels.get_mut().write(offset, &bytes[..self.bpp]);
        }
    }

    fn put_pixels(&mut self, x: i32, y: i32, width: u32, colors: &[Color]) {
        if width == 0 {
            return;
        }
        for (row_index, row) in colors.chunks(width as usize).enumerate() {
            let py = y as i64 + row_index as i64;
            if py >= self.height as i64 {
                break;
            }
            if py < 0 {
                continue;
            }
            let start = (-(x as i64)).clamp(0, row.len() as i64) as usize;
            let end = (self.width as i64 - x as i64).clamp(0, row.len() as i64) as usize;
            if start < end {
                for (column, &color) in row[start..end].iter().enumerate() {
                    self.put_pixel((x as i64 + start as i64 + column as i64) as i32, py as i32, color);
                }
            }
        }
    }

    fn xor_pixel(&mut self, x: i32, y: i32, color: Color) {
        if let Some(offset) = self.offset(x, y) {
            let pixels = self.pixels.get_mut();
            let mut bytes = [0; 4];
            pixels.read(offset, &mut bytes[..self.bpp]);
            let bytes = if self.bpp == 2 {
                (Rgb565Pixel::xor_color(u16::from_le_bytes([bytes[0], bytes[1]]), color) as u32).to_le_bytes()
            } else {
                ArgbPixel::xor_color(u32::from_le_bytes(bytes), color).to_le_bytes()
            };
            pixels.write(offset, &bytes[..self.bpp]);
        }
    }
}

pub struct FramebufferCanvas<'a> {
    canvas: ImageBufferCanvas<GuestImage<'a>>,
}

impl<'a> FramebufferCanvas<'a> {
    pub(super) fn new(framebuffer: &FrameBuffer, context: &'a mut dyn WIPICContext) -> Result<Self> {
        let (address, size, _) = framebuffer.layout(context)?;
        Ok(Self {
            canvas: ImageBufferCanvas::new(GuestImage {
                width: framebuffer.0.width,
                height: framebuffer.0.height,
                stride: framebuffer.0.bpl as usize,
                bpp: (framebuffer.0.bpp / 8) as usize,
                pixels: RefCell::new(Pixels {
                    context,
                    address,
                    size: size as usize,
                    bytes: [0; CACHE_BYTES],
                    start: usize::MAX,
                    len: 0,
                    dirty: 0..0,
                    error: None,
                }),
                finished: false,
            }),
        })
    }

    pub fn flush(self) -> Result<()> {
        self.canvas.into_inner().finish()
    }
}

impl<'a> Deref for FramebufferCanvas<'a> {
    type Target = dyn Canvas + 'a;

    fn deref(&self) -> &Self::Target {
        &self.canvas
    }
}

impl DerefMut for FramebufferCanvas<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.canvas
    }
}
