use serde_json::Value;

use crate::error::GraphError;
use crate::storage::file::DatabaseFile;
use crate::storage::page::{RecordId, PAGE_SIZE};
use crate::storage::property::{pages_needed, write_blob_chain, SlottedPage, FREE_PAGE_MARKER};

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
            // `len` comes from on-disk bytes; reject a length that runs past the payload rather
            // than panicking on the slice.
            if payload.len() < 4 {
                return Err(GraphError::StorageCorrupted(0));
            }
            let len = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
            if 4 + len > payload.len() {
                return Err(GraphError::StorageCorrupted(0));
            }
            let s = std::str::from_utf8(&payload[4..4 + len])
                .map_err(|_| GraphError::StorageCorrupted(0))?;
            Ok(Value::String(s.to_string()))
        }
        TAG_ARRAY | TAG_OBJECT => {
            if payload.len() < 4 {
                return Err(GraphError::StorageCorrupted(0));
            }
            let len = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
            if 4 + len > payload.len() {
                return Err(GraphError::StorageCorrupted(0));
            }
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

        write_property_bytes(db, &bytes)
    }

    /// Writes raw bytes to a property page and returns the RecordId.
    pub fn write_raw(db: &mut DatabaseFile, bytes: &[u8]) -> Result<RecordId, GraphError> {
        write_property_bytes(db, bytes)
    }

    /// Reads raw bytes from the database using a RecordId.
    pub fn read_raw(db: &mut DatabaseFile, rid: RecordId) -> Result<Vec<u8>, GraphError> {
        read_property_bytes(db, rid)
    }

    /// Reads a property map from the database using a RecordId.
    pub fn read(
        db: &mut DatabaseFile,
        rid: RecordId,
    ) -> Result<std::collections::HashMap<String, Value>, GraphError> {
        let bytes = read_property_bytes(db, rid)?;

        rmp_serde::from_slice(&bytes).map_err(|e| GraphError::SchemaError(e.to_string()))
    }

    /// Frees the property record at `rid`, making its space reusable by later writes (A-2).
    ///
    /// Dispatches on the RID shape (blob chain vs. slotted), the same way `read` does:
    /// - **Blob chain** (`slot_id == CHAIN_HEAD_SLOT`): every logical page in the chain is returned
    ///   to the property free-page list.
    /// - **Slotted**: the slot is tombstoned; when its page has no live slots left, the whole
    ///   logical page is returned to the free-page list.
    ///
    /// Idempotent (O-5): freeing an already-freed slotted slot is a silent no-op; a chain page
    /// already on the free-list is skipped. Freeing backs off the property free-slot hint so the
    /// reopened space is found by the ordinary scan (the A-1 hand-off, design §3.4).
    pub fn free(db: &mut DatabaseFile, rid: RecordId) -> Result<(), GraphError> {
        free_property_bytes(db, rid)
    }
}

/// Sentinel `slot_id` marking a RID whose `page_id` is the head of a blob page chain.
const CHAIN_HEAD_SLOT: u16 = 0xFFFF;

/// A property page with less than this many free bytes is treated as "effectively full" by the
/// free-page hint: the hint may advance past it, wasting at most this much per page (~1.6% of a
/// 4 KB page). Small enough to keep the waste negligible, large enough that pages the hint stops
/// at can still hold a useful value (design §3.3).
const HINT_FULL_THRESHOLD: usize = 64;

