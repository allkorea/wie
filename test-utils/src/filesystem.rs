use alloc::{
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};
use core::cmp::min;

use hashbrown::HashMap;
use spin::Mutex;

use wie_backend::Filesystem;
use wie_util::{Result, WieError};

/// In-memory `Filesystem` implementation for tests.
#[derive(Default)]
pub struct MemoryFilesystem {
    files: Mutex<HashMap<(String, String), Vec<u8>>>,
}

impl MemoryFilesystem {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl Filesystem for MemoryFilesystem {
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

        let size_to_read = min(count.min(buf.len()), data.len() - offset);
        buf[..size_to_read].copy_from_slice(&data[offset..offset + size_to_read]);
        Ok(Some(size_to_read))
    }

    async fn write(&self, aid: &str, path: &str, offset: usize, data: &[u8], initial: &[u8]) -> Result<usize> {
        let end = offset
            .checked_add(data.len())
            .ok_or_else(|| WieError::FatalError("file offset overflow".into()))?;
        let mut files = self.files.lock();
        let file = files.entry((aid.to_string(), path.to_string())).or_insert_with(|| initial.to_vec());
        if file.len() < end {
            file.resize(end, 0);
        }
        file[offset..end].copy_from_slice(data);

        Ok(data.len())
    }

    async fn truncate(&self, aid: &str, path: &str, len: usize, initial: &[u8]) -> Result<()> {
        let mut files = self.files.lock();
        let file = files.entry((aid.to_string(), path.to_string())).or_insert_with(|| initial.to_vec());
        file.resize(len, 0);
        Ok(())
    }
}
