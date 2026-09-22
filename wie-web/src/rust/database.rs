use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    vec::Vec,
};

use wie_backend::RecordId;
use wie_util::{Result, WieError};

use crate::indexed_db_store::{Store, StoreKey};

pub struct DatabaseRepository {}

impl DatabaseRepository {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait::async_trait]
impl wie_backend::DatabaseRepository for DatabaseRepository {
    async fn open(&self, name: &str, app_id: &str) -> Result<Box<dyn wie_backend::Database>> {
        let db_name = format!("wie_{app_id}");
        let store = Store::open(&db_name, &db_name).await?;
        Ok(Box::new(Database {
            store,
            key_prefix: name.to_string(),
        }))
    }

    async fn exists(&self, name: &str, app_id: &str) -> Result<bool> {
        let db_name = format!("wie_{app_id}");
        let store = Store::open(&db_name, &db_name).await?;
        Ok(store.get_all_keys().await?.iter().any(|key| key.starts_with(name)))
    }

    async fn delete(&self, name: &str, app_id: &str) -> Result<bool> {
        let db_name = format!("wie_{app_id}");
        Store::open(&db_name, &db_name).await?.delete_records(name).await
    }

    async fn usage(&self, app_id: &str) -> Result<u64> {
        let db_name = format!("wie_{app_id}");
        Store::open(&db_name, &db_name).await?.usage().await
    }
}

pub struct Database {
    store: Store,
    key_prefix: String,
}

impl Database {
    fn record_key(&self, id: RecordId) -> StoreKey {
        StoreKey::String(format!("{}{}", self.key_prefix, id))
    }
}

#[async_trait::async_trait]
impl wie_backend::Database for Database {
    async fn add(&mut self, data: &[u8]) -> Result<RecordId> {
        self.store.add_record(&self.key_prefix, data).await
    }

    async fn next_id(&self) -> Result<RecordId> {
        let ids = self.get_record_ids().await?;
        ids.iter()
            .max()
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| WieError::FatalError("record IDs exhausted".into()))
    }

    async fn get(&self, id: RecordId) -> Result<Option<Vec<u8>>> {
        self.store.get(self.record_key(id)).await
    }

    async fn set(&mut self, id: RecordId, data: &[u8]) -> Result<bool> {
        self.store.set(self.record_key(id), data).await?;
        Ok(true)
    }

    async fn delete(&mut self, id: RecordId) -> Result<bool> {
        self.store.delete(self.record_key(id)).await?;
        Ok(true)
    }

    async fn get_record_ids(&self) -> Result<Vec<RecordId>> {
        Ok(self
            .store
            .get_all_keys()
            .await?
            .iter()
            .filter_map(|key| key.strip_prefix(self.key_prefix.as_str()).and_then(|tail| tail.parse::<RecordId>().ok()))
            .collect())
    }
}
