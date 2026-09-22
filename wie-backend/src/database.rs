use alloc::{boxed::Box, vec::Vec};
use wie_util::Result;

pub type RecordId = u32;

#[async_trait::async_trait]
pub trait Database: Send {
    async fn next_id(&self) -> Result<RecordId>;
    async fn add(&mut self, data: &[u8]) -> Result<RecordId>;
    async fn get(&self, id: RecordId) -> Result<Option<Vec<u8>>>;
    async fn set(&mut self, id: RecordId, data: &[u8]) -> Result<bool>;
    async fn delete(&mut self, id: RecordId) -> Result<bool>;

    async fn get_record_ids(&self) -> Result<Vec<RecordId>>;
}

#[async_trait::async_trait]
pub trait DatabaseRepository {
    async fn open(&self, name: &str, app_id: &str) -> Result<Box<dyn Database>>;
    async fn exists(&self, name: &str, app_id: &str) -> Result<bool>;
    async fn delete(&self, name: &str, app_id: &str) -> Result<bool>;
    /// Returns the bytes occupied by all databases owned by `app_id`.
    async fn usage(&self, app_id: &str) -> Result<u64>;
}
