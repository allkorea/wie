use wie_backend::canvas::Clip;
use wie_util::{Result, WieError};

use crate::WIPICContext;

use super::{FrameBuffer, write_canvas};

pub(super) fn framebuffer(
    context: &mut dyn WIPICContext,
    destination: &FrameBuffer,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    source: &FrameBuffer,
    source_x: i32,
    source_y: i32,
    clip: Clip,
) -> Result<()> {
    let (source_address, source_size, _) = source.layout(context)?;
    let (destination_address, destination_size, _) = destination.layout(context)?;
    let x_start = 0i64.max(-(x as i64)).max(-(source_x as i64)).max(clip.x as i64 - x as i64);
    let y_start = 0i64.max(-(y as i64)).max(-(source_y as i64)).max(clip.y as i64 - y as i64);
    let x_end = (width as i64)
        .min(source.0.width as i64 - source_x as i64)
        .min(destination.0.width as i64 - x as i64)
        .min(clip.x as i64 + clip.width as i64 - x as i64);
    let y_end = (height as i64)
        .min(source.0.height as i64 - source_y as i64)
        .min(destination.0.height as i64 - y as i64)
        .min(clip.y as i64 + clip.height as i64 - y as i64);
    if x_start >= x_end || y_start >= y_end {
        return Ok(());
    }
    let sx = (source_x as i64 + x_start) as u32;
    let sy = (source_y as i64 + y_start) as u32;
    let dx = (x as i64 + x_start) as u32;
    let dy = (y as i64 + y_start) as u32;
    let width = (x_end - x_start) as u32;
    let height = (y_end - y_start) as u32;

    // RGB565 is always opaque, and its color round trip preserves every native bit.
    if source.0.bpp == 16 && destination.0.bpp == 16 {
        if source_address == destination_address
            && source.0.width == destination.0.width
            && source.0.height == destination.0.height
            && source.0.bpl == destination.0.bpl
        {
            return write_canvas(context, destination, |canvas| {
                canvas.copy_area(dx as i32, dy as i32, sx as i32, sy as i32, width, height, clip)
            });
        }
        let disjoint = source_address as u64 + source_size as u64 <= destination_address as u64
            || destination_address as u64 + destination_size as u64 <= source_address as u64;
        if disjoint {
            let mut bytes = [0; 256];
            let row_bytes = width * 2;
            for row in 0..height {
                let src = source_address + (sy + row) * source.0.bpl + sx * 2;
                let dst = destination_address + (dy + row) * destination.0.bpl + dx * 2;
                for offset in (0..row_bytes).step_by(bytes.len()) {
                    let len = (row_bytes - offset).min(bytes.len() as u32) as usize;
                    if context.read_bytes(src + offset, &mut bytes[..len])? != len {
                        return Err(WieError::InvalidMemoryAccess(src + offset));
                    }
                    context.write_bytes(dst + offset, &bytes[..len])?;
                }
            }
            return Ok(());
        }
    }

    // Different views may alias, and ARGB copies use blending rather than raw copy.
    // Snapshot only the visible source rectangle before any destination write.
    let image = source.image_region(context, sx, sy, width, height)?;
    write_canvas(context, destination, |canvas| {
        canvas.draw(dx as i32, dy as i32, width, height, &*image, 0, 0, clip)
    })
}
