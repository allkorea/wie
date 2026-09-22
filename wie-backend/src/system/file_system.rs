use alloc::{borrow::ToOwned, boxed::Box, string::String, sync::Arc, vec::Vec};
use core::cmp::min;

use hashbrown::HashMap;
use spin::Mutex;
use wie_util::{Result, WieError};

use crate::platform::Platform;

/// Normalize a guest-supplied path so both overlay layers see the same key.
///
/// - Leading `/` are stripped (archive paths often carry them).
/// - `.` segments are dropped.
/// - `..` segments, trailing `/`, backslashes, and empty results all
///   return `None`.
fn normalize_guest_path(path: &str) -> Option<String> {
    if path.contains('\\') {
        return None;
    }

    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.ends_with('/') {
        return None;
    }

    let mut out = String::new();
    for seg in trimmed.split('/') {
        match seg {
            "" => continue,
            "." => continue,
            ".." => return None,
            normal => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(normal);
            }
        }
    }

    if out.is_empty() { None } else { Some(out) }
}

/// Unified filesystem view exposed by `System::filesystem()`.
///
/// Wraps the persistent `Platform::filesystem()` backend and an in-memory
/// virtual layer holding archive resources. Writes always hit the platform
/// backend; reads prefer the platform backend and fall back to the virtual
/// layer. Paths are normalized internally so callers pass raw guest paths.
#[derive(Clone)]
pub struct FilesystemOverlay {
    platform: Arc<Box<dyn Platform>>,
    virtual_files: Arc<Mutex<HashMap<String, Arc<Vec<u8>>>>>,
    aid: Arc<str>,
}

impl FilesystemOverlay {
    pub fn new(platform: Arc<Box<dyn Platform>>, aid: &str) -> Self {
        Self {
            platform,
            virtual_files: Arc::new(Mutex::new(HashMap::new())),
            aid: Arc::from(aid),
        }
    }

    pub fn add_virtual(&self, path: &str, data: Vec<u8>) {
        let key = normalize_guest_path(path).unwrap_or_else(|| path.trim_start_matches('/').to_owned());
        self.virtual_files.lock().insert(key, data.into());
    }

    pub fn is_valid_path(&self, path: &str) -> bool {
        normalize_guest_path(path).is_some()
    }

    pub async fn exists(&self, path: &str) -> Result<bool> {
        let Some(normalized) = normalize_guest_path(path) else {
            return Ok(false);
        };

        if self.platform.filesystem().exists(&self.aid, &normalized).await? {
            return Ok(true);
        }
        Ok(self.virtual_files.lock().contains_key(&normalized))
    }

    pub async fn size(&self, path: &str) -> Result<Option<usize>> {
        let Some(normalized) = normalize_guest_path(path) else { return Ok(None) };

        if let Some(size) = self.platform.filesystem().size(&self.aid, &normalized).await? {
            return Ok(Some(size));
        }
        Ok(self.virtual_files.lock().get(&normalized).map(|d| d.len()))
    }

    pub async fn read(&self, path: &str, offset: usize, count: usize, buf: &mut [u8]) -> Result<Option<usize>> {
        let Some(normalized) = normalize_guest_path(path) else { return Ok(None) };
        let count = count.min(buf.len());

        let plat_fs = self.platform.filesystem();
        if let Some(read) = plat_fs.read(&self.aid, &normalized, offset, count, buf).await? {
            return Ok(Some(read));
        }

        let files = self.virtual_files.lock();
        let Some(data) = files.get(&normalized) else { return Ok(None) };
        if offset >= data.len() {
            return Ok(Some(0));
        }
        let n = min(count, data.len() - offset);
        buf[..n].copy_from_slice(&data[offset..offset + n]);
        Ok(Some(n))
    }

    pub async fn write(&self, path: &str, offset: usize, data: &[u8]) -> Result<usize> {
        let normalized = normalize_guest_path(path).ok_or_else(|| WieError::FatalError("invalid file path".into()))?;
        let initial = self.virtual_files.lock().get(&normalized).cloned().unwrap_or_default();
        self.platform.filesystem().write(&self.aid, &normalized, offset, data, &initial).await
    }

