use alloc::{rc::Rc, sync::Arc, vec::Vec};
use core::{
    cell::RefCell,
    sync::atomic::{AtomicBool, Ordering},
};

use js_sys::Uint8ClampedArray;
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData, OffscreenCanvas, OffscreenCanvasRenderingContext2d};

use wie_backend::{Screen, canvas::Image};
use wie_util::{Result, WieError};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(typescript_type = "HTMLCanvasElement | OffscreenCanvas")]
    pub type Canvas;
}

#[derive(Clone)]
enum Surface {
    Html(HtmlCanvasElement, CanvasRenderingContext2d),
    Offscreen(OffscreenCanvas, OffscreenCanvasRenderingContext2d),
}

#[derive(Default)]
struct PaintState {
    rgba: Vec<u8>,
    frame: Option<(ImageData, Uint8ClampedArray)>,
    painted: bool,
    error: Option<JsValue>,
}

#[derive(Clone)]
pub struct WindowImpl {
    surface: Surface,
    should_redraw: Arc<AtomicBool>,
    paint: Rc<RefCell<PaintState>>,
}

// All canvas access stays in the single-threaded web runtime's JS realm.
unsafe impl Send for WindowImpl {}
unsafe impl Sync for WindowImpl {}

impl WindowImpl {
    pub fn new(canvas: Canvas, should_redraw: Arc<AtomicBool>) -> core::result::Result<Self, JsValue> {
        let value: JsValue = canvas.into();
        let surface = if let Some(canvas) = value.dyn_ref::<HtmlCanvasElement>() {
            let context = canvas
                .get_context("2d")?
                .ok_or_else(|| JsValue::from_str("Canvas 2D is unavailable"))?
                .dyn_into()?;
            Surface::Html(canvas.clone(), context)
        } else {
            let canvas: OffscreenCanvas = value.dyn_into()?;
            let context = canvas
                .get_context("2d")?
                .ok_or_else(|| JsValue::from_str("OffscreenCanvas 2D is unavailable"))?
                .dyn_into()?;
            Surface::Offscreen(canvas, context)
        };
        Ok(Self {
            surface,
            should_redraw,
            paint: Rc::new(RefCell::new(PaintState::default())),
        })
    }

    pub fn take_frame(&self) -> bool {
        core::mem::take(&mut self.paint.borrow_mut().painted)
    }

    pub fn take_error(&self) -> Option<JsValue> {
        self.paint.borrow_mut().error.take()
    }

    fn paint_image(&self, image: &dyn Image) -> core::result::Result<(), JsValue> {
        let mut paint = self.paint.borrow_mut();
        image.copy_rgba(&mut paint.rgba);
        let width = self.width();
        let height = self.height();
        let length = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| JsValue::from_str("Canvas dimensions overflow"))?;
        if paint.rgba.len() != length as usize {
            return Err(JsValue::from_str("Image dimensions differ from canvas"));
        }
        if paint
            .frame
            .as_ref()
            .is_none_or(|(data, _)| data.width() != width || data.height() != height)
        {
            // JS owns the pixels; no view into growable WASM memory survives.
            let pixels = Uint8ClampedArray::new_with_length(length);
            let data = ImageData::new_with_js_u8_clamped_array_and_sh(&pixels, width, height)?;
            paint.frame = Some((data, pixels));
        }
        if let Some((data, pixels)) = paint.frame.as_ref() {
            pixels.copy_from(&paint.rgba);
            match &self.surface {
                Surface::Html(_, context) => context.put_image_data(data, 0.0, 0.0)?,
                Surface::Offscreen(_, context) => context.put_image_data(data, 0.0, 0.0)?,
            }
            paint.painted = true;
        }
        Ok(())
    }
}

impl Screen for WindowImpl {
    fn resize(&self, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 || width.checked_mul(height).is_none_or(|pixels| pixels > 16 * 1024 * 1024) {
            return Err(WieError::FatalError("unsupported canvas dimensions".into()));
        }
        match &self.surface {
            Surface::Html(canvas, _) => {
                canvas.set_width(width);
                canvas.set_height(height);
            }
            Surface::Offscreen(canvas, _) => {
                canvas.set_width(width);
                canvas.set_height(height);
            }
        }
        self.paint.borrow_mut().painted = false;
        self.request_redraw()
    }

    fn request_redraw(&self) -> Result<()> {
        self.should_redraw.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn paint(&self, image: &dyn Image) {
        if let Err(error) = self.paint_image(image) {
            self.paint.borrow_mut().error = Some(error);
        }
    }

    fn width(&self) -> u32 {
        match &self.surface {
            Surface::Html(canvas, _) => canvas.width(),
            Surface::Offscreen(canvas, _) => canvas.width(),
        }
    }

    fn height(&self) -> u32 {
        match &self.surface {
            Surface::Html(canvas, _) => canvas.height(),
            Surface::Offscreen(canvas, _) => canvas.height(),
        }
    }
}
