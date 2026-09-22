use alloc::boxed::Box;
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use wasm_bindgen::{JsCast, JsValue};
use wie_util::{Result, WieError};

struct SendWrapper<T>(T);

// Only used by the single-threaded web executor in the originating JS realm.
unsafe impl<T> Send for SendWrapper<T> {}

impl<F, R> Future for SendWrapper<Pin<Box<F>>>
where
    F: Future<Output = core::result::Result<R, JsValue>>,
{
    type Output = Result<R>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().0.as_mut().poll(cx).map(|result| {
            result.map_err(|error| {
                let message = if let Some(message) = error.as_string() {
                    message
                } else if let Some(error) = error.dyn_ref::<js_sys::Error>() {
                    error.message().into()
                } else {
                    "JavaScript storage operation failed".into()
                };
                WieError::FatalError(message)
            })
        })
    }
}

pub fn run_js_future<F, R>(future: F) -> impl Future<Output = Result<R>> + Send
where
    F: Future<Output = core::result::Result<R, JsValue>>,
{
    // Poll with the owning task's waker. Dropping that task cancels the Rust
    // continuation instead of leaving a detached spawn_local task behind.
    SendWrapper(Box::pin(future))
}