    pub async fn truncate(&self, path: &str, len: usize) -> Result<()> {
        let normalized = normalize_guest_path(path).ok_or_else(|| WieError::FatalError("invalid file path".into()))?;
        let initial = self.virtual_files.lock().get(&normalized).cloned().unwrap_or_default();
        self.platform.filesystem().truncate(&self.aid, &normalized, len, &initial).await
    }
}

#[cfg(test)]
mod tests {
    use alloc::{
        boxed::Box,
        string::{String, ToString},
        sync::Arc,
        vec,
        vec::Vec,
    };

    use hashbrown::HashMap;
    use spin::Mutex;
    use wie_util::{Result, WieError};

    use crate::{
        audio_sink::AudioSink,
        canvas::Font,
        database::DatabaseRepository,
        platform::{Filesystem, Platform},
        screen::Screen,
        time::Instant,
    };

    use super::FilesystemOverlay;

    #[derive(Default)]
    struct StubFilesystem {
        files: Mutex<HashMap<(String, String), Vec<u8>>>,
        write_limit: Option<usize>,
        fail_truncate: bool,
    }
    #[async_trait::async_trait]
    impl Filesystem for StubFilesystem {
        async fn exists(&self, aid: &str, path: &str) -> Result<bool> {
            Ok(self.files.lock().contains_key(&(aid.to_string(), path.to_string())))
        }
        async fn size(&self, aid: &str, path: &str) -> Result<Option<usize>> {
            Ok(self.files.lock().get(&(aid.to_string(), path.to_string())).map(|v| v.len()))
        }
        async fn read(&self, aid: &str, path: &str, offset: usize, count: usize, buf: &mut [u8]) -> Result<Option<usize>> {
            let files = self.files.lock();
            let Some(data) = files.get(&(aid.to_string(), path.to_string())) else {
                return Ok(None);
            };
            if offset >= data.len() {
                return Ok(Some(0));
            }
            let n = core::cmp::min(count.min(buf.len()), data.len() - offset);
            buf[..n].copy_from_slice(&data[offset..offset + n]);
            Ok(Some(n))
        }
        async fn write(&self, aid: &str, path: &str, offset: usize, data: &[u8], initial: &[u8]) -> Result<usize> {
            let write_len = self.write_limit.unwrap_or(data.len()).min(data.len());
            let mut files = self.files.lock();
            let file = files.entry((aid.to_string(), path.to_string())).or_insert_with(|| initial.to_vec());
            if file.len() < offset + write_len {
                file.resize(offset + write_len, 0);
            }
            file[offset..offset + write_len].copy_from_slice(&data[..write_len]);
            Ok(write_len)
        }
        async fn truncate(&self, aid: &str, path: &str, len: usize, initial: &[u8]) -> Result<()> {
            if self.fail_truncate {
                return Err(WieError::FatalError("test truncate failure".into()));
            }
            let mut files = self.files.lock();
            let file = files.entry((aid.to_string(), path.to_string())).or_insert_with(|| initial.to_vec());
            file.resize(len, 0);
            Ok(())
        }
    }

    struct StubPlatform {
        fs: StubFilesystem,
    }
    impl Platform for StubPlatform {
        fn font(&self) -> &Font {
            unimplemented!()
        }
        fn screen(&self) -> &dyn Screen {
            unimplemented!()
        }
        fn now(&self) -> Instant {
            Instant::from_epoch_millis(0)
        }
        fn monotonic_millis(&self) -> u64 {
            unimplemented!()
        }
        fn database_repository(&self) -> &dyn DatabaseRepository {
            unimplemented!()
        }
        fn filesystem(&self) -> &dyn Filesystem {
            &self.fs
        }
        fn audio_sink(&self) -> Box<dyn AudioSink> {
            unimplemented!()
        }
        fn write_stdout(&self, _buf: &[u8]) {}
        fn write_stderr(&self, _buf: &[u8]) {}
        fn exit(&self) {}
        fn vibrate(&self, _duration_ms: u64, _intensity: u8) {}
    }

    fn setup() -> FilesystemOverlay {
        setup_with_filesystem(StubFilesystem::default())
    }

    fn setup_with_filesystem(fs: StubFilesystem) -> FilesystemOverlay {
        let platform: Arc<Box<dyn Platform>> = Arc::new(Box::new(StubPlatform { fs }));
        FilesystemOverlay::new(platform, "test-aid")
    }

