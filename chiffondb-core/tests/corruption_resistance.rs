//! Corruption-resistance regression tests (promoted from the 2026-07-10 review probe for M-2).
//!
//! A `.chiffon` file is a trust boundary (it may come from elsewhere). Crafted page/value bytes
//! must surface as a `GraphError`, never a panic (slice out-of-bounds) or an infinite loop.

use chiffondb_core::storage::page::{SlotId, PAGE_SIZE};
use chiffondb_core::storage::property::SlottedPage;
use chiffondb_core::storage::value::decode_value;

/// Slot directory entry with offset+length far beyond PAGE_SIZE.
#[test]
fn corrupted_slot_entry_read_does_not_panic() {
    let mut bytes = [0u8; PAGE_SIZE];
    bytes[0..2].copy_from_slice(&1u16.to_le_bytes()); // slot_count = 1
    bytes[2..4].copy_from_slice(&0xFFF0u16.to_le_bytes()); // offset = 65520
    bytes[4..6].copy_from_slice(&0xFFF0u16.to_le_bytes()); // length = 65520
    let page = SlottedPage::from_bytes(bytes);
    let result = std::panic::catch_unwind(|| page.read_property(SlotId(0)));
    assert!(
        matches!(result, Ok(Err(_))),
        "corrupted slot entry must be a GraphError, not a panic: {result:?}"
    );
}

/// String tag whose declared length exceeds the actual payload.
#[test]
fn decode_value_truncated_string_does_not_panic() {
    // TAG_STRING(4) + len=1000 + only 3 bytes of payload
    let mut bytes = vec![4u8];
    bytes.extend_from_slice(&1000u32.to_le_bytes());
    bytes.extend_from_slice(b"abc");
    let result = std::panic::catch_unwind(|| decode_value(&bytes));
    assert!(
        matches!(result, Ok(Err(_))),
        "truncated string must be a GraphError, not a panic: {result:?}"
    );
}

/// Object tag with over-long declared length.
#[test]
fn decode_value_truncated_object_does_not_panic() {
    let mut bytes = vec![6u8];
    bytes.extend_from_slice(&5000u32.to_le_bytes());
    bytes.extend_from_slice(&[0x80]); // empty msgpack map, but len says 5000
    let result = std::panic::catch_unwind(|| decode_value(&bytes));
    assert!(
        matches!(result, Ok(Err(_))),
        "truncated object must be a GraphError, not a panic: {result:?}"
    );
}

/// Label-index chain page with a corrupted count (larger than fits in a page):
/// reading entries must not slice past the page.
#[test]
fn label_index_corrupted_count_does_not_panic() {
    use chiffondb_core::storage::file::DatabaseFile;
    use chiffondb_core::storage::label_index::LabelIndex;
    use chiffondb_core::storage::page::RecordId;

    let mut f = DatabaseFile::create_in_memory().unwrap();
    {
        let mut idx = LabelIndex::new(&mut f);
        idx.add(1, RecordId::new(0, 0)).unwrap();
    }
    // Corrupt every appended page's count field to the maximum u32.
    let total = f.page_count().unwrap();
    for pid in 1..total {
        let mut page = f.read_page(pid).unwrap();
        page[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        f.write_page(pid, &page).unwrap();
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut idx = LabelIndex::new(&mut f);
        idx.get(1)
    }));
    assert!(
        matches!(result, Ok(Err(_)) | Ok(Ok(_))),
        "corrupted count must not panic: {result:?}"
    );
}
