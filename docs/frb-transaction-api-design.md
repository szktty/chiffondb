# Transaction フラット API 設計案 — レビュー依頼

## 背景

ChiffonDB の Dart バインディング（`chiffondb` pub パッケージ）は flutter_rust_bridge (frb) v2 を介して `Connection` API を Dart に公開している。

現在の `Connection::begin()` は `Transaction<'a> { conn: &mut Connection, snapshot: Option<DbSnapshot> }` を返す。この型はライフタイム付き借用構造体であり、frb は `RustOpaque` として自動生成できない（frb 2.x は `'a` ライフタイムを持つ型の opaque 化に対応していない）。

このため Dart 側では Transaction を使ったトランザクション API が公開できない状態になっている。

## 提案: Connection にフラット手続き API を追加

`Connection` 構造体に 3 つのメソッドを追加し、`Transaction<'a>` を経由せずにトランザクションを制御する。

### シグネチャ（`api/mod.rs` への追加）

```rust
impl Connection {
    /// Begins a transaction. Saves a snapshot of the current database state.
    /// Returns an error if a transaction is already in progress.
    pub fn begin_transaction(&mut self) -> Result<(), String> {
        if self.pending_snapshot.is_some() {
            return Err("transaction already in progress".to_string());
        }
        let snap = self.db_mut()?.take_snapshot();
        self.pending_snapshot = Some(snap);
        Ok(())
    }

    /// Commits the current transaction. Flushes pending writes to disk.
    /// Returns an error if no transaction is in progress.
    pub fn commit_transaction(&mut self) -> Result<(), String> {
        if self.pending_snapshot.take().is_none() {
            return Err("no transaction in progress".to_string());
        }
        self.db_mut()?.flush().map_err(|e| e.to_string())
    }

    /// Rolls back the current transaction. Restores the database to the
    /// state at the time of the last `begin_transaction` call.
    /// Returns an error if no transaction is in progress.
    pub fn rollback_transaction(&mut self) -> Result<(), String> {
        match self.pending_snapshot.take() {
            None => Err("no transaction in progress".to_string()),
            Some(snap) => {
                self.db_mut()?.restore_snapshot(snap);
                Ok(())
            }
        }
    }
}
```

### Connection 構造体への `pending_snapshot` フィールド追加

```rust
pub struct Connection {
    inner: Option<Database>,
    pending_snapshot: Option<DbSnapshot>, // ← 追加
}
```

既存の `open`/`create`/`open_in_memory` の `Connection { inner: Some(db) }` 初期化を全て `Connection { inner: Some(db), pending_snapshot: None }` に変更する。

### 利用する既存 API

| メソッド | 場所 | アクセス修飾子 |
|---|---|---|
| `db.take_snapshot()` | `src/db.rs:1386` | `pub(crate)` |
| `db.restore_snapshot()` | `src/db.rs:1394` | `pub(crate)` |
| `db.flush()` | `src/db.rs:111` | `pub` |

`DbSnapshot` は `pub(crate)` のため、`Connection` と同じ crate（chiffondb-core）内でのみ参照可能。既存の `Transaction<'a>` も同様なので問題なし。

### 既存 `Transaction<'a>` との関係

- 既存の `begin()`/`Transaction<'a>` は**そのまま残す**（Rust ネイティブ利用者向け）。
- frb 公開対象は新しいフラット 3 メソッドのみ。`Transaction<'a>` は引き続き frb から無視される。

## Dart 側での想定呼び出し

```dart
await ChiffonDb.init();
final conn = await Connection.open(path: 'my.db');

await conn.beginTransaction();
try {
  await conn.insertNode(typeName: 'Person', propsJson: '{"name":"Alice"}');
  await conn.commitTransaction();
} catch (e) {
  await conn.rollbackTransaction();
}
```

frb は `begin_transaction`/`commit_transaction`/`rollback_transaction` を `crateApiConnectionBeginTransaction` 等として自動生成する（`-> Result<(), String>` は `Future<void>` に変換）。

## テスト観点

1. **commit で永続化**: begin → insert → commit → close → reopen して件数が増えていること
2. **rollback で破棄**: begin → insert → rollback → 件数が元に戻ること
3. **二重 begin はエラー**: begin 後に再び begin を呼ぶと `Err("transaction already in progress")` が返ること
4. **no-transaction rollback はエラー**: begin なしで rollback を呼ぶと `Err("no transaction in progress")` が返ること
5. **commit 後は rollback 不可**: commit 後に rollback を呼ぶと `Err("no transaction in progress")` が返ること

---

## レビューで確認したい点

1. `Connection` に `pending_snapshot: Option<DbSnapshot>` を持たせることで、**既存の `Transaction<'a>` と意味的に矛盾しないか**（ネスト transaction を実装する予定があれば設計を変える必要がある）
2. **ネスト transaction のサポート**は将来必要か（今回は単一保留のみ）
3. `take_snapshot`/`restore_snapshot` を `pub(crate)` から `pub(super)` 等に変える必要があるか（`Connection` と `Database` は同一 crate 内なので現状で問題ないはずだが確認）
4. 既存の `Connection` の初期化箇所（`open`/`create`/`open_in_memory`/`open_with_max_memory`/`create_with_max_memory`）全てに `pending_snapshot: None` を追加することへの懸念点

---

## 補足: frb codegen との関係

現在 `api/mod.rs` は `Transaction<'a>` を通じて `pub(crate)` な `DbSnapshot`/`take_snapshot`/`restore_snapshot` を参照しているため、`chiffondb-ffi`（Dart バインディングの FFI crate）から `api/mod.rs` をコンパイルできない状態になっている。

**このフラット API が実装されると `api/mod.rs` の `pub(crate)` 内部型への直接参照がなくなり**、`chiffondb-ffi` からのコンパイルが通るようになって frb codegen が完成する。つまりこのレビューの承認・実装が Dart バインディング完成の前提条件になっている。
