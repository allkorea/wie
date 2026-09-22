extern crate std;

use alloc::{
    collections::{BTreeMap, BTreeSet},
    format,
    string::{String, ToString},
    vec::Vec,
};
use std::io::{Cursor, Read};

use wie_util::{Result, WieError};
use zip::ZipArchive;

const MAX_ENTRIES: usize = 16_384;
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;

fn invalid(message: impl core::fmt::Display) -> WieError {
    WieError::FatalError(format!("Invalid zip archive: {message}"))
}

/// Owns only the ZIP index. Resource names remain unchanged; selected reads
/// return owned bytes and validate their CRC and decompressed length.
pub struct Archive<'a> {
    zip: ZipArchive<Cursor<&'a [u8]>>,
}

impl<'a> Archive<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.len() as u64 > MAX_ENTRY_BYTES {
            return Err(invalid("compressed size limit exceeded"));
        }
        let mut zip = ZipArchive::new(Cursor::new(data)).map_err(invalid)?;
        if zip.len() > MAX_ENTRIES {
            return Err(invalid("entry count limit exceeded"));
        }

        // zip 8.6 indexes duplicate raw names as a single entry. Check the
        // original central headers (PKWARE APPNOTE 4.3.12) before using that index.
        let mut offset = usize::try_from(zip.central_directory_start()).map_err(invalid)?;
        let mut names = BTreeSet::new();
        while data.get(offset..offset.saturating_add(4)) == Some(b"PK\x01\x02") {
            let header = data
                .get(offset..offset.saturating_add(46))
                .ok_or_else(|| invalid("short central header"))?;
            let name_len = u16::from_le_bytes([header[28], header[29]]) as usize;
            let extra_len = u16::from_le_bytes([header[30], header[31]]) as usize;
            let comment_len = u16::from_le_bytes([header[32], header[33]]) as usize;
            let name_start = offset + 46;
            let name = data
                .get(name_start..name_start.saturating_add(name_len))
                .ok_or_else(|| invalid("short entry name"))?;
            if !names.insert(name) {
                return Err(invalid("duplicate entry name"));
            }
            if names.len() > MAX_ENTRIES {
                return Err(invalid("entry count limit exceeded"));
            }
            offset = name_start
                .checked_add(name_len + extra_len + comment_len)
                .filter(|end| *end <= data.len())
                .ok_or_else(|| invalid("short central entry"))?;
        }
        if names.len() != zip.len() {
            return Err(invalid("inconsistent central index"));
        }

        let mut decoded_names = BTreeSet::new();
        let mut total = 0u64;
        for index in 0..zip.len() {
            let file = zip.by_index_raw(index).map_err(invalid)?;
            let name = file.name();
            if name.contains('\0') || name.split(['/', '\\']).any(|part| part == "..") {
                return Err(invalid("unsafe entry path"));
            }
            if !decoded_names.insert(name.to_string()) {
                return Err(invalid("duplicate decoded entry name"));
            }
            total = total.checked_add(file.size()).ok_or_else(|| invalid("size overflow"))?;
            if file.size() > MAX_ENTRY_BYTES || total > MAX_TOTAL_BYTES {
                return Err(invalid("decompressed size limit exceeded"));
            }
        }
        Ok(Self { zip })
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.zip.file_names()
    }

    pub fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(index) = self.names().position(|entry| entry == name) else {
            return Ok(None);
        };
        self.read_index(index)
    }

    fn read_index(&mut self, index: usize) -> Result<Option<Vec<u8>>> {
        let file = self.zip.by_index(index).map_err(invalid)?;
        if !file.is_file() {
            return Ok(None);
        }
        let size = file.size();
        let mut bytes = Vec::new();
        file.take(size + 1).read_to_end(&mut bytes).map_err(invalid)?;
        if bytes.len() as u64 != size {
            return Err(invalid("entry length mismatch"));
        }
        Ok(Some(bytes))
    }

    pub fn extract_matching(&mut self, include: impl Fn(&str) -> bool) -> Result<BTreeMap<String, Vec<u8>>> {
        let names: Vec<(usize, String)> = self
            .names()
            .enumerate()
            .filter(|(_, name)| include(name))
            .map(|(index, name)| (index, name.to_string()))
            .collect();
        let mut files = BTreeMap::new();
        for (index, name) in names {
            if let Some(bytes) = self.read_index(index)? {
                files.insert(name, bytes);
            }
        }
        Ok(files)
    }
}

pub fn extract_zip(data: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    Archive::new(data)?.extract_matching(|_| true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{ZipWriter, write::SimpleFileOptions};

    fn fixture(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored))
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn selected_metadata_does_not_decompress_other_entries() {
        let mut data = fixture(&[("manifest", b"name"), ("payload", b"large")]);
        let payload = data.windows(5).position(|bytes| bytes == b"large").unwrap();
        data[payload] ^= 1;
        let mut archive = Archive::new(&data).unwrap();
        assert_eq!(archive.read("manifest").unwrap(), Some(b"name".to_vec()));
        assert!(archive.read("payload").is_err());
        assert!(extract_zip(&data).is_err());
    }

    #[test]
    fn rejects_duplicates_traversal_truncation_and_oversized_entries() {
        let mut duplicate = fixture(&[("one", b"1"), ("two", b"2")]);
        for index in 0..duplicate.len() - 2 {
            if &duplicate[index..index + 3] == b"two" {
                duplicate[index..index + 3].copy_from_slice(b"one");
            }
        }
        assert!(Archive::new(&duplicate).is_err());
        assert!(Archive::new(&fixture(&[("../escape", b"1")])).is_err());
        let mut data = fixture(&[("file", b"bytes")]);
        assert!(Archive::new(&data[..data.len() - 8]).is_err());
        let central = data.windows(4).position(|bytes| bytes == b"PK\x01\x02").unwrap();
        data[central + 24..central + 28].copy_from_slice(&((MAX_ENTRY_BYTES + 1) as u32).to_le_bytes());
        assert!(Archive::new(&data).is_err());
    }

    #[test]
    fn preserves_guest_paths_and_empty_files() {
        let data = fixture(&[("/res/./data", b""), ("a//b", b"x")]);
        let files = extract_zip(&data).unwrap();
        assert_eq!(files.get("/res/./data"), Some(&Vec::new()));
        assert_eq!(files.get("a//b"), Some(&b"x".to_vec()));
    }
}