/// Writes `bytes` into the property store and returns its RecordId.
///
/// The RID's `page_id` is a *logical* property page number (resolved through the property page
/// directory), not a physical page id. Small values share a slotted page; values larger than a
/// page are split across a chain whose pages are each mapped into the directory, so the chain is
/// dense in logical-page space and needs no physical contiguity (design §3.4).
fn write_property_bytes(db: &mut DatabaseFile, bytes: &[u8]) -> Result<RecordId, GraphError> {
    if bytes.len() + 6 <= PAGE_SIZE {
        // Try to reuse a slot, scanning from the free-page hint (design §3.3, bounded-waste rule).
        // A page counts as "effectively full" — the hint may advance past it — if it's not a
        // slotted page (blob-chain page, never written again) or has less than HINT_FULL_THRESHOLD
        // free. A page with >= HINT_FULL_THRESHOLD free stops the hint even if this value didn't
        // fit, so small later values can still use its tail (waste bounded to the threshold).
        let logical_count = db.property_page_count();
        let start = db.property_first_free_hint();
        // Advance the hint over the leading run of effectively-full pages we scan past.
        let mut new_hint = start;
        let mut hint_frozen = false;
        for logical in start..logical_count {
            let raw = db.read_property_page(logical)?;
            let mut spage = SlottedPage::from_bytes(raw);
            let usable = spage.is_valid();
            if usable {
                if let Ok(slot) = spage.write_property(bytes) {
                    db.write_property_page(logical, spage.as_bytes())?;
                    if !hint_frozen && new_hint != start {
                        db.set_property_first_free_hint(new_hint)?;
                    }
                    return Ok(RecordId::new(logical as u32, slot.0));
                }
            }
            // Value didn't fit here (or page unusable). If the page still has room for smaller
            // values, freeze the hint at it; otherwise it's effectively full and the hint moves on.
            if !hint_frozen {
                if usable && spage.free_space() >= HINT_FULL_THRESHOLD {
                    hint_frozen = true; // keep new_hint at this page
                } else {
                    new_hint = logical + 1;
                }
            }
        }
        // No space in any scanned page: map a fresh logical property page.
        let mut spage = SlottedPage::new();
        let slot = spage
            .write_property(bytes)
            .map_err(|_| GraphError::StorageCorrupted(0))?;
        let logical = db.alloc_property_page(spage.as_bytes())?;
        // The new page has room, so the hint should point at the first still-open page. Take the
        // minimum of the frozen page (if any) and the allocated page: `alloc_property_page` may have
        // *reused* a freed page whose logical id is below `start` (free-list reuse), and that page
        // now has room — so the hint must back off to it, or the ordinary scan (which only looks at
        // `hint..count`) would never revisit it and would append a fresh page for every later value.
        let candidate = if hint_frozen { new_hint } else { logical };
        let final_hint = candidate.min(logical);
        db.set_property_first_free_hint(final_hint)?;
        Ok(RecordId::new(logical as u32, slot.0))
    } else {
        // Blob chain: each chunk page is mapped into the directory. `write_blob_chain` links
        // pages by relative index within `pages`; we translate those to the assigned logical
        // page numbers so the chain can be followed through the directory on read.
        let needed = pages_needed(bytes.len()).max(1);
        let mut pages = vec![[0u8; PAGE_SIZE]; needed];
        write_blob_chain(&mut pages, bytes)?;
        let mut first_logical = 0usize;
        let mut logicals = Vec::with_capacity(needed);
        for (i, page) in pages.iter().enumerate() {
            let logical = db.alloc_property_page(page)?;
            if i == 0 {
                first_logical = logical;
            }
            logicals.push(logical);
        }
        // Rewrite each page's `next` link from a relative index to the next page's logical
        // number, so reads resolve the chain through the directory.
        relink_chain_to_logical(db, &logicals)?;
        Ok(RecordId::new(first_logical as u32, CHAIN_HEAD_SLOT))
    }
}

/// Reads raw property bytes back for `rid` (slotted page or blob chain).
fn read_property_bytes(db: &mut DatabaseFile, rid: RecordId) -> Result<Vec<u8>, GraphError> {
    if rid.slot_id.0 == CHAIN_HEAD_SLOT {
        // Follow the chain by logical page number via the directory.
        let mut pages = Vec::new();
        let mut logical = rid.page_id.0 as usize;
        let limit = db.property_page_count();
        loop {
            let page = db.read_property_page(logical)?;
            let next = u32::from_le_bytes(page[0..4].try_into().unwrap_or([0xFF; 4]));
            pages.push(page);
            if next == 0xFFFF_FFFF {
                break;
            }
            logical = next as usize;
            if pages.len() > limit {
                return Err(GraphError::StorageCorrupted(logical as u32));
            }
        }
        read_blob_chain_dense(&pages)
    } else {
        let raw = db.read_property_page(rid.page_id.0 as usize)?;
        let spage = SlottedPage::from_bytes(raw);
        Ok(spage.read_property(rid.slot_id)?.to_vec())
    }
}

