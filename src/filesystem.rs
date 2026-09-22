use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

use directories::ProjectDirs;

use wie_backend::Filesystem;
use wie_util::{Result, WieError};

fn io_error(error: std::io::Error) -> WieError {
    WieError::FatalError(format!("filesystem: {error}"))
}

/// Persistent filesystem backed by `std::fs` under `<base>/<aid>/fs/<path>`.
/// I/O failures are distinct from missing files.
pub struct CliFilesystem {
    base_path: PathBuf,
    mutations: Mutex<()>,
}

impl CliFilesystem {
    pub fn new() -> Self {
        let base_dir = ProjectDirs::from("net", "dlunch", "wie").unwrap();
        Self {
            base_path: base_dir.data_dir().to_owned(),
            mutations: Mutex::new(()),
        }
    }

    fn path_for(&self, aid: &str, path: &str) -> Option<PathBuf> {
        let sanitized_aid: String = aid.chars().filter(|c| !matches!(c, '/' | '\\' | '\0')).collect();
        if sanitized_aid.is_empty() || sanitized_aid == "." || sanitized_aid == ".." {
            tracing::error!(aid, path, "rejected: invalid aid");
            return None;
        }

        let mut normalized = PathBuf::new();
        for component in Path::new(path).components() {
            match component {
                Component::Normal(c) => normalized.push(c),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    tracing::error!(aid, path, "path traversal attempt rejected");
                    return None;
                }
            }
        }

        if normalized.as_os_str().is_empty() {
            tracing::error!(aid, path, "rejected: empty normalized path");
            return None;
        }

        Some(self.base_path.join(&sanitized_aid).join("fs").join(normalized))
    }

    fn writable(&self, aid: &str, path: &str, initial: &[u8]) -> Result<fs::File> {
        let path = self.path_for(aid, path).ok_or_else(|| WieError::FatalError("invalid file path".into()))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        match OpenOptions::new().read(true).write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(initial).map_err(io_error)?;
                Ok(file)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => OpenOptions::new().read(true).write(true).open(path).map_err(io_error),
            Err(error) => Err(io_error(error)),
        }
    }
}

impl Default for CliFilesystem {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Filesystem for CliFilesystem {
    async fn exists(&self, aid: &str, path: &str) -> Result<bool> {
        Ok(self.size(aid, path).await?.is_some())
    }

    async fn size(&self, aid: &str, path: &str) -> Result<Option<usize>> {
        let Some(path) = self.path_for(aid, path) else { return Ok(None) };
        match path.metadata() {
            Ok(metadata) if metadata.is_file() => Ok(Some(
                usize::try_from(metadata.len()).map_err(|_| WieError::FatalError("file too large".into()))?,
            )),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error(error)),
        }
    }

    async fn read(&self, aid: &str, path: &str, offset: usize, count: usize, buf: &mut [u8]) -> Result<Option<usize>> {
        let Some(disk_path) = self.path_for(aid, path) else { return Ok(None) };
        let mut file = match OpenOptions::new().read(true).open(&disk_path) {
            Ok(f) => f,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
        };
        file.seek(SeekFrom::Start(offset as u64)).map_err(io_error)?;
        let count = count.min(buf.len());
        file.read(&mut buf[..count]).map(Some).map_err(io_error)
    }

    async fn write(&self, aid: &str, path: &str, offset: usize, data: &[u8], initial: &[u8]) -> Result<usize> {
        offset
            .checked_add(data.len())
            .ok_or_else(|| WieError::FatalError("file offset overflow".into()))?;
        let _mutation = self
            .mutations
            .lock()
            .map_err(|_| WieError::FatalError("filesystem lock poisoned".into()))?;
        let mut file = self.writable(aid, path, initial)?;
        if offset as u64 > file.metadata().map_err(io_error)?.len() {
            file.set_len(offset as u64).map_err(io_error)?;
        }
        file.seek(SeekFrom::Start(offset as u64)).map_err(io_error)?;
        file.write_all(data).map_err(io_error)?;
        Ok(data.len())
    }

    async fn truncate(&self, aid: &str, path: &str, len: usize, initial: &[u8]) -> Result<()> {
        let _mutation = self
            .mutations
            .lock()
            .map_err(|_| WieError::FatalError("filesystem lock poisoned".into()))?;
        self.writable(aid, path, initial)?.set_len(len as u64).map_err(io_error)
    }
}
