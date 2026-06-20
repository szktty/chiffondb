use serde_json::Value;

use crate::error::GraphError;
use crate::storage::file::DatabaseFile;
use crate::storage::page::{RecordId, PAGE_SIZE};
use crate::storage::property::{pages_needed, read_blob_chain, write_blob_chain, SlottedPage};

// ---- Value tag definitions ----
const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_INT: u8 = 2;
const TAG_FLOAT: u8 = 3;
const TAG_STRING: u8 = 4;
const TAG_ARRAY: u8 = 5;
const TAG_OBJECT: u8 = 6;

/// Encodes a `serde_json::Value` into a byte sequence.
///
/// Format: [tag: u8] [payload...]
///   - Null:         tag only
///   - Bool:         tag + u8 (0/1)
///   - Int:          tag + i64 LE (when the JSON Number is an integer)
///   - Float:        tag + f64 LE
///   - String:       tag + len:u32 LE + UTF-8 bytes
///   - Array/Object: tag + len:u32 LE + MessagePack bytes
pub fn encode_value(value: &Value) -> Result<Vec<u8>, GraphError> {
    let mut buf = Vec::new();
    match value {
        Value::Null => {
            buf.push(TAG_NULL);
        }
        Value::Bool(b) => {
            buf.push(TAG_BOOL);
            buf.push(*b as u8);
        }
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                buf.push(TAG_INT);
                buf.extend_from_slice(&i.to_le_bytes());
            } else {
                buf.push(TAG_FLOAT);
                buf.extend_from_slice(&n.as_f64().unwrap_or(0.0).to_le_bytes());
            }
        }
        Value::String(s) => {
            buf.push(TAG_STRING);
            let bytes = s.as_bytes();
            buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(bytes);
        }
        Value::Array(_) | Value::Object(_) => {
            let tag = if matches!(value, Value::Array(_)) {
                TAG_ARRAY
            } else {
                TAG_OBJECT
            };
            buf.push(tag);
            let mp =
                rmp_serde::to_vec(value).map_err(|e| GraphError::SchemaError(e.to_string()))?;
            buf.extend_from_slice(&(mp.len() as u32).to_le_bytes());
            buf.extend_from_slice(&mp);
        }
    }
    Ok(buf)
}

/// Decodes a byte sequence into a `serde_json::Value`.
pub fn decode_value(bytes: &[u8]) -> Result<Value, GraphError> {
    if bytes.is_empty() {
        return Err(GraphError::StorageCorrupted(0));
    }
    let tag = bytes[0];
    let payload = &bytes[1..];
    match tag {
        TAG_NULL => Ok(Value::Null),
        TAG_BOOL => {
            if payload.is_empty() {
                return Err(GraphError::StorageCorrupted(0));
            }
            Ok(Value::Bool(payload[0] != 0))
        }
        TAG_INT => {
            if payload.len() < 8 {
                return Err(GraphError::StorageCorrupted(0));
            }
            let i = i64::from_le_bytes(payload[0..8].try_into().unwrap());
            Ok(Value::Number(i.into()))
        }
        TAG_FLOAT => {
            if payload.len() < 8 {
                return Err(GraphError::StorageCorrupted(0));
            }
            let f = f64::from_le_bytes(payload[0..8].try_into().unwrap());
            let n = serde_json::Number::from_f64(f).ok_or(GraphError::StorageCorrupted(0))?;
            Ok(Value::Number(n))
        }
        TAG_STRING => {
            if payload.len() < 4 {
                return Err(GraphError::StorageCorrupted(0));
            }
            let len = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
            let s = std::str::from_utf8(&payload[4..4 + len])
                .map_err(|_| GraphError::StorageCorrupted(0))?;
            Ok(Value::String(s.to_string()))
        }
        TAG_ARRAY | TAG_OBJECT => {
            if payload.len() < 4 {
                return Err(GraphError::StorageCorrupted(0));
            }
            let len = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
            let v: Value = rmp_serde::from_slice(&payload[4..4 + len])
                .map_err(|e| GraphError::SchemaError(e.to_string()))?;
            Ok(v)
        }
        _ => Err(GraphError::StorageCorrupted(0)),
    }
}

// ---- Label list helpers ----

/// Encodes an additional label list (`Vec<u16>`) into MessagePack.
pub fn encode_label_list(type_ids: &[u16]) -> Result<Vec<u8>, GraphError> {
    rmp_serde::to_vec(type_ids).map_err(|e| GraphError::SchemaError(e.to_string()))
}

/// Decodes a MessagePack byte sequence into a `Vec<u16>`.
pub fn decode_label_list(bytes: &[u8]) -> Result<Vec<u16>, GraphError> {
    rmp_serde::from_slice(bytes).map_err(|e| GraphError::SchemaError(e.to_string()))
}

// ---- PropertyStore ----

/// Property store backed by a database file.
/// Writes node/edge properties (`HashMap<String, Value>`) into a single slot.
pub struct PropertyStore;