/// Frees the property record at `rid` (slotted slot or blob chain); see `PropertyStore::free`.
fn free_property_bytes(db: &mut DatabaseFile, rid: RecordId) -> Result<(), GraphError> {
    if rid.slot_id.0 == CHAIN_HEAD_SLOT {
        // Idempotence (O-5): a chain head already on the free-list carries FREE_PAGE_MARKER in its
        // `slot_count` field. Freeing it again must be a no-op, not misread the marker bytes as a
        // `next` link (which would resolve to logical 65535 → StorageCorrupted). This is the
        // documented "a chain page already on the free-list is skipped" behavior (M-1, review-d).
        let head = db.read_property_page(rid.page_id.0 as usize)?;
        if page_is_free_marked(&head) {
            return Ok(());
        }
        // Collect the chain's logical pages first, then free them — so a mid-walk error can't leave
        // a half-freed chain (we only touch the free-list once the whole chain is gathered).
        let mut logicals = Vec::new();
        let mut logical = rid.page_id.0 as usize;
        let limit = db.property_page_count();
        loop {
            let page = db.read_property_page(logical)?;
            let next = u32::from_le_bytes(page[0..4].try_into().unwrap_or([0xFF; 4]));
            logicals.push(logical);
            if next == 0xFFFF_FFFF {
                break;
            }
            logical = next as usize;
            if logicals.len() > limit {
                return Err(GraphError::StorageCorrupted(logical as u32));
            }
        }
        for logical in logicals {
            db.free_property_page(logical)?;
            back_off_property_hint(db, logical)?;
        }
        Ok(())
    } else {
        let logical = rid.page_id.0 as usize;
        let raw = db.read_property_page(logical)?;
        // Idempotence (O-5): if this page is already free-listed (marker), freeing a slot on it
        // would push it onto the free-list a second time, creating a cycle. No-op instead (M-1).
        if page_is_free_marked(&raw) {
            return Ok(());
        }
        let mut spage = SlottedPage::from_bytes(raw);
        spage.free_slot(rid.slot_id)?; // idempotent no-op if already freed (O-5)
        if spage.live_count() == 0 {
            // The page holds no live values — return the whole logical page to the free-list.
            db.free_property_page(logical)?;
        } else {
            db.write_property_page(logical, spage.as_bytes())?;
        }
        back_off_property_hint(db, logical)?;
        Ok(())
    }
}

/// True if `page` is on the property free-page list: its `slot_count` field (`[0..2]`) holds
/// `FREE_PAGE_MARKER`. Used to make freeing idempotent against a stale RID whose page was already
/// reclaimed (M-1). Note the R-1 caveat: a live blob-chain *tail* page's `[0..2]` also equals
/// `0xFFFF` (low half of the `0xFFFF_FFFF` end-sentinel), so this is only ever consulted on a page
/// reached as a *head* (chain head or slotted page), never a chain tail.
fn page_is_free_marked(page: &[u8; PAGE_SIZE]) -> bool {
    u16::from_le_bytes(page[0..2].try_into().unwrap_or([0; 2])) == FREE_PAGE_MARKER
}

/// Backs off the property free-slot hint so a freed page/slot below the current hint is found again
/// by the ordinary forward scan (`hint = min(hint, freed_logical)`). This is the property-side
/// counterpart to `back_off_topology_hint`, the debt A-1 deferred until A-2's free path existed
/// (alloc-hint design §5, §8-1; this design §3.4).
fn back_off_property_hint(db: &mut DatabaseFile, freed_logical: usize) -> Result<(), GraphError> {
    if freed_logical < db.property_first_free_hint() {
        db.set_property_first_free_hint(freed_logical)?;
    }
    Ok(())
}

