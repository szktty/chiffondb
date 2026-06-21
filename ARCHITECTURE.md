# ChiffonDB Architecture

## File layout

All data is stored in a single `.chiffon` file. The file is treated as a sequence of **fixed-size 4096-byte pages**.

```
Offset 0     : Page 0  (header page)
Offset 4096  : Page 1
Offset 8192  : Page 2
...
```

Internally the file is logically divided into three segments. The starting page number of each segment is recorded in the header page.

| Segment | Contents | Record format |
|---------|----------|---------------|
| Topology | Connectivity of nodes and edges | Fixed-size slots |
| Property | Variable-length data (strings, blobs, etc.) | Slotted page |
| Vector | Embedding vectors (implemented after MVP) | Fixed-size blocks |

---

## Header page (Page 0)

| Field | Size | Contents |
|-------|------|----------|
| magic | 8 bytes | `CHIFFON\0` |
| version | 4 bytes | Format version (u32) |
| page_size | 4 bytes | Page size (fixed at 4096) |
| topology_segment_start | 4 bytes | Starting page number of the topology segment |
| property_segment_start | 4 bytes | Starting page number of the property segment |
| vector_segment_start | 4 bytes | Starting page number of the vector segment (0 = unused) |
| page_directory_root | 4 bytes | Page number of the latest page directory (for MVCC) |
| schema_root | 4 bytes | Page number where schema information is stored |
| reserved | remainder | Reserved for future use (zero-filled) |

---

## RecordId (logical pointer)

References between nodes and edges are all represented by a **RecordId** (a logical pointer). Raw physical addresses are never used.

```rust
pub struct RecordId {
    pub page_id: PageId,   // Page number (u32)
    pub slot_id: SlotId,   // Slot number within the page (u16)
}
// 6 bytes total
```

An optional reference (`Option<RecordId>`) is represented as 7 bytes when serialized. `None` is indicated by a sentinel value of all `0xFF` bytes.

---

## Topology pages

### Page structure (fixed-size slots)

A topology page starts with a bitmap (for free-slot management), followed by fixed-size slot data.

```
[bitmap] [slot 0] [slot 1] ... [slot N]
```

- 1 bit = 1 slot (0 = free, 1 = in use)
- Both NodeRecord and EdgeRecord use a **fixed slot size of 64 bytes**
- A 4096-byte page holds at most 63 of the 64-byte slots

### NodeRecord (64 bytes)

Source: `chiffondb-core/src/storage/record.rs`

```
[0..6]   id            This record's own RecordId (6 bytes)
[6..8]   node_type_id  Type ID in the schema (u16)
[8..15]  first_out_edge  Head of the out-edge adjacency list (Option<RecordId>, 7 bytes)
[15..22] first_in_edge   Head of the in-edge adjacency list (Option<RecordId>, 7 bytes)
[22..29] property_ref    Reference to a property page (Option<RecordId>, 7 bytes)
[29..36] label_ref       Reference to the additional-label list (Option<RecordId>, 7 bytes)
[36]     flags           Bit flags (deleted, etc.) (u8)
[37..64] reserved        Reserved for future use
```

### EdgeRecord (64 bytes)

Source: `chiffondb-core/src/storage/record.rs`

```
[0..6]   id            This record's own RecordId (6 bytes)
[6..8]   edge_type_id  Edge type ID in the schema (u16)
[8..14]  from_node     RecordId of the source node (6 bytes)
[14..20] to_node       RecordId of the destination node (6 bytes)
[20..27] next_out_edge Next out-edge of the source node (Option<RecordId>, 7 bytes)
[27..34] next_in_edge  Next in-edge of the destination node (Option<RecordId>, 7 bytes)
[34..41] property_ref  Reference to a property page (Option<RecordId>, 7 bytes)
[41]     flags         Bit flags (u8)
[42..49] label_ref     Reference to the additional-label list (Option<RecordId>, 7 bytes)
[49..64] reserved      Reserved for future use
```

### Adjacency lists (topology traversal)

Nodes and edges form **doubly linked lists**.

