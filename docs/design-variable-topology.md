# 設計: 可変長トポロジ（論理ページディレクトリ導入）

- ステータス: ドラフト
- 作成日: 2026-06-30
- 対象: `chiffondb-core` のストレージ／トポロジ層
- 作業ブランチ: `feature/variable-topology`
- 関連: [ARCHITECTURE.md](../ARCHITECTURE.md), `docs/plan-page-cache.md`（Phase 2）

## 0. 作業方針

- **作業ブランチ**: `feature/variable-topology` で実装する。
- **実装モデルの使い分け**: トークン節約のため、Sonnet で十分高品質な実装が可能な機能は
  Sonnet で実装する（純粋ロジック・定型的な serialization・テストなど）。設計判断や
  ストレージ層の不変条件に関わる箇所は Opus で扱う。
- **後方互換性は考えない**: どの機能でも破壊的変更を行ってよい。後方互換性よりも
  **ストレージサイズ（容量上限）の拡大を優先**する。旧形式ファイルのアップグレード変換や
  移行パスは実装しない。`VERSION` は単に bump し、旧バージョンは `deserialize` で拒否する。

## 1. 目的と背景

現在、ノード数は **約2000** が上限である。これはアルゴリズム上の限界ではなく、
固定サイズのトポロジセグメントから決まる構造的制約に過ぎない。

### 現状の制約構造

- ファイルは固定境界の3セグメント（topology / property / vector）に分かれている。
  境界はヘッダの `*_segment_start` に記録される。
  - `topology_segment_start = 1`
  - `property_segment_start = 64`（= topology の終端）
- トポロジセグメントは固定範囲 `[1, 64)` の **63ページ**。
- node ページと edge ページを**物理的にインターリーブ**配置している
  （`topology.rs` の論理→物理マッピング）。
  - node 論理ページ `i` → 物理ページ `topo_start + i*2`
  - edge 論理ページ `i` → 物理ページ `topo_start + i*2 + 1`
- したがって node 用は `63.div_ceil(2) = 32` ページ、edge 用は `31` ページに固定。
- 1ページ（4096B）あたり node レコードは `NODE_RECORD_SIZE = 64`B とビットマップから
  約 **63スロット**。
- `32ページ × ~63 ≈ 約2000ノード`。超過時は `alloc_node_slot` が
  `GraphError::CapacityExceeded`（`error.rs`）を返す。

property セグメントが物理ページ64から固定で始まるため、topology はそれ以上伸びられない。

### 重要な既存事実

- `RecordId.page_id`（u32）には**物理ファイルページではなく「論理ページ番号」**が入っている
  （`RecordId::new(logical as u32, slot.0)`）。物理変換は `node_pid()` / `edge_pid()` が担う。
  → 外部に露出する RecordId はすでに論理アドレスであり、本変更で**意味は変わらない**。
- `page_directory_root` ヘッダフィールドは ARCHITECTURE.md 上 **MVCC 用に予約**（常に0）。
  本設計の page directory とは**目的が異なる**点に注意（後述 §6）。

## 2. ゴール

論理ページ → 物理ページの間接テーブル（page directory）を導入し、
topology / property / vector を任意の物理ページに配置可能にする。
これにより固定境界を撤廃し、topology を動的に拡張できるようにする。

### 到達可能なノード数

- 論理ページ番号は `RecordId.page_id` の **u32**。node 1ページ ~63 スロットなので、
  アドレス空間としての上限は `2^32 × 63 ≈ 2700億ノード`。
- 実際の制約は (a) ディスク容量、(b) `node_page_count`（現在 u32）、
  (c) page directory 自体のサイズ。現実的には **数億〜数十億ノード**を狙える。
- 常駐メモリは従来どおり page-cache 予算で頭打ち（page directory もキャッシュ経由で参照）。

## 3. 設計概要

### 3.1 page directory

論理ページ番号 → 物理ページ番号 の配列。物理ページ群（チェーン）に永続化する。

```
logical node page i ─┐
                     ├─> page directory ─> physical page
logical edge page i ─┘
```

- エントリは物理ページ番号 `u32`。1ページ（4096B、ヘッダ8B を除く）に約 `4088/4 ≈ 1022` エントリ。
- 論理空間が増えたら directory ページをチェーンで追加（property の page chain と同じ要領）。
- ルートはヘッダの専用フィールド（新設、§6 で `page_directory_root` との切り分けを決定）。

### 3.2 論理アドレス空間の分け方

現状は node/edge を物理インターリーブで分けている。directory 導入後は
**論理番号空間そのものを node/edge で分離**する（物理配置は directory に委ねる）。

候補:
- (A) 名前空間プレフィックスを論理番号の最上位ビットで分ける（node = 0xx…, edge = 1xx…）。
- (B) node/edge それぞれに独立した directory を持つ。

→ **決定: (B) 独立 directory**（シンプルな方針を優先）。`node_page_count` /
`edge_page_count` が既に独立管理されており、スキャン（`live_node_rids` 等）も
論理 0..count で回しているため、移行が素直。

