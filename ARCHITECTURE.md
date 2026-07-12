# ChiffonDB Architecture

## File layout

All data is stored in a single `.chiffon` file. The file is treated as a sequence of **fixed-size 4096-byte pages**.

```
Offset 0     : Page 0  (header page)
Offset 4096  : Page 1
Offset 8192  : Page 2
...
```

Pages are **not** partitioned into fixed physical segments. Node, edge, and property record pages
(and their directory/index pages) are all appended to the end of the file and located through
**page directories** — one per kind, whose root is stored in the header. A logical page number is
resolved to a physical page id through its directory, so every kind can grow independently on the
shared append tail without a fixed capacity. (This replaced the original fixed `[topology][property]`
segment layout, which capped a database at ~2000 nodes; see the version history below.)

| Kind | Contents | Record format | Located via |
|------|----------|---------------|-------------|
| Node | Node records + adjacency-list heads | Fixed 64-byte slots | node page directory |
| Edge | Edge records + adjacency-list links | Fixed 64-byte slots | edge page directory |
| Property | Variable-length values (strings, JSON, blobs, labels) | Slotted page / page chain | property page directory |
| Label index | `type_id → {node RecordId}` (tier 1) | Per-type page chain | `label_index_root` |
| Property index | `(type, property) value → {node RecordId}` (tier 2/3) | Hash-bucket page chains | `property_index_root` |
| Schema | Schema DSL + type-id assignments | Page chain | `schema_root` |

Embedding vectors (a "vector" kind) are reserved for future work; there is no vector write path yet.

---

## Header page (Page 0)

The on-disk `version` is `8`. `FileHeader::deserialize` rejects any other version rather than
misreading an older layout (there is no backward compatibility — see the version history).