    #[futures_test::test]
    async fn add_then_read_virtual() {
        let fs = setup();
        fs.add_virtual("a.bin", vec![1, 2, 3, 4]);

        let mut buf = [0u8; 4];
        assert_eq!(fs.read("a.bin", 0, 4, &mut buf).await.unwrap(), Some(4));
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[futures_test::test]
    async fn size_falls_through_to_virtual() {
        let fs = setup();
        fs.add_virtual("x", vec![0; 17]);

        assert_eq!(fs.size("x").await.unwrap(), Some(17));
        assert_eq!(fs.size("nope").await.unwrap(), None);
    }

    #[futures_test::test]
    async fn exists_checks_both_layers() {
        let fs = setup();
        fs.add_virtual("x", vec![1]);

        assert!(fs.exists("x").await.unwrap());
        assert!(!fs.exists("y").await.unwrap());

        fs.write("written", 0, &[9]).await.unwrap();
        assert!(fs.exists("written").await.unwrap());
    }

    #[futures_test::test]
    async fn leading_slash_normalized() {
        let fs = setup();
        fs.add_virtual("/a/b", vec![9]);

        assert!(fs.exists("a/b").await.unwrap());
        assert!(fs.exists("/a/b").await.unwrap());
    }

    #[futures_test::test]
    async fn read_past_eof_virtual_returns_some_zero() {
        let fs = setup();
        fs.add_virtual("a", vec![1, 2, 3]);

        let mut buf = [0u8; 4];
        assert_eq!(fs.read("a", 10, 4, &mut buf).await.unwrap(), Some(0));
    }

    #[futures_test::test]
    async fn read_missing_returns_none() {
        let fs = setup();
        let mut buf = [0u8; 4];
        assert_eq!(fs.read("nope", 0, 4, &mut buf).await.unwrap(), None);
    }

    #[futures_test::test]
    async fn platform_write_shadows_virtual() {
        let fs = setup();
        fs.add_virtual("cfg.dat", vec![0xAA, 0xBB, 0xCC]);
        fs.write("cfg.dat", 0, &[1, 2, 3, 4]).await.unwrap();

        let mut buf = [0u8; 4];
        assert_eq!(fs.read("cfg.dat", 0, 4, &mut buf).await.unwrap(), Some(4));
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[futures_test::test]
    async fn first_persistent_write_materializes_virtual_prefix() {
        let fs = setup();
        fs.add_virtual("append.dat", vec![0xAA, 0xBB]);

        assert_eq!(fs.write("append.dat", 2, &[0xCC]).await.unwrap(), 1);

        let mut buf = [0u8; 3];
        assert_eq!(fs.read("append.dat", 0, 3, &mut buf).await.unwrap(), Some(3));
        assert_eq!(buf, [0xAA, 0xBB, 0xCC]);
    }

    #[futures_test::test]
    async fn first_persistent_truncate_materializes_virtual_data() {
        let fs = setup();
        fs.add_virtual("truncate.dat", vec![1, 2, 3, 4]);

        fs.truncate("truncate.dat", 2).await.unwrap();

        let mut buf = [0u8; 2];
        assert_eq!(fs.read("truncate.dat", 0, 2, &mut buf).await.unwrap(), Some(2));
        assert_eq!(buf, [1, 2]);
    }

    #[futures_test::test]
    async fn persistent_backend_exposes_short_writes_and_failed_truncation() {
        let short_write_fs = setup_with_filesystem(StubFilesystem {
            write_limit: Some(2),
            ..Default::default()
        });
        assert_eq!(short_write_fs.write("short.dat", 0, &[1, 2, 3, 4]).await.unwrap(), 2);

        let failed_truncate_fs = setup_with_filesystem(StubFilesystem {
            fail_truncate: true,
            ..Default::default()
        });
        assert_eq!(failed_truncate_fs.write("truncate.dat", 0, &[1, 2, 3, 4]).await.unwrap(), 4);
        assert!(failed_truncate_fs.truncate("truncate.dat", 2).await.is_err());
        assert_eq!(failed_truncate_fs.size("truncate.dat").await.unwrap(), Some(4));
    }
}