### 3.3 物理ページの確保と再利用

- 新規物理ページは現状どおり `append_page` で末尾追加。
- 解放ページの再利用（free list）は**本設計の必須要件ではない**が、directory があると
  自然に載るため将来拡張として節を立てる（§7）。

## 4. 影響範囲

| 層 | 変更 |
|----|------|
| `storage/file.rs` | ヘッダに directory ルート用フィールド追加 / `VERSION` bump |
| `storage/topology.rs` | `node_pid`/`edge_pid` を directory 経由の解決に変更、`*_capacity` 撤廃 |
| 新規 `storage/page_directory.rs` | directory の読み書き・拡張 |
| `storage/value.rs` | property ページ確保が固定 `prop_start` 前提 → directory 経由へ |
| `db.rs` | セグメント事前確保ロジック（pages 1..prop_start の zero-fill）の撤廃 |
| `error.rs` | `CapacityExceeded` の扱い（u32 枯渇時のみに縮退） |
| ARCHITECTURE.md | セグメント固定境界の記述を更新 |

## 5. 移行（既存ファイルの互換）

**後方互換性は考えない**（§0）。

- `VERSION` を 3 → 4 に bump。
- 旧形式（v3、固定セグメント）ファイルは `FileHeader::deserialize` が**そのまま拒否**する
  （現状の version 不一致エラーの挙動を踏襲）。アップグレード変換は実装しない。
- 容量上限の拡大を最優先とし、移行パスの実装コストはかけない。

## 6. `page_directory_root`（MVCC 予約）との関係

ARCHITECTURE.md は `page_directory_root` を **MVCC のトランザクション単位 page directory**
用に予約している。本設計の directory は「論理→物理の恒久マッピング」であり、目的が異なる。

選択肢:
- (i) **同一機構に統合**: 本設計の directory をそのまま将来 MVCC のルートに発展させる
  （CoW でルートを差し替える土台になる）。フィールドを再利用。
- (ii) **別フィールドに分離**: 恒久 directory と MVCC スナップショット root を別管理にする。

→ **決定: (i) 統合**（シンプルな方針を優先）。directory 間接化は CoW の前提そのものなので
統合が自然。MVCC 実装時にルート差し替えのセマンティクスを確定させる（将来作業）。

## 7. 将来拡張（本設計のスコープ外）

- 解放物理ページの free list（directory と相性が良い）。
- CoW / MVCC（directory のルート差し替え）。
- `node_page_count` / `edge_page_count` の u32 → u64 化（u32 枯渇は現実的に遠いので後回し）。

## 8. 決定事項 / 未決事項

決定済み:
1. ~~§6: MVCC 用 `page_directory_root` と統合か分離か~~ → **決定: 統合（§6）**。
2. ~~§5: 旧 v3 ファイルの扱い~~ → **決定: 後方互換を取らず拒否（§0, §5）**。
3. ~~§3.2: node/edge の論理空間分離~~ → **決定: (B) 独立 directory（§3.2）**。
4. ~~インデックスのスコープ~~ → **決定: 容量拡大とインデックスを同時に設計（§11）**。

未決（実装途中で詰める）:
5. directory のキャッシュ戦略（page-cache に載せるだけでよいか、ホットなルート段を別持ちするか）。
6. WAL / ロールバックとの整合（directory 拡張も WAL 経由のため write-through で問題ないかの確認）。
7. インデックスのデータ構造詳細（B-tree のノードレイアウト・分割閾値など。§11 で方針のみ確定）。

## 9. 段階実装プラン（案）

- Phase 1: page directory データ構造と読み書き（単体テスト先行。容量境界・チェーン拡張）。
- Phase 2: `topology.rs` の物理解決を directory 経由に差し替え（既存テスト緑維持）。
- Phase 3: property / vector セグメントの固定境界撤廃。
- Phase 4: ラベルインデックス（§11 層1）。`list`/`count`/型フィルタを O(matches) に。
- Phase 5: プロパティインデックス（§11 層2）。スキーマ DSL 拡張 + B-tree。
- Phase 6: 一意制約（§11 層3）をプロパティインデックスの一意版として。
- Phase 7: `VERSION` bump・ARCHITECTURE.md 更新。
- 各 Phase の停止点で `docs/review-request-*` を作成（CLAUDE.md のレビューワークフロー）。

### コスト見直しの方針

インデックス（特に Phase 5 の B-tree）は実装コストが大きくなりうる。途中で**実装コストが
過大と判明した場合は立ち止まり、スコープを見直す**（例: B-tree → 等価専用ハッシュへ縮退、
層2/層3 を別ブランチへ切り出す等）。容量拡大（Phase 1–3）は本ブランチの主目的なので維持し、
インデックスの段は柔軟に調整する。

## 10. 実装完了後の作業（クリーンアップ & マージ）

実装・テスト・レビューがすべて完了した後、以下を順に行う。