/// Rewrites the `next` link of each chain page from `write_blob_chain`'s relative index to the
/// next page's logical page number (`0xFFFF_FFFF` stays as the end sentinel).
fn relink_chain_to_logical(db: &mut DatabaseFile, logicals: &[usize]) -> Result<(), GraphError> {
    for (i, &logical) in logicals.iter().enumerate() {
        let mut page = db.read_property_page(logical)?;
        let next = if i + 1 < logicals.len() {
            logicals[i + 1] as u32
        } else {
            0xFFFF_FFFF
        };
        page[0..4].copy_from_slice(&next.to_le_bytes());
        db.write_property_page(logical, &page)?;
    }
    Ok(())
}

/// Reassembles blob bytes from chain pages already collected in chain order.
///
/// The pages are gathered by following logical `next` links, so they are already in order;
/// this only concatenates each page's chunk (unlike `read_blob_chain`, which re-follows a
/// relative-index `next` within the slice).
fn read_blob_chain_dense(pages: &[[u8; PAGE_SIZE]]) -> Result<Vec<u8>, GraphError> {
    const CHAIN_HEADER_SIZE: usize = 8;
    let mut result = Vec::new();
    for (idx, page) in pages.iter().enumerate() {
        let chunk_len = u32::from_le_bytes(page[4..8].try_into().unwrap_or([0; 4])) as usize;
        if CHAIN_HEADER_SIZE + chunk_len > PAGE_SIZE {
            return Err(GraphError::StorageCorrupted(idx as u32));
        }
        result.extend_from_slice(&page[CHAIN_HEADER_SIZE..CHAIN_HEADER_SIZE + chunk_len]);
    }
    Ok(result)
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

    /// A blob larger than one page spans a directory-mapped page chain; it must round-trip and
    /// the RID's `page_id` must be a logical property page number, not a physical pid.
    #[test]
    fn large_blob_spans_chain_and_roundtrips() {
        let (mut db, _path) = make_db();
        // ~3 pages of data forces a multi-page chain.
        let big = "x".repeat(PAGE_SIZE * 3);
        let mut props = HashMap::new();
        props.insert("blob".to_string(), json!(big));
        let rid = PropertyStore::write(&mut db, &props).unwrap();
        assert_eq!(rid.slot_id.0, CHAIN_HEAD_SLOT);
        // The chain consumed several logical property pages.
        assert!(db.property_page_count() >= 3);
        assert_eq!(PropertyStore::read(&mut db, rid).unwrap(), props);
    }

    /// Property RIDs are logical, so appending unrelated physical pages between property writes
    /// must not corrupt earlier properties — the directory resolves each logical page wherever
    /// it physically landed. This is the interleaving the §3.4 logicalization exists to survive.
    #[test]
    fn properties_survive_interleaved_physical_appends() {
        let (mut db, _path) = make_db();
        let mut first = HashMap::new();
        first.insert("a".to_string(), json!("first"));
        let rid1 = PropertyStore::write(&mut db, &first).unwrap();

        // Simulate a topology page being appended to the file tail between property writes.
        db.append_page(&[7u8; PAGE_SIZE]).unwrap();

        let mut second = HashMap::new();
        second.insert("b".to_string(), json!("second"));
        let rid2 = PropertyStore::write(&mut db, &second).unwrap();

        // Another interleaved append, then read both back.
        db.append_page(&[9u8; PAGE_SIZE]).unwrap();

        assert_eq!(PropertyStore::read(&mut db, rid1).unwrap(), first);
        assert_eq!(PropertyStore::read(&mut db, rid2).unwrap(), second);
    }

    /// Large blob + interleaved appends + reopen: the chain must still resolve from the
    /// persisted property-directory metadata in the header.
    #[test]
    fn large_blob_persists_across_reopen_with_interleaving() {
        let (mut db, path) = make_db();
        let big = "y".repeat(PAGE_SIZE * 2 + 100);
        let mut props = HashMap::new();
        props.insert("blob".to_string(), json!(big));
        db.append_page(&[1u8; PAGE_SIZE]).unwrap();
        let rid = PropertyStore::write(&mut db, &props).unwrap();
        db.append_page(&[2u8; PAGE_SIZE]).unwrap();
        db.flush().unwrap();
        drop(db);

        let mut db2 = DatabaseFile::open(&path).unwrap();
        assert_eq!(PropertyStore::read(&mut db2, rid).unwrap(), props);
    }
}
