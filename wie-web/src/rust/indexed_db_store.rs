use alloc::{borrow::ToOwned, string::String, vec::Vec};

use js_sys::{Array, Uint8Array};
use wasm_bindgen::prelude::*;
use wie_util::Result;

use crate::util::run_js_future;

type JsResult<T> = core::result::Result<T, JsValue>;

#[wasm_bindgen(module = "/src/ts/indexed_db_store.ts")]
extern "C" {
    #[wasm_bindgen(catch, js_name = flushWrites)]
    pub async fn flush_writes() -> JsResult<()>;
    #[wasm_bindgen(js_name = closeCoreStores)]
    pub fn close_core_stores();

    type IndexedDBStore;

    #[wasm_bindgen(static_method_of = IndexedDBStore, catch)]
    async fn open(db_name: &str, store_name: &str) -> JsResult<IndexedDBStore>;
    #[wasm_bindgen(method, catch)]
    async fn get_all_keys(this: &IndexedDBStore) -> JsResult<Array>;
    #[wasm_bindgen(method, catch)]
    async fn get(this: &IndexedDBStore, key: &JsValue) -> JsResult<JsValue>;
    #[wasm_bindgen(method, catch)]
    async fn contains(this: &IndexedDBStore, key: &JsValue) -> JsResult<JsValue>;
    #[wasm_bindgen(method, catch)]
    async fn size(this: &IndexedDBStore, key: &JsValue) -> JsResult<JsValue>;
    #[wasm_bindgen(method, catch)]
    async fn read(this: &IndexedDBStore, key: &JsValue, offset: usize, count: usize) -> JsResult<JsValue>;
    #[wasm_bindgen(method, catch)]
    async fn write(this: &IndexedDBStore, key: &JsValue, offset: usize, data: Uint8Array, initial: Uint8Array) -> JsResult<JsValue>;
    #[wasm_bindgen(method, catch)]
    async fn truncate(this: &IndexedDBStore, key: &JsValue, len: usize, initial: Uint8Array) -> JsResult<()>;
    #[wasm_bindgen(method, catch)]
    async fn set(this: &IndexedDBStore, key: &JsValue, data: Uint8Array) -> JsResult<()>;
    #[wasm_bindgen(method, catch)]
    async fn delete(this: &IndexedDBStore, key: &JsValue) -> JsResult<()>;
    #[wasm_bindgen(method, catch)]
    async fn add_record(this: &IndexedDBStore, prefix: &str, data: Uint8Array) -> JsResult<JsValue>;
    #[wasm_bindgen(method, catch)]
    async fn usage(this: &IndexedDBStore) -> JsResult<JsValue>;
    #[wasm_bindgen(method, catch)]
    async fn delete_records(this: &IndexedDBStore, prefix: &str) -> JsResult<JsValue>;
}

// The web runtime and these handles stay in their originating JS realm.
unsafe impl Sync for IndexedDBStore {}
unsafe impl Send for IndexedDBStore {}

pub struct Store {
    js: IndexedDBStore,
}

#[derive(Clone)]
pub enum StoreKey {
    String(String),
    Pair(String, String),
}

impl StoreKey {
    fn into_js_value(self) -> JsValue {
        match self {
            Self::String(value) => JsValue::from_str(&value),
            Self::Pair(first, second) => Array::of2(&JsValue::from_str(&first), &JsValue::from_str(&second)).into(),
        }
    }
}

impl Clone for Store {
    fn clone(&self) -> Self {
        Self { js: self.js.clone().into() }
    }
}

fn bytes(value: JsValue) -> JsResult<Option<Vec<u8>>> {
    if value.is_undefined() {
        Ok(None)
    } else {
        Ok(Some(
            value
                .dyn_into::<Uint8Array>()
                .map_err(|_| JsValue::from_str("invalid stored bytes"))?
                .to_vec(),
        ))
    }
}

fn unsigned(value: JsValue, max: f64) -> JsResult<u64> {
    let number = value
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0 && *n <= max)
        .ok_or_else(|| JsValue::from_str("invalid storage result"))?;
    Ok(number as u64)
}

