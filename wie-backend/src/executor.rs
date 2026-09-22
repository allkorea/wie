use alloc::{boxed::Box, sync::Arc, task::Wake, vec::Vec};
use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, Waker},
};

use hashbrown::HashMap;
use spin::Mutex;

use wie_util::{Result, WieError};

use crate::time::Instant;

type Task = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

struct TaskWake(AtomicBool);

impl Wake for TaskWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

pub struct ExecutorInner {
    current_task_id: Option<usize>,
    tasks: HashMap<usize, Option<Task>>,
    sleeping_tasks: HashMap<usize, Instant>,
    task_ids: Vec<usize>,
    last_task_id: usize,
    last_now: Instant,
    stopped: bool,
}

pub trait AsyncCallable<R>: Send
where
    R: Send,
{
    fn call(self) -> impl Future<Output = R> + Send;
}

impl<F, R, Fut> AsyncCallable<R> for F
where
    F: FnOnce() -> Fut + 'static + Send,
    R: AsyncCallableResult,
    Fut: Future<Output = R> + 'static + Send,
{
    async fn call(self) -> R {
        self().await
    }
}

pub trait AsyncCallableResult: Send {
    fn err(self) -> Option<WieError>;
}

impl<R> AsyncCallableResult for core::result::Result<R, WieError>
where
    R: Send,
{
    fn err(self) -> Option<WieError> {
        self.err()
    }
}

impl AsyncCallableResult for () {
    fn err(self) -> Option<WieError> {
        None
    }
}

#[derive(Clone)]
pub struct Executor {
    inner: Arc<Mutex<ExecutorInner>>,
    wake: Arc<TaskWake>,
}

impl Executor {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let inner = Arc::new(Mutex::new(ExecutorInner {
            current_task_id: None,
            tasks: HashMap::new(),
            sleeping_tasks: HashMap::new(),
            task_ids: Vec::new(),
            last_task_id: 0,
            last_now: Instant::from_epoch_millis(0),
            stopped: false,
        }));