impl PropertyStore {
    /// Encodes a property map and writes it to the database, returning the RecordId.
    pub fn write(
        db: &mut DatabaseFile,
        props: &std::collections::HashMap<String, Value>,
    ) -> Result<RecordId, GraphError> {
        // Serialize the entire HashMap into MessagePack.
        let bytes = rmp_serde::to_vec(props).map_err(|e| GraphError::SchemaError(e.to_string()))?;

        // Check if it fits in a SlottedPage; if not, use a page chain.
        if bytes.len() + 6 <= PAGE_SIZE {
            // Try to append to an existing property page.
            let page_count = db.page_count()?;
            let prop_start = db.header.property_segment_start;

            for pid in prop_start..page_count {
                let raw = db.read_page(pid)?;
                let mut spage = SlottedPage::from_bytes(raw);
                if !spage.is_valid() {
                    continue;
                }
                if let Ok(slot) = spage.write_property(&bytes) {
                    db.write_page(pid, spage.as_bytes())?;
                    return Ok(RecordId::new(pid, slot.0));
                }
            }

            // No space found: add a new page.
            let mut spage = SlottedPage::new();
            let slot = spage
                .write_property(&bytes)
                .map_err(|_| GraphError::StorageCorrupted(0))?;
            let pid = db.append_page(spage.as_bytes())?;
            Ok(RecordId::new(pid, slot.0))
        } else {
            // Larger than 4 KB: write using a page chain.
            let needed = pages_needed(bytes.len()).max(1);
            let mut pages = vec![[0u8; PAGE_SIZE]; needed];
            write_blob_chain(&mut pages, &bytes)?;
            let first_pid = db.append_page(&pages[0])?;
            for page in pages.iter().skip(1) {
                db.append_page(page)?;
            }
            // SlotId=0xFFFF is a sentinel indicating the head of a chain.
            Ok(RecordId::new(first_pid, 0xFFFF))
        }
    }

    /// Writes raw bytes to a property page and returns the RecordId.
    pub fn write_raw(db: &mut DatabaseFile, bytes: &[u8]) -> Result<RecordId, GraphError> {
        if bytes.len() + 6 <= PAGE_SIZE {
            let page_count = db.page_count()?;
            let prop_start = db.header.property_segment_start;

            for pid in prop_start..page_count {
                let raw = db.read_page(pid)?;
                let mut spage = SlottedPage::from_bytes(raw);
                if !spage.is_valid() {
                    continue;
                }
                if let Ok(slot) = spage.write_property(bytes) {
                    db.write_page(pid, spage.as_bytes())?;
                    return Ok(RecordId::new(pid, slot.0));
                }
            }

            let mut spage = SlottedPage::new();
            let slot = spage
                .write_property(bytes)
                .map_err(|_| GraphError::StorageCorrupted(0))?;
            let pid = db.append_page(spage.as_bytes())?;
            Ok(RecordId::new(pid, slot.0))
        } else {
            let needed = pages_needed(bytes.len()).max(1);
            let mut pages = vec![[0u8; PAGE_SIZE]; needed];
            write_blob_chain(&mut pages, bytes)?;
            let first_pid = db.append_page(&pages[0])?;
            for page in pages.iter().skip(1) {
                db.append_page(page)?;
            }
            Ok(RecordId::new(first_pid, 0xFFFF))
        }
    }

    /// Reads raw bytes from the database using a RecordId.
    pub fn read_raw(db: &mut DatabaseFile, rid: RecordId) -> Result<Vec<u8>, GraphError> {
        if rid.slot_id.0 == 0xFFFF {
            let page_count = db.page_count()?;
            let first_pid = rid.page_id.0;
            let mut pages = Vec::new();
            let mut pid = first_pid;
            loop {
                let page = db.read_page(pid)?;
                let next = u32::from_le_bytes(page[0..4].try_into().unwrap());
                pages.push(page);
                if next == 0xFFFF_FFFF {
                    break;
                }
                pid = first_pid + next;
                if pages.len() > page_count as usize {
                    return Err(GraphError::StorageCorrupted(pid));
                }
            }
            read_blob_chain(&pages)
        } else {
            let raw = db.read_page(rid.page_id.0)?;
            let spage = SlottedPage::from_bytes(raw);
            Ok(spage.read_property(rid.slot_id)?.to_vec())
        }
    }

