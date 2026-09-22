use alloc::{boxed::Box, rc::Rc, string::ToString};
use core::cell::RefCell;

use wie_backend::Filesystem;
use wie_util::{Result, WieError};

use crate::indexed_db_store::{Store, StoreKey};

const DB_NAME: &str = "wie_filesystem";
const STORE_NAME: &str = "files";

fn make_key(aid: &str, path: &str) -> StoreKey {
    StoreKey::Pair(aid.to_string(), path.to_string())
}

pub struct WebFilesystem {
    store: Rc<RefCell<Option<Store>>>,
}

// Single-threaded wasm; these JS handles stay in their originating realm.
unsafe impl Send for WebFilesystem {}
unsafe impl Sync for WebFilesystem {}

impl WebFilesystem {
    pub fn new() -> Self {
        Self {
            store: Rc::new(RefCell::new(None)),
        }
    }

    async fn store(&self) -> Result<Store> {
        if let Some(store) = self.store.borrow().as_ref() {
            return Ok(store.clone());
        }
        let store = Store::open(DB_NAME, STORE_NAME).await?;
        *self.store.borrow_mut() = Some(store.clone());
        Ok(store)
    }
}

#[async_trait::async_trait]
impl Filesystem for WebFilesystem {
    async fn exists(&self, aid: &str, path: &str) -> Result<bool> {
        self.store().await?.contains(make_key(aid, path)).await
    }

    async fn size(&self, aid: &str, path: &str) -> Result<Option<usize>> {
        self.store().await?.size(make_key(aid, path)).await
    }

    async fn read(&self, aid: &str, path: &str, offset: usize, count: usize, buf: &mut [u8]) -> Result<Option<usize>> {
        let count = count.min(buf.len());
        let Some(data) = self.store().await?.read(make_key(aid, path), offset, count).await? else {
            return Ok(None);
        };
        if data.len() > count {
            return Err(WieError::FatalError("storage read exceeded output length".into()));
        }
        buf[..data.len()].copy_from_slice(&data);
        Ok(Some(data.len()))
    }

    async fn write(&self, aid: &str, path: &str, offset: usize, data: &[u8], initial: &[u8]) -> Result<usize> {
        self.store().await?.write(make_key(aid, path), offset, data, initial).await
    }

    async fn truncate(&self, aid: &str, path: &str, len: usize, initial: &[u8]) -> Result<()> {
        self.store().await?.truncate(make_key(aid, path), len, initial).await
    }
}