| Offset | Field | Size | Contents |
|--------|-------|------|----------|
| 0 | magic | 8 | `CHIFFON\0` |
| 8 | version | 4 | Format version (u32), currently 9 |
| 12 | page_size | 4 | Page size (fixed at 4096) |
| 16 | label_index_root | 4 | Root page of the tier-1 label index (`0xFFFFFFFF` = none) |
| 20 | property_index_root | 4 | Root page of the tier-2/3 property index (`0xFFFFFFFF` = none) |
| 24 | *(reserved)* | 8 | Zero-filled |
| 32 | schema_root | 4 | First page of the schema page chain (0 = none) |
| 36 | schema_version | 4 | Bumped on each schema change |
| 40 | node_page_count | 4 | Logical node pages in use (= node directory's mapped length) |
| 44 | edge_page_count | 4 | Logical edge pages in use (= edge directory's mapped length) |
| 48 | property_dir_root | 4 | Root of the property page directory (`0xFFFFFFFF` = none) |
| 52 | property_dir_len | 4 | Mapped length of the property page directory |
| 56 | node_dir_root | 4 | Root of the node page directory (`0xFFFFFFFF` = none) |
| 60 | edge_dir_root | 4 | Root of the edge page directory (`0xFFFFFFFF` = none) |
| 64 | node_first_free_page | 4 | Free-slot search hint for node pages (0 = from start) |
| 68 | edge_first_free_page | 4 | Free-slot search hint for edge pages |
| 72 | property_first_free_page | 4 | Free-slot search hint for property pages |
| 76 | property_free_list_head | 4 | Head of the property free-page list (`0xFFFFFFFF` = empty) |
| 80.. | *(reserved)* | remainder | Zero-filled |

The header is the single in-memory source of truth for all directory/index roots and page counts.
It is updated **through the WAL** like any other page, and reverts in full on rollback (so the
whole header is treated as transaction state and moves in lockstep with the data).

### On-disk format version history

| Version | Change |
|---------|--------|
| 3 | Product rename to ChiffonDB (magic `TANABATA` → `CHIFFON\0`). |
| 4 | Variable-length topology: property page directory; property RIDs become logical page numbers. |
| 5 | Removed the vestigial segment-start fields and the unused MVCC `page_directory_root`. |
| 6 | Tier-1 label index (root at offset 16). |
| 7 | Tier-2 property index (root at offset 20). |
| 8 | Tier-2 index hash changed from `DefaultHasher` (not stable across Rust releases) to a fixed FNV-1a, invalidating persisted index entries. |
| 9 | Added per-kind free-slot search hints (offsets 64..76), making insertion amortized O(1). |
| 10 | Property space reclaim (A-2): property free-page list head at offset 76; slotted-page header grows a `live_count` field and gains slot tombstones — old v9 property pages are rejected. |

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
- **Adjacency traversal** just follows the pointer chain, requiring no index (index-free). This is
  separate from the label/property indexes below, which accelerate *lookups by type or property
  value*, not neighbor traversal.

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

## Indexes

Both index tiers are persisted on disk (pages resolved through the same bounded page cache) and
carry **no in-memory state**, so their mutations ride the WAL and roll back together with the data.
Every chain walk is bounded by the file's page count, so a corrupt `next` link errors as
`StorageCorrupted` rather than looping.

### Tier 1: label (type) index — always on

Maps `type_id → {node RecordId}` so `list_nodes(type)` / `count_nodes(type)` are O(matches)
instead of a full scan. A node is registered under **every** label it carries (primary
`node_type_id` plus each additional/dynamic label), so `MATCH (n:Label)` hits under any of a node's
labels. Structure: a per-type `RecordId` page chain, with a small `type_id → chain head` root map.
Root at header offset 16. Source: `chiffondb-core/src/storage/label_index.rs`.

### Tier 2: property index — declared with `@index`

Accelerates equality `find` / `find_all` on a `(type, property)` pair declared `@index` in the
schema. The key is a `PropertyPath` (a flat field name or a nested-scalar path such as
`profile.city`); only scalar values are indexed (arrays / objects / absent values are excluded —
a **partial index**). It is an **equality hash-bucket** structure, not a B+tree: an entry stores a
fixed 24 bytes `[type_id][path_hash][value_hash][rid]`, hashed into 256 buckets. Because the value
is stored as a hash, a lookup returns **candidates** that the query layer confirms against each
node's real value (so hash collisions never produce wrong results). Range / ordered queries are not
accelerated. Hashing uses a fixed **FNV-1a** (not `std`'s release-unstable `DefaultHasher`) so
persisted entries survive toolchain updates. Root at header offset 20. Source:
`chiffondb-core/src/storage/property_index.rs`.

### Tier 3: unique constraint — declared with `@unique`

A `@unique` field is maintained in the tier-2 index (so it is also queryable) and additionally
**enforced**: `insert` / `update` check the index before writing and return `UniqueViolation` if a
*different* node already holds the value (updating a node to its own value is allowed). The check
runs pre-write, so a rejected operation leaves the database unchanged. Non-scalar / absent values
are unconstrained (partial semantics). Enforcement is on the node's **primary** type's `@unique`
fields.

Tier-4 (full-text / vector) is future work.

---

## Durability and concurrency (current model)

Durability is provided by the WAL, not by copy-on-write. Every page write is appended to
the WAL file (write-through); on `flush()` the WAL is checkpointed into the main file. A
crash before checkpoint is recovered by replaying the WAL on next open — `open` checkpoints the
WAL **before** reading the header, so a header updated only in the WAL (a crash/drop before flush)
is applied rather than lost. See the "Bounded page cache" section below for the read/write/rollback
paths.

Concurrency is single-writer, single-reader per database file, enforced by an exclusive
advisory lock plus an in-process open-file registry (`storage::file`): a second attempt to
open the same file fails. There is no shared multi-reader access.

> **Crash atomicity is not guaranteed at the API-call level.** The WAL is write-through and has
> **no commit boundary**: it records page writes, not transaction boundaries. In-process rollback
> (`take_snapshot` / `restore_snapshot`, which truncates the WAL to the snapshot length) is exact,
> but a crash *mid-operation* — e.g. partway through a single `insert_node` that writes several
> pages — replays whatever reached the WAL, so a partially-applied operation can survive recovery.
> Treat durability as "flushed state, plus a best-effort tail", not as all-or-nothing per call.
> A WAL commit marker (replay only up to the last committed record) is future work.

> **Not yet implemented (planned).** True copy-on-write and MVCC — snapshot isolation with
> non-blocking readers (SWMR) and incremental backups derived from appended pages — are design
> goals, not the current behavior. The per-kind page directories introduced for variable-length
> topology are the substrate a future CoW would swap roots on.

## Bounded page cache

The file backend routes its page I/O through a single bounded LRU page cache (`storage::buffer::PageCache`), modeled on SQLite's pcache. Its capacity is `OpenOptions::max_memory_bytes / PAGE_SIZE` (default 4 MiB ≈ 1024 pages).

> **Scope.** All on-disk data flows through `read_page`/`write_page`: node/edge record pages,
> property (blob) pages, and the directory/index pages alike. `TopologyStore` holds only O(1)
> metadata and faults record pages via `DatabaseFile`; the indexes are on-disk with no resident
> per-node state. As a result the whole engine's resident memory is bounded by the cache budget
> regardless of database size — with one caveat below.
>
> **Node capacity.** There is no longer a fixed ~2000-node cap: node/edge/property pages grow on
> the append tail, resolved through per-kind page directories. The ceiling is now the u32 logical
> page space (billions of nodes), bounded in practice by disk.
>
> **Insertion cost.** The slot allocators no longer scan from page 0 on every insert. A per-kind
> free-slot search hint in the header (offsets 64..76) starts the scan at the first page that may
> have room, so contiguous inserts are amortized O(1). `free`/`delete` walks the hint back so freed
> slots are reused (for property, the hint also backs off when the free-page list reuses a low
> logical page). For fixed-size node/edge pages this is exact; for variable-size property pages
> the hint advances past pages with less than 64 bytes free (bounded waste), and a delete-heavy
> adversarial pattern can still cost O(pages) per op (single-hint limitation, not a free-list).
>
> **Property space reclaim (A-2).** `update` / `delete` on a node or edge now frees the property
> space it held. Freed property pages go on an intrusive free-page list (head at header offset 76;
> each free page carries a `FREE_PAGE_MARKER` in its `slot_count` field and links to the next free
> page), and `alloc_property_page` pops that list before appending — so deleted space is reused, not
> leaked. Slotted values are freed by tombstoning the slot (offset `0xFFFF`, `live_count` decremented);
> a page returns to the free-list once its `live_count` hits 0. Reclaim is **page-granular**: a
> partly-live page keeps its dead slots' bytes until it fully empties (intra-page compaction is a
> deferred follow-up, A-2b). Reading a freed slot returns `PropertySlotFreed`, distinct from
> `StorageCorrupted`.
>
> **Known limitations (not addressed on this branch).**
> - **Property reclaim is page-granular, not compacting** (A-2b): a page with one immortal live slot
>   holds its other freed slots' bytes until that slot is freed too.
> - **Schema-chain space is still not reclaimed in place.** Schema-chain rewrites reclaim only by a
>   full rebuild (vacuum).
> - **`live_node_rids` materializes every RecordId into a `Vec`**, which is O(nodes) memory at call
>   time (not bounded by the cache budget). Full scans that use it are the exception to the
>   resident-memory bound above.

- **Read path**: cache → WAL file → main `.chiffon` file. The first hit is returned and then inserted into the cache (evicting the LRU page if full).
- **Write path**: every write is appended to the WAL file immediately (write-through) and the page is cached. Because the durable copy already lives in the WAL, a cached page can be evicted at any time without data loss; the WAL keeps only a small `page_id → byte-offset` index in memory rather than the page bytes.
- **Checkpoint** (`flush`): WAL entries are written back to the main file and the WAL is cleared. Cached pages remain valid (they equal the checkpointed contents).
- **Rollback**: the cache is cleared and the WAL file is truncated back to the length captured at `begin()`, so post-snapshot writes vanish from both memory and the durable log.
