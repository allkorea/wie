use alloc::boxed::Box;

use wie_util::Result;

use crate::{audio_sink::AudioSink, canvas::Font, database::DatabaseRepository, screen::Screen, time::Instant};

pub trait Platform: Send + Sync {
    fn font(&self) -> &Font;
    fn screen(&self) -> &dyn Screen;
    fn now(&self) -> Instant;
    /// Host execution budget clock, in milliseconds; advances even while guest time is paused.
    fn monotonic_millis(&self) -> u64;
    fn database_repository(&self) -> &dyn DatabaseRepository;
    fn filesystem(&self) -> &dyn Filesystem;
    fn audio_sink(&self) -> Box<dyn AudioSink>;
    fn write_stdout(&self, buf: &[u8]);
    fn write_stderr(&self, buf: &[u8]);
    fn exit(&self);
    fn vibrate(&self, duration_ms: u64, intensity: u8);
}

/// Platform filesystem abstraction. Every method is scoped by `aid`;
/// implementations MUST NOT cross aid boundaries.
#[async_trait::async_trait]
pub trait Filesystem: Send + Sync {
    async fn exists(&self, aid: &str, path: &str) -> Result<bool>;

    async fn size(&self, aid: &str, path: &str) -> Result<Option<usize>>;

    /// Read up to `count` bytes starting at `offset` into `buf[..count]`.
    ///
    /// - File missing → `None`.
    /// - `offset >= size` (read past EOF) → `Some(0)`.
    /// - Otherwise → `Some(n)` where `0 < n <= count`. Short reads allowed
    ///   at end of file.
    /// - The output is limited by both `count` and `buf.len()`.
    async fn read(&self, aid: &str, path: &str, offset: usize, count: usize, buf: &mut [u8]) -> Result<Option<usize>>;

    /// Write `data` starting at `offset`.
    ///
    /// - Creates the file (and any missing intermediate directories) if it
    ///   does not yet exist. A zero-length `data` is a valid way to
    ///   materialize an empty file.
    /// - If `offset + data.len() > current_size` the implementation MUST
    ///   automatically extend the file, zero-filling the gap.
    /// - Returns the number of bytes actually written. On success this
    ///   equals `data.len()`.
    /// - `initial` supplies archive bytes only if the persistent file is missing.
    ///   Initialization and mutation must be serialized with other mutations.
    /// - Errors must propagate; success follows completion of the write.
    async fn write(&self, aid: &str, path: &str, offset: usize, data: &[u8], initial: &[u8]) -> Result<usize>;

    /// Truncate the file to exactly `len` bytes. Creates the file if
    /// missing.
    /// - `len > current_size` → zero-fill extend.
    /// - `len < current_size` → tail bytes dropped.
    /// Uses `initial` only for a missing persistent file, as in `write`.
    async fn truncate(&self, aid: &str, path: &str, len: usize, initial: &[u8]) -> Result<()>;
}
