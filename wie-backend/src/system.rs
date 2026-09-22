mod audio;
mod event_queue;
mod file_system;

use alloc::{boxed::Box, sync::Arc};

use spin::{RwLock, RwLockWriteGuard};

use wie_util::Result;

use crate::{
    AsyncCallable,
    executor::Executor,
    platform::Platform,
    task::{SleepFuture, YieldFuture},
    task_runner::TaskRunner,
};

use self::{audio::Audio, event_queue::EventQueue};

pub use self::{
    event_queue::{Event, KeyCode},
    file_system::FilesystemOverlay,
};

#[derive(Clone)]
pub struct System {
    pid: Arc<str>,
    aid: Arc<str>,
    executor: Executor,
    platform: Arc<Box<dyn Platform>>,
    filesystem: FilesystemOverlay,
    event_queue: Arc<RwLock<EventQueue>>,
    audio: Arc<RwLock<Audio>>,
    task_runner: Arc<dyn TaskRunner>,
    random_state: Arc<RwLock<u32>>,
}

impl System {
    pub fn new<T>(platform: Box<dyn Platform>, pid: &str, aid: &str, task_runner: T) -> Self
    where
        T: TaskRunner + 'static,
    {
        let audio_sink = platform.audio_sink();
        let platform = Arc::new(platform);

        Self {
            pid: Arc::from(pid),
            aid: Arc::from(aid),
            executor: Executor::new(),
            filesystem: FilesystemOverlay::new(platform.clone(), aid),
            platform,
            event_queue: Arc::new(RwLock::new(EventQueue::new())),
            audio: Arc::new(RwLock::new(Audio::new(audio_sink))),
            task_runner: Arc::new(task_runner),
            random_state: Arc::new(RwLock::new(1)),
        }
    }

    /// Terminal cleanup by the emulator owner; dropping a shared System handle does not stop it.
    pub fn shutdown(&mut self) {
        self.executor.shutdown();
        let events = core::mem::take(&mut *self.event_queue.write());
        // Timer callbacks can retain System handles; release them without the queue lock.
        drop(events);
        self.audio.write().shutdown();
    }

    pub fn tick(&mut self) -> Result<()> {
        let platform = self.platform.clone();
        self.executor.tick(|| platform.now(), || platform.monotonic_millis())
    }

    pub fn spawn<C>(&self, callable: C)
    where
        C: AsyncCallable<Result<()>> + 'static + Send,
    {
        let runner_clone = self.task_runner.clone();
        self.executor.spawn(async move || runner_clone.run(Box::pin(callable.call())).await);
    }

    pub fn sleep(&self, timeout: u64) -> SleepFuture {
        SleepFuture::new(timeout, &self.executor)
    }

    pub fn current_task_id(&self) -> u64 {
        self.executor.current_task_id()
    }

    pub fn yield_now(&self) -> YieldFuture {
        YieldFuture::new()
    }

    /// Unified filesystem view. Reads consult the persistent platform
    /// backend first and fall back to the in-memory virtual layer loaded
    /// from archives; writes always hit the platform backend.
    pub fn filesystem(&self) -> &FilesystemOverlay {
        &self.filesystem
    }

    pub fn pid(&self) -> &str {
        &self.pid
    }

    pub fn aid(&self) -> &str {
        &self.aid
    }

    pub fn random_state(&self) -> u32 {
        *self.random_state.read()
    }

    pub fn set_random_state(&self, random_state: u32) {
        *self.random_state.write() = random_state;
    }

    pub fn platform(&self) -> &dyn Platform {
        self.platform.as_ref().as_ref()
    }

    pub fn audio(&self) -> RwLockWriteGuard<'_, Audio> {
        self.audio.as_ref().write()
    }

    pub fn event_queue(&self) -> RwLockWriteGuard<'_, EventQueue> {
        self.event_queue.write()
    }
}