        Self {
            inner,
            wake: Arc::new(TaskWake(AtomicBool::new(false))),
        }
    }

    /// Returns zero if the runtime owner has already shut down this executor.
    pub fn spawn<C, R>(&self, callable: C) -> usize
    where
        C: AsyncCallable<R> + 'static,
        R: AsyncCallableResult,
    {
        let fut = async move {
            let result = callable.call().await;
            if let Some(err) = result.err() {
                return Err(err);
            }

            Ok(())
        };

        let task_id = {
            let mut inner = self.inner.lock();
            if inner.stopped {
                return 0;
            }
            inner.last_task_id += 1;
            let task_id = inner.last_task_id;
            inner.tasks.insert(task_id, Some(Box::pin(fut)));
            task_id
        };

        self.wake.wake_by_ref();

        task_id
    }

    pub fn shutdown(&self) {
        let tasks = {
            let mut inner = self.inner.lock();
            inner.stopped = true;
            inner.sleeping_tasks.clear();
            core::mem::take(&mut inner.tasks)
        };
        // Futures can own this executor and ARM guards whose destructors take locks.
        drop(tasks);
    }

    // TODO we need to remove error handling from here. we need to JoinHandle like on spawn..
    pub fn tick<T, B>(&mut self, now: T, budget_now: B) -> Result<bool>
    where
        T: Fn() -> Instant,
        B: Fn() -> u64,
    {
        let started = budget_now();
        loop {
            if budget_now().saturating_sub(started) >= 8 {
                let now = now();
                let inner = self.inner.lock();
                return Ok(!inner.stopped
                    && self.wake.0.load(Ordering::Acquire)
                    && inner
                        .tasks
                        .keys()
                        .any(|id| inner.sleeping_tasks.get(id).is_none_or(|until| *until <= now)));
            }
            let now = now();

            {
                let inner = self.inner.lock();
                if inner.stopped {
                    break;
                }
                let running_task_count = inner.tasks.len() - inner.sleeping_tasks.len();
                if running_task_count == 0 && !inner.sleeping_tasks.is_empty() {
                    let next_wakeup = *inner.sleeping_tasks.values().min().unwrap();
                    if now < next_wakeup {
                        break;
                    }
                }
            }

            self.wake.0.store(false, Ordering::Release);
            self.step(now)?;
            if !self.wake.0.load(Ordering::Acquire) {
                break;
            }
        }

        Ok(false)
    }

    pub fn current_task_id(&self) -> u64 {
        self.inner.lock().current_task_id.unwrap() as _
    }

    fn step(&mut self, now: Instant) -> Result<()> {
        let mut task_ids = {
            let mut inner = self.inner.lock();
            inner.last_now = now;
            let mut task_ids = core::mem::take(&mut inner.task_ids);
            task_ids.extend(inner.tasks.keys().copied());
            task_ids
        };

        let mut first_error = None;
        let waker = Waker::from(self.wake.clone());

        for task_id in task_ids.iter().copied() {
            let (mut task, previous_task_id) = {
                let mut inner = self.inner.lock();
                if inner.stopped {
                    break;
                }
                if inner.sleeping_tasks.get(&task_id).is_some_and(|until| *until > now) {
                    continue;
                }
                inner.sleeping_tasks.remove(&task_id);
                let Some(task) = inner.tasks.get_mut(&task_id).and_then(Option::take) else {
                    continue;
                };
                (task, inner.current_task_id.replace(task_id))
            };

            let mut context = Context::from_waker(&waker);
            let result = task.as_mut().poll(&mut context);
            {
                let mut inner = self.inner.lock();
                inner.current_task_id = previous_task_id;
                if !inner.stopped && result.is_pending() {
                    // The occupied slot preserves the map allocation and iteration order.
                    *inner.tasks.get_mut(&task_id).unwrap() = Some(task);
                    continue;
                }
                inner.tasks.remove(&task_id);
                inner.sleeping_tasks.remove(&task_id);
            }
            // Completed and cancelled futures can re-enter the executor on drop.
            drop(task);
            if let Poll::Ready(Err(err)) = result
                && first_error.is_none()
            {
                first_error = Some(err);
            }
        }
        task_ids.clear();
        self.inner.lock().task_ids = task_ids;

        if let Some(err) = first_error { Err(err) } else { Ok(()) }
    }

    pub(crate) fn sleep(&self, timeout: u64) {
        let mut inner = self.inner.lock();
        if inner.stopped {
            return;
        }
        let task_id = inner.current_task_id.unwrap();
        let until = inner.last_now + timeout;
        inner.sleeping_tasks.insert(task_id, until);
        self.wake.wake_by_ref();
    }
}

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, sync::Arc};
    use core::{
        cell::Cell,
        future::{Future, poll_fn},
        pin::Pin,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        task::{Context, Poll},
    };

    use wie_util::WieError;

    use super::Executor;
    use crate::time::Instant;

    struct YieldOnce(bool);

    impl Future for YieldOnce {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    fn advancing_clock(start: u64) -> impl Fn() -> Instant {
        let time = Cell::new(start);
        move || {
            let now = time.get();
            time.set(now + 1);
            Instant::from_epoch_millis(now)
        }
    }

    fn advancing_budget() -> impl Fn() -> u64 {
        let clock = advancing_clock(0);
        move || clock().raw()
    }

    #[test]
    fn host_budget_expires_when_guest_time_is_frozen_or_moves_backwards() {
        for backwards in [false, true] {
            let mut executor = Executor::new();
            let polls = Arc::new(AtomicUsize::new(0));
            let observed = polls.clone();
            executor.spawn(move || async move {
                poll_fn(|cx| {
                    observed.fetch_add(1, Ordering::Relaxed);
                    cx.waker().wake_by_ref();
                    Poll::<()>::Pending
                })
                .await;
            });
            let epoch = Cell::new(1000);
            let yielded = executor
                .tick(
                    || {
                        if backwards {
                            epoch.set(epoch.get() - 1);
                        }
                        Instant::from_epoch_millis(epoch.get())
                    },
                    advancing_budget(),
                )
                .unwrap();
            assert!(yielded);
            assert_eq!(polls.load(Ordering::Relaxed), 7);
            executor.shutdown();
            assert!(!executor.tick(advancing_clock(1000), advancing_budget()).unwrap());
        }
    }

    #[test]
    fn pending_tasks_only_repeat_within_a_tick_when_woken() {
        for wakes in [0, 2] {
            let mut executor = Executor::new();
            let polls = Arc::new(AtomicUsize::new(0));
            let observed = polls.clone();
            executor.spawn(move || async move {
                poll_fn(|cx| {
                    if observed.fetch_add(1, Ordering::Relaxed) < wakes {
                        cx.waker().wake_by_ref();
                    }
                    Poll::<()>::Pending
                })
                .await;
            });
            assert!(!executor.tick(advancing_clock(0), advancing_budget()).unwrap());
            assert_eq!(polls.load(Ordering::Relaxed), wakes + 1);
            assert!(!executor.tick(advancing_clock(100), advancing_budget()).unwrap());
            assert_eq!(polls.load(Ordering::Relaxed), wakes + 2);
        }
    }

    #[test]
    fn continuation_requires_work_that_is_ready_at_the_budget_boundary() {
        for timeout in [0, 100] {
            let mut executor = Executor::new();
            let sleeper = executor.clone();
            executor.spawn(move || async move {
                crate::task::SleepFuture::new(timeout, &sleeper).await;
            });
            let budget = Cell::new(0);
            let yielded = executor
                .tick(
                    || Instant::from_epoch_millis(0),
                    || {
                        let now = budget.get();
                        budget.set(now + 4);
                        now
                    },
                )
                .unwrap();
            assert_eq!(yielded, timeout == 0);
            assert!(!executor.tick(advancing_clock(100), advancing_budget()).unwrap());
        }

        let mut executor = Executor::new();
        executor.spawn(|| async {
            poll_fn(|cx| {
                cx.waker().wake_by_ref();
                Poll::Ready(())
            })
            .await;
        });
        assert!(!executor.tick(advancing_clock(0), advancing_budget()).unwrap());
    }

    #[test]
    fn repeated_steps_reuse_task_storage_and_drop_completed_futures_outside_locks() {
        struct OnDrop(Executor);
        impl Future for OnDrop {
            type Output = wie_util::Result<()>;

            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                Poll::Ready(Ok(()))
            }
        }
        impl Drop for OnDrop {
            fn drop(&mut self) {
                let inner = self.0.inner.try_lock().unwrap();
                assert!(inner.current_task_id.is_none());
            }
        }

        let mut executor = Executor::new();
        for _ in 0..32 {
            executor.spawn(|| core::future::pending::<()>());
        }
        executor.inner.lock().tasks.insert(33, Some(Box::pin(OnDrop(executor.clone()))));
        executor.step(Instant::from_epoch_millis(0)).unwrap();
        let (capacity, ids) = {
            let inner = executor.inner.lock();
            (inner.tasks.capacity(), inner.task_ids.as_ptr())
        };
        for now in 1..100 {
            executor.step(Instant::from_epoch_millis(now)).unwrap();
            let inner = executor.inner.lock();
            assert_eq!(inner.tasks.len(), 32);
            assert_eq!(inner.tasks.capacity(), capacity);
            assert_eq!(inner.task_ids.as_ptr(), ids);
        }
        executor.shutdown();
    }

    #[test]
    fn zero_duration_sleeps_and_newly_spawned_tasks_resume_in_the_same_tick() {
        let mut executor = Executor::new();
        let completed = Arc::new(AtomicBool::new(false));
        let observed = completed.clone();
        let task_executor = executor.clone();
        executor.spawn(move || async move {
            crate::task::SleepFuture::new(0, &task_executor).await;
            task_executor.spawn(move || async move {
                observed.store(true, Ordering::Relaxed);
            });
        });
        executor.tick(advancing_clock(0), advancing_budget()).unwrap();
        assert!(completed.load(Ordering::Relaxed));
    }

    #[test]
    fn test_failed_task_preserves_others() {
        let mut executor = Executor::new();

        executor.spawn(|| async { Err::<(), _>(WieError::FatalError("test error".into())) });

        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();
        executor.spawn(move || async move {
            YieldOnce(false).await;
            completed_clone.store(true, Ordering::Relaxed);
        });

        assert!(executor.tick(advancing_clock(0), advancing_budget()).is_err());
        assert!(!completed.load(Ordering::Relaxed));

        executor.tick(advancing_clock(100), advancing_budget()).unwrap();
        assert!(completed.load(Ordering::Relaxed));
    }

    #[test]
    fn test_failed_task_preserves_sleeping_tasks() {
        let mut executor = Executor::new();

        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();
        let executor_clone = executor.clone();
        executor.spawn(move || async move {
            executor_clone.sleep(100);
            YieldOnce(false).await;
            completed_clone.store(true, Ordering::Relaxed);
        });

        executor.spawn(|| async { Err::<(), _>(WieError::FatalError("test error".into())) });

        assert!(executor.tick(advancing_clock(0), advancing_budget()).is_err());
        assert!(!completed.load(Ordering::Relaxed));

        executor.tick(advancing_clock(50), advancing_budget()).unwrap();
        assert!(!completed.load(Ordering::Relaxed));

        executor.tick(advancing_clock(200), advancing_budget()).unwrap();
        assert!(completed.load(Ordering::Relaxed));
    }

    #[test]
    fn test_all_ok_tasks_complete() {
        let mut executor = Executor::new();

        let completed_a = Arc::new(AtomicBool::new(false));
        let completed_a_clone = completed_a.clone();
        executor.spawn(move || async move {
            completed_a_clone.store(true, Ordering::Relaxed);
        });

        let completed_b = Arc::new(AtomicBool::new(false));
        let completed_b_clone = completed_b.clone();
        executor.spawn(move || async move {
            YieldOnce(false).await;
            completed_b_clone.store(true, Ordering::Relaxed);
        });

        executor.tick(advancing_clock(0), advancing_budget()).unwrap();

        assert!(completed_a.load(Ordering::Relaxed));
        assert!(completed_b.load(Ordering::Relaxed));
    }

    #[test]
    fn shutdown_drops_tasks_outside_locks_and_does_not_requeue_them() {
        struct OnDrop(Executor, Arc<AtomicUsize>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                assert!(self.0.inner.try_lock().is_some());
                assert_eq!(self.0.spawn(|| async {}), 0);
                self.1.fetch_add(1, Ordering::Relaxed);
            }
        }

        for stop_inside_poll in [false, true] {
            let mut executor = Executor::new();
            let weak = Arc::downgrade(&executor.inner);
            let drops = Arc::new(AtomicUsize::new(0));
            let guard = OnDrop(executor.clone(), drops.clone());
            executor.spawn(move || async move {
                let guard = guard;
                if stop_inside_poll {
                    guard.0.shutdown();
                }
                core::future::pending::<()>().await;
                drop(guard);
            });
            executor.tick(advancing_clock(0), advancing_budget()).unwrap();
            executor.shutdown();
            executor.shutdown();
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            assert!(executor.inner.lock().tasks.is_empty());
            drop(executor);
            assert!(weak.upgrade().is_none());
        }
    }
}
