use alloc::boxed::Box;
use core::{
    pin::Pin,
    task::{Context, Poll},
};
use wie_util::Result;

use crate::{ArmCore, ThreadId};

pub struct ArmCoreThreadWrapper {
    // Fields drop in declaration order: guest state must outlive the future's destructors.
    future: Pin<Box<dyn Future<Output = Result<()>> + Send>>,
    thread: ThreadContextOwner,
}

struct ThreadContextOwner {
    core: ArmCore,
    thread_id: ThreadId,
}

impl Drop for ThreadContextOwner {
    fn drop(&mut self) {
        self.core.delete_thread_context(self.thread_id);
    }
}

impl ArmCoreThreadWrapper {
    pub fn new<F, Fut>(core: ArmCore, thread_id: ThreadId, entry: F) -> Result<Self>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let thread = ThreadContextOwner { core, thread_id };
        Ok(Self {
            future: Box::pin(entry()),
            thread,
        })
    }
}

impl Future for ArmCoreThreadWrapper {
    type Output = Result<()>;

    #[tracing::instrument(name = "native thread", fields(id = self.thread.thread_id), skip_all)]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let _guard = self.thread.core.enter_thread_context(self.thread.thread_id);

        if let Some(debug) = self.thread.core.debug_inner()
            && !debug.is_thread_resumed(self.thread.thread_id)
        {
            return Poll::Pending;
        }

        self.future.as_mut().poll(cx)
    }
}

impl Unpin for ArmCoreThreadWrapper {}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::Waker,
    };

    use crate::Allocator;

    use super::*;

    #[test]
    fn future_destructors_run_before_thread_context_is_released() {
        struct OnDrop(ArmCore, Arc<AtomicUsize>);

        impl Drop for OnDrop {
            fn drop(&mut self) {
                let ids = self.0.get_thread_ids();
                assert_eq!(ids.len(), 1);
                self.0.read_thread_context(ids[0]).unwrap();
                self.1.fetch_add(1, Ordering::Relaxed);
            }
        }

        let mut core = ArmCore::new(false, None).unwrap();
        Allocator::init(&mut core).unwrap();
        let drops = Arc::new(AtomicUsize::new(0));

        for poll in [false, true] {
            let guard = OnDrop(core.clone(), drops.clone());
            let mut task = core
                .run_in_thread(move || async move {
                    let guard = guard;
                    core::future::pending::<()>().await;
                    drop(guard);
                    Ok(())
                })
                .unwrap();
            if poll {
                assert!(Pin::new(&mut task).poll(&mut Context::from_waker(Waker::noop())).is_pending());
            }
            drop(task);
            assert!(core.get_thread_ids().is_empty());
        }

        assert_eq!(drops.load(Ordering::Relaxed), 2);
        core.shutdown();
    }
}