1. **ストレージ実装ドキュメントの執筆**: page directory と可変長トポロジの実装詳細を
   恒久ドキュメントとして書く（`ARCHITECTURE.md` への統合、または専用ドキュメント。
   オンディスクレイアウト・論理→物理解決・directory チェーン拡張・容量上限を含む）。
2. **進捗／レビュー系ドキュメントの削除**: 実装過程で作成・コミットした
   `docs/review-request-*` / `docs/review-*` などの進捗・レビュー用ドキュメントを削除する
   （恒久ドキュメントへ要点を吸収した上で）。
3. **コメント・ドキュメントの英語化**: 実装中に追加したコード内コメントおよびドキュメントを
   **すべて英語で書き直す**（CLAUDE.md のコメント規約に従う）。この設計ドキュメント自体も
   削除対象（§10-2）か英語化対象かは、恒久ドキュメントへの吸収後に判断する。
4. **CHANGELOG.md の更新**: `[Unreleased]` に簡潔なエントリを追加する。詳細はストレージの
   実装ドキュメントを参照する旨を記す（CHANGELOG 本体には要点のみ）。
5. **スカッシュマージ**: `feature/variable-topology` を `main` へスカッシュマージする。
   ただし **マージ準備の際は必ず指示を仰ぎ、指示があるまでコミット・マージを行わない**。

## 11. インデックス設計（標準グラフ DB 準拠）

容量を数億ノードへ拡大すると、現状の**全件スキャン**検索（`index.rs` の `find` /
`find_all` / `rids_of_type`、および `list_nodes` / `count_nodes` の型フィルタ）はすべて
O(nodes) のため実用不可になる。容量拡大とインデックスは不可分なので同時に設計する。

一般的なプロパティグラフ DB のインデックス階層に倣う。

### 層1: ラベル（型）インデックス — 暗黙・常時

- 「あるラベル（型）を持つノード全集合」への高速アクセス。`list_nodes("User")` /
  `count_nodes` / `rids_of_type` の起点であり、層2スキャンの起点でもある。
- ユーザー宣言は不要。**全型に対し常時**保持する。
- 構造: `type_id → ノード RecordId 集合`。実装は B-tree もしくは型ごとのページチェーン
  （挿入/削除が O(log n)〜O(1)）。page directory 上に載せ、メモリは page-cache で頭打ち。

### 層2: プロパティインデックス — 明示宣言（B-tree）

- `(型, プロパティキー) → ノード` の二次インデックス。`find` / `find_all` を加速する。
- **明示宣言**: スキーマ DSL に `@index` 相当のアノテーションを追加（§11.1）。
- 構造: **オンディスク B+tree**。等価検索に加え、範囲クエリ・`OrderBy`
  （traversal に既存）も加速できる。page directory 上に配置。
- インデックスキーは **`PropertyPath`**（`traversal/command.rs` に既存）を流用し、
  フラットキーと **JSON ネストキー**の両方をサポート（§11.2）。

### 層3: 一意制約（unique constraint）

- `(型, プロパティキー)` の一意性を保証。内部的には層2の**一意版インデックス**。
- スキーマ DSL の `@unique` 相当で宣言。挿入/更新時に重複を検出し
  `GraphError`（新変種）を返す。`id` のような一意キーの自然な表現になる。

### 層4: 全文 / ベクトルインデックス — 将来作業（スコープ外）

- 全文検索・近傍探索（vector セグメントは既にヘッダ予約あり）。本ブランチでは扱わない。

### 11.1 スキーマ DSL 拡張

現状の `field_def = ident ~ ":" ~ type_expr`（`schema.pest`）にフィールド単位の
アノテーションを追加する。記法は実装時に確定（例: `email: String @unique`,
`name: String @index`）。`ast.rs` の `FieldDef` にインデックス/一意フラグを持たせ、
`apply_schema` 時にインデックスを構築する。

### 11.2 JSON ネストキーのインデックス

- ChiffonDB には既に `PropertyPath::{Flat, Path}`（`traversal/command.rs`）があり、
  `resolve()` が `Value::Object` を walk して**ネストしたスカラー値**を取り出す。
  これをインデックスキーの生成にそのまま流用する。
  - フラット: `User.email`
  - JSON ネスト: `User.profile.city`（`Path { path: ["profile", "city"] }`）
- 制約（現状 `resolve` の仕様を継承）:
  - **スカラー終端のみ**。中間がオブジェクトでない／最終がオブジェクトの場合は対象外。
  - **配列インデックスは非対応**（`items[0]` は walk しない）。
  - 解決値が `None` のノードはインデックスに載らない（**部分インデックス**相当）。
- これは標準グラフ DB の「ネストプロパティ / JSON パスインデックス」に相当し、
  スカラー終端のみという制約も標準実装と整合的。

### 11.3 整合性

- インデックスはノードの挿入 / 更新 / 削除と**同一トランザクション内**で更新する
  （WAL write-through に載せ、ロールバックで一緒に巻き戻る）。
- 索引と実体（topology / property）の二重更新の原子性は WAL に委ねる。
