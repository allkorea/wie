use alloc::{sync::Arc, vec::Vec};
use core::{
    cell::RefCell,
    sync::atomic::{AtomicBool, Ordering},
};

use js_sys::Uint8ClampedArray;
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};

use wie_backend::{Screen, canvas::Image};
use wie_util::Result;

#[derive(Default)]
struct PaintState {
    context: Option<CanvasRenderingContext2d>,
    rgba: Vec<u8>,
    frame: Option<(ImageData, Uint8ClampedArray)>,
}

pub struct WindowImpl {
    canvas: HtmlCanvasElement,
    should_redraw: Arc<AtomicBool>,
    paint: RefCell<PaintState>,
}

unsafe impl Send for WindowImpl {} // XXX We're on wasm, so it's fine
unsafe impl Sync for WindowImpl {}

impl WindowImpl {
    pub fn new(canvas: HtmlCanvasElement, should_redraw: Arc<AtomicBool>) -> Self {
        Self {
            canvas,
            should_redraw,
            paint: RefCell::new(PaintState::default()),
        }
    }
}

impl Screen for WindowImpl {
    fn resize(&self, width: u32, height: u32) -> Result<()> {
        self.canvas.set_width(width);
        self.canvas.set_height(height);
        self.request_redraw()
    }

    fn request_redraw(&self) -> Result<()> {
        self.should_redraw.store(true, Ordering::SeqCst);

        Ok(())
    }

    fn paint(&self, image: &dyn Image) {
        let mut paint = self.paint.borrow_mut();
        if paint.context.is_none() {
            paint.context = Some(
                self.canvas
                    .get_context("2d")
                    .unwrap()
                    .unwrap()
                    .dyn_into::<CanvasRenderingContext2d>()
                    .unwrap(),
            );
        }
        image.copy_rgba(&mut paint.rgba);
        let width = self.width();
        let height = self.height();
        if paint
            .frame
            .as_ref()
            .is_none_or(|(data, _)| data.width() != width || data.height() != height)
        {
            // JS owns this storage; no view into growable WASM memory survives the call.
            let pixels = Uint8ClampedArray::new_with_length(paint.rgba.len() as u32);
            let data = ImageData::new_with_js_u8_clamped_array_and_sh(&pixels, width, height).unwrap();
            paint.frame = Some((data, pixels));
        }
        let (data, pixels) = paint.frame.as_ref().unwrap();
        pixels.copy_from(&paint.rgba);
        paint.context.as_ref().unwrap().put_image_data(data, 0.0, 0.0).unwrap();
    }

    fn width(&self) -> u32 {
        self.canvas.width()
    }

    fn height(&self) -> u32 {
        self.canvas.height()
    }
}
