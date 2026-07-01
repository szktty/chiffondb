# 設計: セッション・オーケストレーター（自動化サーバー）

> 複数の Claude Code セッションを協調させ、`docs/review-process.md` の
> 実装⇄レビューのサイクルを自動で回すためのローカルサーバーの設計。
>
> Status: ドラフト（設計合意フェーズ）。Date: 2026-06-30。

## 1. 目的とスコープ

### 目的

`docs/review-process.md` が定める「実装セッション(fixer)」と「レビューセッション(verifier)」の
役割分離を、人間が手で turn を投げ続けなくても回し続けられるようにする。具体的には:

- 一方のセッションが成果物（指示書ファイル）を出したら、もう一方のセッションを起こして
  次のステップへ進める。
- ハンドオフの状態（いま誰の番か）を 1 か所で保持し、各セッションに「次に何をすべきか」を
  明示的に割り当てる。

### スコープ外（重要）

- **core ロジックの編集判断はサーバーがしない。** サーバーはあくまでハンドオフの調停役。
  verify/fix の中身の正しさは各セッション（とスキル `chiffon-review`）が担保する。
- **confirmation bias 防止の原則は維持する。** verifier と fixer は必ず別セッション。
  サーバーは同一セッションに verify と fix を連続で割り当ててはならない。
- **uncommitted な作業ツリーの保護も維持する。** review-process.md の「破壊的 git 禁止」は
  そのまま生きる。サーバーは git 操作を一切代行しない（各セッションがスキル経由で行う）。

## 2. なぜサーバーが要るか（Claude Code の実行モデル前提）

Claude Code のセッションは常駐デーモンではない。動くのは次の 3 契機だけ:

1. ユーザー／親セッションが turn を送ったとき。
2. `run_in_background` のツールが完了し harness が再呼び出ししたとき。
3. スケジューラ（`ScheduleWakeup` / cron / `send_later`）が発火したとき。

→ 「ソケットを開いて recv() でブロックし続ける常駐」はできない。そこで:

- **方式A (long-poll)**: セッションは `run_in_background` でサーバーへ long-poll する
  ブロッキングコマンドを 1 本走らせる。サーバーは「そのセッション宛の指示」が来るまで
  リクエストを握り、来た瞬間に 200 を返す → コマンド終了 → harness がセッションを再起動 →
  続きを処理。これが最も「反応があれば即処理」に近い。
- **方式B (定期ポーリング / フォールバック)**: long-poll がタイムアウト／切断したり、
  long-poll コマンド自体が想定外に死んだ場合に備え、`ScheduleWakeup` で長め(20〜30分)に
  起きて生存確認・取りこぼし回収する。

**両方併用**で運用する: 通常は A で低レイテンシ、A が壊れても B で生存。

## 3. アーキテクチャ

```
                ┌───────────────────────────────────────────┐
                │   orchestrator server (local, single proc) │
                │                                             │
                │   ┌─────────────┐    ┌──────────────────┐   │
                │   │ state machine│    │ long-poll waiters│   │
                │   │ (cycle FSM) │    │ (per session)    │   │
                │   └─────────────┘    └──────────────────┘   │
                │           │  reads/writes                    │
                │           ▼                                  │
                │   docs/*.md は「真実の源」ではなく観測対象     │
                │   状態は server が持つ (orchestrator state) │
                └───────────────────────────────────────────┘
                   ▲ long-poll            ▲ long-poll
                   │ (background Bash)     │ (background Bash)
            ┌──────┴───────┐        ┌──────┴───────┐
            │ session: impl │        │ session: review│
            │ (fixer)       │        │ (verifier)    │
            └───────────────┘        └───────────────┘
```

### 3.1 真実の源をどこに置くか

review-process.md の現行運用では、状態は **ファイルの存在**で表現されている
（`review-request-*` があり対応する `review-*` がない → verify の番、など）。
これは人間が読む分には良いが、自動化では競合しやすい（両セッションが同時に同じファイルを
見て同時に動く）。

→ **サーバーが権威ある状態(orchestrator state)を持つ**。`docs/*.md` は引き続き
各セッションが生成する成果物だが、「いま誰の番か」の調停はサーバーの state machine が行う。
サーバーはファイルの存在を**観測**して state を進めるが、割り当ては排他的に 1 セッションへ。

## 4. ステートマシン

`chiffon-review` の 5 サブコマンドをそのまま状態遷移に写す。1 サイクルは:

```
  IMPLEMENTING ──(request: review-request-* 出現)──► AWAITING_REVIEW
  AWAITING_REVIEW ──(verify 開始)──► VERIFYING
  VERIFYING ──(result: review-* 出現)──► REVIEWED
  REVIEWED ─┬─(Must fix あり)──► FIXING ──(fix→request)──► AWAITING_REVIEW   (ループ)
            └─(passes)────────► CLOSING ──(close: commit + plan + 次 implement-request)──► IDLE/次サイクル
```

各状態には **担当ロール**(impl / review)が紐づく。サーバーの不変条件:

- `VERIFYING` と `FIXING` を**同一セッションに割り当てない**(confirmation bias 防止)。
- 同時に active なのは各ロール 1 セッションまで(排他)。
- 状態遷移はサーバー内で逐次化(単一プロセス・単一ロックで直列化)。