```
Node.first_out_edge ──► Edge ──► Edge.next_out_edge ──► Edge ──► None
Node.first_in_edge  ──► Edge ──► Edge.next_in_edge  ──► Edge ──► None
```

- When adding an edge, it is **inserted at the head** of the list (O(1))
- When deleting an edge, the previous and next links are rewired to bypass it
- Traversal just follows the pointer chain, requiring no index (index-free adjacency traversal)

Source: `chiffondb-core/src/storage/topology.rs`

---

## Property pages (slotted page)

Pages that store variable-length data such as strings, JSON, and lists.

```
[0..2]       slot_count (u16)
[2..2+N*4]   Slot directory (4 bytes each: offset u16 + length u16)
   ...free space...
[from the end]   Data area (filled from the back)
```

- The directory grows from the front, and data fills from the back
- The page is full when the two collide
- Referenced via the RecordId pointed to by a NodeRecord / EdgeRecord `property_ref`

Source: `chiffondb-core/src/storage/property.rs`

### Blobs larger than 4 KB (page chains)

When a blob exceeds 4 KB, multiple pages are linked together in a chain.

```
[0..4]  next_page_id (u32, 0xFFFFFFFF = end)
[4..8]  chunk_length (u32)
[8..]   Data (up to 4088 bytes)
```

The property page slot records the "first page ID + total size", while the actual data is stored in the chain.

### Additional labels (multiple labels)

The slot of the property page pointed to by `label_ref` stores an array of additional type IDs (`Vec<u16>`) serialized with MessagePack. When there are no additional labels, `label_ref` is `None` (the sentinel value).

---

## Durability and concurrency (current model)

Durability is provided by the WAL, not by copy-on-write. Every page write is appended to
the WAL file (write-through); on `flush()` the WAL is checkpointed into the main file. A
crash before checkpoint is recovered by replaying the WAL on next open. See the "Bounded
page cache" section below for the read/write/rollback paths.

Concurrency is single-writer, single-reader per database file, enforced by an exclusive
advisory lock plus an in-process open-file registry (`storage::file`): a second attempt to
open the same file fails. There is no shared multi-reader access.

> **Not yet implemented (planned).** True copy-on-write and MVCC — a per-transaction page
> directory (logical→physical page mapping), snapshot isolation with non-blocking readers
> (SWMR), and incremental backups derived from appended pages — are design goals, not the
> current behavior. The `page_directory_root` header field is reserved (always 0) for this
> future work.
```

## Bounded page cache

The file backend routes its page I/O through a single bounded LRU page cache (`storage::buffer::PageCache`), modeled on SQLite's pcache. Its capacity is `OpenOptions::max_memory_bytes / PAGE_SIZE` (default 4 MiB ≈ 1024 pages).

> **Scope.** All on-disk data flows through `read_page`/`write_page`: property (blob) pages and topology (node/edge record) pages alike. `TopologyStore` holds only O(1) metadata (segment start + logical page counts) and faults record pages via `DatabaseFile`, and there is no in-memory property index — lookups (`find` / `find_by_type`) scan the topology through the same bounded cache. As a result the whole engine's resident memory is bounded by the cache budget regardless of database size.
>
> **Known limit.** The topology segment is a fixed range `[topology_segment_start, property_segment_start)` (pages 1–63 by default), so a database holds at most ~2000 nodes before allocation returns `CapacityExceeded`. A variable-length topology segment is future work.

- **Read path**: cache → WAL file → main `.chiffon` file. The first hit is returned and then inserted into the cache (evicting the LRU page if full).
- **Write path**: every write is appended to the WAL file immediately (write-through) and the page is cached. Because the durable copy already lives in the WAL, a cached page can be evicted at any time without data loss; the WAL keeps only a small `page_id → byte-offset` index in memory rather than the page bytes.
- **Checkpoint** (`flush`): WAL entries are written back to the main file and the WAL is cleared. Cached pages remain valid (they equal the checkpointed contents).
- **Rollback**: the cache is cleared and the WAL file is truncated back to the length captured at `begin()`, so post-snapshot writes vanish from both memory and the durable log.
