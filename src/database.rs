use std::{fs, io::Write, path::PathBuf};

use directories::ProjectDirs;

use wie_backend::RecordId;
use wie_util::{Result, WieError};

fn storage_error(error: std::io::Error) -> WieError {
    WieError::FatalError(format!("database: {error}"))
}

pub struct DatabaseRepository {
    base_path: PathBuf,
}

impl DatabaseRepository {
    pub fn new() -> Self {
        let base_dir = ProjectDirs::from("net", "dlunch", "wie").unwrap();

        let base_path = base_dir.data_dir().to_owned();

        Self { base_path }
    }

    fn get_path_for_database(&self, name: &str, app_id: &str) -> PathBuf {
        let sanitized_app_id: String = app_id.chars().filter(|c| !matches!(c, '/' | '\\' | '\0')).collect();
        let app_id = if sanitized_app_id.is_empty() || sanitized_app_id == "." || sanitized_app_id == ".." {
            "_"
        } else {
            &sanitized_app_id
        };

        let name: String = name.chars().map(|c| if matches!(c, '\\' | '\0') { '_' } else { c }).collect();
        let mut normalized_name = PathBuf::new();
        for segment in name.trim_start_matches('/').split('/') {
            match segment {
                "" | "." => {}
                ".." => normalized_name.push("_"),
                segment => normalized_name.push(segment),
            }
        }
        if normalized_name.as_os_str().is_empty() {
            normalized_name.push("_");
        }

        self.base_path.join(app_id).join("db").join(normalized_name)
    }

    fn get_path_for_app_databases(&self, app_id: &str) -> PathBuf {
        self.get_path_for_database("_", app_id).parent().unwrap().to_owned()
    }

    fn directory_usage(path: &std::path::Path) -> Result<u64> {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(storage_error(error)),
        };
        let mut usage: u64 = 0;
        for entry in entries {
            let entry = entry.map_err(storage_error)?;
            let file_type = entry.file_type().map_err(storage_error)?;
            let size = if file_type.is_file() {
                entry.metadata().map_err(storage_error)?.len()
            } else if file_type.is_dir() {
                Self::directory_usage(&entry.path())?
            } else {
                0
            };
            usage = usage
                .checked_add(size)
                .ok_or_else(|| WieError::FatalError("database usage overflow".into()))?;
        }
        Ok(usage)
    }
}

#[async_trait::async_trait]
impl wie_backend::DatabaseRepository for DatabaseRepository {
    async fn open(&self, name: &str, app_id: &str) -> Result<Box<dyn wie_backend::Database>> {
        Ok(Box::new(Database::new(self.get_path_for_database(name, app_id)).map_err(storage_error)?))
    }

    async fn exists(&self, name: &str, app_id: &str) -> Result<bool> {
        self.get_path_for_database(name, app_id).try_exists().map_err(storage_error)
    }

    async fn delete(&self, name: &str, app_id: &str) -> Result<bool> {
        match fs::remove_dir_all(self.get_path_for_database(name, app_id)) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(storage_error(error)),
        }
    }

    async fn usage(&self, app_id: &str) -> Result<u64> {
        Self::directory_usage(&self.get_path_for_app_databases(app_id))
    }
}

pub struct Database {
    base_path: PathBuf,
}

impl Database {
    pub fn new(base_path: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&base_path)?;
        Ok(Self { base_path })
    }

    fn find_empty_record_id(&self) -> Result<RecordId> {
        let mut record_id: RecordId = 1;
        while self.get_path_for_record(record_id).try_exists().map_err(storage_error)? {
            record_id = record_id
                .checked_add(1)
                .ok_or_else(|| WieError::FatalError("record IDs exhausted".into()))?;
        }
        Ok(record_id)
    }

    fn get_path_for_record(&self, id: RecordId) -> PathBuf {
        self.base_path.join(id.to_string())
    }
}

#[async_trait::async_trait]
impl wie_backend::Database for Database {
    async fn next_id(&self) -> Result<RecordId> {
        self.find_empty_record_id()
    }

    async fn add(&mut self, data: &[u8]) -> Result<RecordId> {
        let mut id = self.find_empty_record_id()?;
        loop {
            match fs::OpenOptions::new().write(true).create_new(true).open(self.get_path_for_record(id)) {
                Ok(mut file) => {
                    file.write_all(data).map_err(storage_error)?;
                    return Ok(id);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    id = id.checked_add(1).ok_or_else(|| WieError::FatalError("record IDs exhausted".into()))?;
                }
                Err(error) => return Err(storage_error(error)),
            }
        }
    }

    async fn get(&self, id: RecordId) -> Result<Option<Vec<u8>>> {
        match fs::read(self.get_path_for_record(id)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(storage_error(error)),
        }
    }

    async fn set(&mut self, id: RecordId, data: &[u8]) -> Result<bool> {
        fs::write(self.get_path_for_record(id), data).map_err(storage_error)?;
        Ok(true)
    }

    async fn delete(&mut self, id: RecordId) -> Result<bool> {
        match fs::remove_file(self.get_path_for_record(id)) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(storage_error(error)),
        }
    }

    async fn get_record_ids(&self) -> Result<Vec<RecordId>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.base_path).map_err(storage_error)? {
            let entry = entry.map_err(storage_error)?;
            if entry.file_type().map_err(storage_error)?.is_file() {
                let id = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse().ok())
                    .ok_or_else(|| WieError::FatalError("invalid record filename".into()))?;
                ids.push(id);
            }
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::DatabaseRepository;

    #[test]
    fn database_path_includes_db_segment() {
        let repo = DatabaseRepository {
            base_path: PathBuf::from("/tmp/wie_test"),
        };
        let path = repo.get_path_for_database("records", "game123");
        assert_eq!(path, PathBuf::from("/tmp/wie_test/game123/db/records"));
    }

    #[test]
    fn database_path_strips_guest_leading_slash() {
        let repo = DatabaseRepository {
            base_path: PathBuf::from("/tmp/wie_test"),
        };
        let path = repo.get_path_for_database("/save0.dat", "PD140106");
        assert_eq!(path, PathBuf::from("/tmp/wie_test/PD140106/db/save0.dat"));
    }

    #[test]
    fn database_path_does_not_escape_app_scope() {
        let repo = DatabaseRepository {
            base_path: PathBuf::from("/tmp/wie_test"),
        };
        let path = repo.get_path_for_database("/../save0.dat", "PD140106");
        assert!(path.starts_with(PathBuf::from("/tmp/wie_test/PD140106/db")));
    }
}