    /// Reads a property map from the database using a RecordId.
    pub fn read(
        db: &mut DatabaseFile,
        rid: RecordId,
    ) -> Result<std::collections::HashMap<String, Value>, GraphError> {
        let bytes = if rid.slot_id.0 == 0xFFFF {
            // Page chain
            let page_count = db.page_count()?;
            let first_pid = rid.page_id.0;
            let mut pages = Vec::new();
            let mut pid = first_pid;
            loop {
                let page = db.read_page(pid)?;
                let next = u32::from_le_bytes(page[0..4].try_into().unwrap());
                pages.push(page);
                if next == 0xFFFF_FFFF {
                    break;
                }
                pid = first_pid + next;
                if pages.len() > page_count as usize {
                    return Err(GraphError::StorageCorrupted(pid));
                }
            }
            read_blob_chain(&pages)?
        } else {
            // SlottedPage
            let raw = db.read_page(rid.page_id.0)?;
            let spage = SlottedPage::from_bytes(raw);
            spage.read_property(rid.slot_id)?.to_vec()
        };

        rmp_serde::from_slice(&bytes).map_err(|e| GraphError::SchemaError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::file::DatabaseFile;
    use proptest::prelude::*;
    use serde_json::json;
    use std::collections::HashMap;
    use tempfile::NamedTempFile;

    // ---- encode/decode tests ----

    #[test]
    fn roundtrip_null() {
        let v = Value::Null;
        assert_eq!(decode_value(&encode_value(&v).unwrap()).unwrap(), v);
    }

    #[test]
    fn roundtrip_bool() {
        for b in [true, false] {
            let v = Value::Bool(b);
            assert_eq!(decode_value(&encode_value(&v).unwrap()).unwrap(), v);
        }
    }

    #[test]
    fn roundtrip_int() {
        for i in [0i64, 1, -1, i64::MAX, i64::MIN] {
            let v = json!(i);
            assert_eq!(decode_value(&encode_value(&v).unwrap()).unwrap(), v);
        }
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn roundtrip_float() {
        for f in [0.0f64, 1.5, -3.14] {
            let v = json!(f);
            let encoded = encode_value(&v).unwrap();
            let decoded = decode_value(&encoded).unwrap();
            assert_eq!(decoded.as_f64().unwrap(), v.as_f64().unwrap());
        }
    }

    #[test]
    fn roundtrip_string() {
        let v = json!("hello, chiffondb!");
        assert_eq!(decode_value(&encode_value(&v).unwrap()).unwrap(), v);
    }

    #[test]
    fn roundtrip_array() {
        let v = json!([1, "two", true, null]);
        assert_eq!(decode_value(&encode_value(&v).unwrap()).unwrap(), v);
    }

    #[test]
    fn roundtrip_object() {
        let v = json!({"key": "value", "num": 42});
        assert_eq!(decode_value(&encode_value(&v).unwrap()).unwrap(), v);
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(50))]
        #[test]
        fn random_value_roundtrip(
            s in ".*",
            i in any::<i64>(),
            b in any::<bool>(),
            choice in 0usize..4,
        ) {
            let v = match choice {
                0 => Value::Null,
                1 => Value::Bool(b),
                2 => json!(i),
                _ => Value::String(s),
            };
            let encoded = encode_value(&v).unwrap();
            let decoded = decode_value(&encoded).unwrap();
            // Null/Bool/Int/String must match exactly.
            prop_assert_eq!(decoded, v);
        }
    }

    // ---- PropertyStore tests ----

    fn make_db() -> (DatabaseFile, std::path::PathBuf) {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.into_temp_path().to_path_buf();
        std::fs::remove_file(&path).ok();
        let db = DatabaseFile::create(&path).unwrap();
        (db, path)
    }

    #[test]
    fn property_store_write_and_read() {
        let (mut db, _path) = make_db();
        let mut props = HashMap::new();
        props.insert("id".to_string(), json!("user_1"));
        props.insert("name".to_string(), json!("Alice"));
        props.insert("age".to_string(), json!(30));
        props.insert("active".to_string(), json!(true));

        let rid = PropertyStore::write(&mut db, &props).unwrap();
        let loaded = PropertyStore::read(&mut db, rid).unwrap();
        assert_eq!(props, loaded);
    }

    #[test]
    fn property_store_multiple_entries() {
        let (mut db, _path) = make_db();
        let mut rids = Vec::new();
        let entries: Vec<HashMap<String, Value>> = (0..10)
            .map(|i| {
                let mut m = HashMap::new();
                m.insert("id".to_string(), json!(i.to_string()));
                m.insert("score".to_string(), json!(i * 10));
                m
            })
            .collect();

        for props in &entries {
            rids.push(PropertyStore::write(&mut db, props).unwrap());
        }
        for (rid, expected) in rids.iter().zip(entries.iter()) {
            let loaded = PropertyStore::read(&mut db, *rid).unwrap();
            assert_eq!(&loaded, expected);
        }
    }

    #[test]
    fn property_store_persist_across_reopen() {
        let (mut db, path) = make_db();
        let mut props = HashMap::new();
        props.insert("key".to_string(), json!("value"));
        let rid = PropertyStore::write(&mut db, &props).unwrap();
        db.flush().unwrap();
        drop(db);

        let mut db2 = DatabaseFile::open(&path).unwrap();
        let loaded = PropertyStore::read(&mut db2, rid).unwrap();
        assert_eq!(props, loaded);
    }
}
