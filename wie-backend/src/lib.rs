#![no_std]
extern crate alloc;

mod archive;
mod audio_sink;
pub mod canvas;
mod database;
mod executor;
mod platform;
mod screen;
mod system;
mod task;
mod task_runner;
pub mod text_layout;
mod time;

pub use self::{
    archive::{Archive, extract_zip},
    audio_sink::{AudioCommand, AudioEventData, AudioHandle, AudioSequence, AudioSink, TimedAudioEvent},
    canvas::Font,
    database::{Database, DatabaseRepository, RecordId},
    executor::{AsyncCallable, AsyncCallableResult},
    platform::{Filesystem, Platform},
    screen::Screen,
    system::{Event, FilesystemOverlay, KeyCode, System},
    task::YieldFuture,
    task_runner::{DefaultTaskRunner, TaskRunner},
    time::Instant,
};

use alloc::{boxed::Box, vec::Vec};

use wie_util::Result;

pub trait Emulator {
    fn handle_event(&mut self, event: Event);
    fn tick(&mut self) -> Result<()>;
}

pub struct ProfileSample {
    /// Leaf-first call stack: [pc, lr, lr_prev, ...].
    pub stack: Vec<u32>,
    pub count: u64,
}

/// Called periodically during execution with a batch of samples that the
/// profiler accumulated since the previous flush. The callback also fires once
/// more when the runtime shuts down to drain anything still in the buffer.
pub type ProfileCallback = Box<dyn FnMut(Vec<ProfileSample>) + Send + Sync>;

pub struct Options {
    pub enable_gdbserver: bool,
    pub profile: Option<ProfileCallback>,
}