impl Store {
    pub async fn add_record(&self, prefix: &str, data: &[u8]) -> Result<u32> {
        let js: IndexedDBStore = self.js.clone().into();
        let prefix = prefix.to_owned();
        let data = Uint8Array::from(data);
        run_js_future(async move { Ok(unsigned(js.add_record(&prefix, data).await?, u32::MAX as f64)? as u32) }).await
    }

    pub async fn usage(&self) -> Result<u64> {
        let js: IndexedDBStore = self.js.clone().into();
        run_js_future(async move { unsigned(js.usage().await?, 9007199254740991.0) }).await
    }

    pub async fn delete_records(&self, prefix: &str) -> Result<bool> {
        let js: IndexedDBStore = self.js.clone().into();
        let prefix = prefix.to_owned();
        run_js_future(async move {
            js.delete_records(&prefix)
                .await?
                .as_bool()
                .ok_or_else(|| JsValue::from_str("invalid delete result"))
        })
        .await
    }

    pub async fn open(db_name: &str, store_name: &str) -> Result<Self> {
        let db_name = db_name.to_owned();
        let store_name = store_name.to_owned();
        let js = run_js_future(async move { IndexedDBStore::open(&db_name, &store_name).await }).await?;
        Ok(Self { js })
    }

    pub async fn get_all_keys(&self) -> Result<Vec<String>> {
        let js: IndexedDBStore = self.js.clone().into();
        run_js_future(async move { Ok(js.get_all_keys().await?.iter().filter_map(|key| key.as_string()).collect()) }).await
    }

    pub async fn get(&self, key: StoreKey) -> Result<Option<Vec<u8>>> {
        let js: IndexedDBStore = self.js.clone().into();
        run_js_future(async move { bytes(js.get(&key.into_js_value()).await?) }).await
    }

    pub async fn contains(&self, key: StoreKey) -> Result<bool> {
        let js: IndexedDBStore = self.js.clone().into();
        run_js_future(async move {
            js.contains(&key.into_js_value())
                .await?
                .as_bool()
                .ok_or_else(|| JsValue::from_str("invalid existence result"))
        })
        .await
    }

    pub async fn size(&self, key: StoreKey) -> Result<Option<usize>> {
        let js: IndexedDBStore = self.js.clone().into();
        run_js_future(async move {
            let value = js.size(&key.into_js_value()).await?;
            if value.is_undefined() {
                return Ok(None);
            }
            let size = value
                .as_f64()
                .filter(|size| size.is_finite() && *size >= 0.0 && size.fract() == 0.0 && *size <= usize::MAX as f64)
                .ok_or_else(|| JsValue::from_str("invalid stored length"))?;
            Ok(Some(size as usize))
        })
        .await
    }

    pub async fn read(&self, key: StoreKey, offset: usize, count: usize) -> Result<Option<Vec<u8>>> {
        let js: IndexedDBStore = self.js.clone().into();
        run_js_future(async move { bytes(js.read(&key.into_js_value(), offset, count).await?) }).await
    }

    pub async fn write(&self, key: StoreKey, offset: usize, data: &[u8], initial: &[u8]) -> Result<usize> {
        let js: IndexedDBStore = self.js.clone().into();
        let data = Uint8Array::from(data);
        let initial = Uint8Array::from(initial);
        run_js_future(async move { Ok(unsigned(js.write(&key.into_js_value(), offset, data, initial).await?, usize::MAX as f64)? as usize) }).await
    }

    pub async fn truncate(&self, key: StoreKey, len: usize, initial: &[u8]) -> Result<()> {
        let js: IndexedDBStore = self.js.clone().into();
        let initial = Uint8Array::from(initial);
        run_js_future(async move { js.truncate(&key.into_js_value(), len, initial).await }).await
    }

    pub async fn set(&self, key: StoreKey, data: &[u8]) -> Result<()> {
        let js: IndexedDBStore = self.js.clone().into();
        let data = Uint8Array::from(data);
        run_js_future(async move { js.set(&key.into_js_value(), data).await }).await
    }

    pub async fn delete(&self, key: StoreKey) -> Result<()> {
        let js: IndexedDBStore = self.js.clone().into();
        run_js_future(async move { js.delete(&key.into_js_value()).await }).await
    }
}