### 4.1 遷移のトリガ

サーバーは次のいずれかで遷移を判定する:

1. セッションからの **明示的な完了通知**(後述 API の `POST /done`)。これが主。
2. `docs/*.md` の **ファイル出現の観測**(フォールバック検証)。明示通知と矛盾したら通知優先、
   ただしファイルが無いのに「done」と言われたら拒否(成果物の実在を確認)。

## 5. セッション側プロトコル

各セッションは起動時にサーバーへ登録し、long-poll で次の指示を待つ。

### 5.1 ライフサイクル

```
1. register:   POST /register {role: impl|review, session_id} → server が状態に応じ待機 or 即指示
2. wait:       (background Bash) GET /next?session_id=...  ← long-poll。指示が来るまでブロック
3. 指示受領:    {action: "verify"|"fix"|"result"|"close"|"request", request_file: "docs/..."}
4. 実行:        セッションは対応する `chiffon-review <action>` を実行
5. done:        POST /done {session_id, action, artifact: "docs/review-2026-..-x.md"}
6. → 2 へ戻る (再び long-poll)
```

### 5.2 long-poll を background Bash で回す形

セッション内では概念的に次のような background コマンドを 1 本維持する:

```bash
# run_in_background で起動。サーバーが指示を返すまで戻らない(サーバ側でタイムアウト握り)。
curl -sS --max-time 600 "http://127.0.0.1:PORT/next?session_id=$SID"
```

このコマンドが完了 → harness がセッションを再起動 → 返ってきた JSON の `action` を見て
該当スキルを実行 → 完了したら `/done` を叩き、再び `/next` の background コマンドを起動。

### 5.3 フォールバック(方式B)

long-poll コマンドが想定外に死ぬ/タイムアウトの取りこぼしに備え、各セッションは
`ScheduleWakeup` を 20〜30 分の長めで仕込み、起きたら `GET /next`(non-block 即時版、
`?wait=0`)で取りこぼしを回収してから long-poll を貼り直す。

## 6. サーバー API（最小）

すべて `127.0.0.1` バインドのローカル HTTP。認証はローカル限定で省略(将来トークン化)。

| Method | Path | 役割 |
|---|---|---|
| POST | `/register` | セッションをロールに登録。既存ロールがいれば 409 |
| GET  | `/next?session_id=&wait=N` | long-poll。担当の指示が来るまで最大 N 秒ブロック。無ければ 204 |
| POST | `/done` | アクション完了通知。artifact の実在を検証して状態遷移 |
| GET  | `/state` | 現在の FSM 状態・各ロールの割り当て(デバッグ/人間用) |
| POST | `/reset` | 手動でサイクルを初期化(人間の介入用) |

`/done` は **artifact ファイルの存在を確認**してから遷移する(嘘の done を弾く)。

## 7. confirmation bias と作業ツリー保護の担保

- **別セッション保証**: サーバーは impl ロールと review ロールを別 `session_id` として保持。
  `VERIFYING` を割り当てるのは review セッション、`FIXING` は impl セッション。サーバーが
  同一 session_id に両方を割り当てることは状態機械上ありえない。
- **破壊的 git の禁止**: サーバーは git を一切実行しない。コミット/スナップショットは各
  セッションが `chiffon-review` スキル経由で行い、review-process.md の「破壊的 git 禁止」
  ルールがそのまま適用される。
- **snapshot 強制**: `verify` 割り当て時、指示 payload に「最初に wip スナップショットを取れ」
  を含め、review-process.md の手順を毎回想起させる。

## 8. 実装方針(言語・配置)

- **言語**: サーバー本体は Rust(リポジトリが pure Rust。ただし core クレートには混ぜず、
  `tools/` 等の独立した小クレート or 単一バイナリとして分離する。core の依存を汚さない)。
  長poll は async ランタイム(tokio + axum or hyper)で待機者を保持。
- **状態の永続化**: 単一プロセス内メモリ + 起動時に `docs/` を観測して状態を復元
  (プロセス再起動耐性)。状態スナップショットを `docs/.orchestrator-state.json` 等へ
  ダンプしてもよい(要 .gitignore)。
- **セッション側**: 新しいスキル(例 `chiffon-orchestrate`)を足し、register/next/done の
  ループと `chiffon-review` の各サブコマンド実行を繋ぐ。

## 9. 未決事項 / 次の検討

- セッションの**起動**自体を誰がやるか。サーバーが新規セッションを spawn するのか、人間が
  2 枚開いて register させるのか。初期は後者(人間が 2 セッション開始)が単純。
- `Must fix` ループの**停止条件**(無限往復の上限、人間エスカレーション閾値)。
- long-poll のコネクション上限・タイムアウト値のチューニング。
- サーバープロセスのライフサイクル(誰が起動・終了するか、ヘルスチェック)。

## 10. 段階的な作り方

1. (本書) 設計合意。
2. 最小 PoC: long-poll サーバー + 1 往復の疎通(register→next→done→next)だけ動かし、
   background Bash からの再起動が本当に効くか実機確認。
3. FSM を `verify→result→fix→request→close` に拡張。
4. `chiffon-orchestrate` スキルでセッション側ループを実装。
5. フォールバック(ScheduleWakeup)と停止条件を追加。
