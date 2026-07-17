# ChiffonDB

Lightweight embedded property graph database written in pure Rust

ChiffonDB is a lightweight embedded property graph database engine designed to be embedded in consumer desktop and mobile applications. It runs as a single file, with a low memory footprint and no GC (Rust).

## Features

- Schema enforcement — node types, edge types, and properties are defined in a DSL (`.graph` files). Unknown type or property names become compile errors.
- JSON AST traversal API — queries are expressed as a structured sequence of JSON instructions, designed to be written by AI.
- Cypher subset support — basic Cypher queries can also be executed (experimental).
- Lightweight storage design — fixed-size, page-based row storage, optimized for traversal.
- Low memory footprint (bounded) — all on-disk data (property/blob pages, topology node/edge records) is read and written through a single fixed-size LRU page cache (SQLite-pcache style), and there is no in-memory index that grows with the data. Resident memory therefore stays capped by the cache budget regardless of database size. The cap is configurable (default 4 MiB) via `open_with_max_memory` / `create_with_max_memory`. Measured: with a 64 KiB budget the resident set stays at 16 pages whether the database holds 500 or 1800 nodes — occupancy does not track node count (`cargo run --example cache_memory_bench --release`).
- CLI tool — the `chiffon` command performs database operations, schema management, and query execution.

> **Current limit:** the topology segment is fixed-size, so a single database holds at most ~2000 nodes (a variable-length segment is future work). Copy-on-write/MVCC are planned but not yet implemented; durability is provided by a write-ahead log.

## Quick Start

```rust
use chiffondb::Connection;

fn main() -> Result<(), String> {
    // Create a new database
    let mut db = Connection::create("myapp.chiffon".to_string())?;

    // Apply a schema
    db.apply_schema(r#"
        node Person {
            name: String
            age:  Int
        }
        node City {
            name: String
        }
        edge LIVES_IN {
            from: Person
            to:   City
        }
    "#.to_string())?;

    // Insert nodes
    let alice = db.insert_node("Person".to_string(), r#"{"name":"Alice","age":30}"#.to_string())?;
    let tokyo = db.insert_node("City".to_string(), r#"{"name":"Tokyo"}"#.to_string())?;

    // Insert an edge
    db.insert_edge("LIVES_IN".to_string(), alice, tokyo, "{}".to_string())?;

    // JSON AST traversal
    let result = db.execute_traversal(r#"{
        "version": 1,
        "start": { "type": "Node", "label": "Person", "key": "name", "value": "Alice" },
        "steps": [
            { "action": "OutEdges", "label": "LIVES_IN" },
            { "action": "OutNodes", "label": "City" }
        ],
        "collect": { "type": "Nodes", "properties": ["name"] }
    }"#.to_string())?;
    println!("{result}"); // [{"name":"Tokyo"}]

    // Cypher query (experimental)
    let result = db.execute_cypher(
        "MATCH (p:Person) WHERE p.age > 20 RETURN p.name, p.age".to_string()
    )?;
    println!("{result}"); // [{"p.age":30,"p.name":"Alice"}]

    Ok(())
}
```

## Language bindings

ChiffonDB is a Rust library. Bindings for other languages are distributed separately.

| Language | Package | Repository |
|----------|---------|------------|
| Rust | [`chiffondb`](https://crates.io/crates/chiffondb) | this repository |
| Dart / Flutter | `chiffon` (pub.dev) | _(coming soon)_ |

## Schema DSL

Node types, edge types, and properties are defined in `.graph` files.

```
node User {
    name: String
    age:  Int
    bio:  String  // a property may be omitted or set to null
}

node Post {
    title:      String
    body:       String
    created_at: DateTime
}

edge WROTE {
    from: User
    to:   Post
    props: {
        at: DateTime
    }
}
```

Type names and property names are validated against the schema, so referencing an undefined type or property is reported as an error.

## Data types

| Type | Internal representation | Description |
|------|-------------------------|-------------|
| `Int` | `i64` | Integers and timestamps |
| `Float` | `f64` | Decimals and scores |
| `Boolean` | `bool` | Flags |
| `DateTime` | `i64` (UTC ms) | Date and time |
| `String` | UTF-8 | Text |
| `Json` | UTF-8 JSON | Escape hatch for miscellaneous metadata |
| `Blob` | Byte string | Images and arbitrary binary (page-chained above 4 KB) |
| `List<T>` | MessagePack | Array of primitives |
| `Map<K, V>` | MessagePack | Key-value map |
| `Vector<N>` | `f32 × N` | Embedding vector (for future GraphRAG support) |

## CLI commands

```bash
# Initialize a database
chiffon init --db myapp.chiffon

# Apply a schema
chiffon schema apply --db myapp.chiffon --schema schema.graph

# Show the schema
chiffon schema show --db myapp.chiffon

# Run a JSON AST query
chiffon query --db myapp.chiffon --json query.json

# Show database info
chiffon info --db myapp.chiffon
```

## Build and run

```bash
# Build
cargo build --release

# Test
cargo test

# Lint check
cargo clippy -- -D warnings
```

## Documentation

- [Architecture](ARCHITECTURE.md) — storage layout, record format, durability/concurrency model

## Author

SUZUKI Tetsuya <tetsuya.suzuki@gmail.com>

## License

[Apache License 2.0](LICENSE)
