# 引き継ぎ: セッション・オーケストレーターを独立リポジトリで作る

> 別リポジトリ(別セッション)でこのサーバーの実装を始める人(=Claude)向けの cold-start メモ。
> ローカルで ChiffonDB リポジトリも読める前提。設計の全文はコピーしない —
> 下記の一次資料を必ず最初に読むこと。
>
> Date: 2026-06-30。

## 0. まず読む一次資料(この順で)

ChiffonDB リポジトリ(`<chiffondb>` = このファイルがあるリポジトリのルート)内:

1. `<chiffondb>/docs/design-session-orchestrator.md` — **本体の設計。これが正典。**
   目的・実行モデル前提・アーキテクチャ・FSM・API・段階的な作り方が全部ここにある。
2. `<chiffondb>/docs/review-process.md` — 自動化する対象のプロセス。役割分離(verifier/fixer)
   と「破壊的 git 禁止」「uncommitted ツリー保護」の原則。サーバーはこれを壊さない。
3. `<chiffondb>/.claude/skills/chiffon-review/SKILL.md` — 5 サブコマンド
   (verify/result/fix/request/close)。FSM の状態はこれに 1:1 で対応する。
4. `<chiffondb>/CLAUDE.md` — コード規約(`unwrap`/`expect` 禁止、`unsafe` 禁止、エラーは
   `thiserror` + `?` で伝播、コメントは英語で WHY のみ)。**新リポジトリでも同じ規約を踏襲する。**

## 1. このプロジェクトが解く問題(1 段落)

Claude Code のセッションは常駐デーモンではなく、turn/バックグラウンドツール完了/スケジューラ
発火の 3 契機でしか動かない。だから「実装セッション ⇄ レビューセッション」を人手の turn 投入
なしで回すには、ハンドオフを調停するローカルサーバーが要る。各セッションは `run_in_background`
で long-poll を 1 本握り、サーバーが指示を返した瞬間に再起動して続きを処理する(方式A)。
long-poll 死亡に備えた `ScheduleWakeup` の長めフォールバック(方式B)を併用する。

## 2. 新リポジトリのセットアップ

- **言語**: Rust。async は tokio + axum(or hyper)。long-poll は待機者(waiter)をサーバ内に
  保持して実現する。
- **配置**: ChiffonDB の core クレートには絶対に混ぜない(設計で明記)。独立リポジトリなので
  そもそも別だが、依存は最小に保つ。
- **規約**: 上記 CLAUDE.md と同じ(`unwrap`/`expect` 禁止 → `?` 伝播、`unsafe` 禁止、
  コメントは英語で非自明な WHY のみ)。新リポジトリにも同等の CLAUDE.md を置くこと。
- **状態の永続化**: 単一プロセス内メモリ + 起動時に観測で復元。スナップショットを JSON で
  ダンプするなら `.gitignore` に入れる。

## 3. 最初の一歩 = PoC(最大の不確実性をここで潰す)

設計 §10 の段階 2。**「background Bash の long-poll が完了したらセッションが本当に再起動して
続きを処理できるか」**が全体の成否を握る最大の未検証点なので、ここを最初に実機確認する。

PoC のゴール(これだけ):
1. `127.0.0.1` バインドの HTTP サーバーを立て、`GET /next` を long-poll(指示が来るまで握る、
   `--max-time` 相当のサーバ側タイムアウトで返す)で実装。
2. `POST /done` で「次の指示」を投入できる。
3. ChiffonDB 側(or 任意)のセッションから `run_in_background` で
   `curl -sS --max-time 600 http://127.0.0.1:PORT/next?session_id=poc` を起動 →
   別経路から `/done` を叩く → curl が返る → **セッションが再起動して続行する**ことを確認。

これが確認できたら FSM(§4)とセッション側ループへ進む。効かなければ方式B(ポーリング)主体に
設計を寄せ直す判断材料になる(早期に分かるほど良い)。

## 4. PoC の次(設計 §10 段階 3〜5)

3. FSM を `IMPLEMENTING→AWAITING_REVIEW→VERIFYING→REVIEWED→(FIXING ループ|CLOSING)` に拡張。
   - 不変条件: `VERIFYING` と `FIXING` を**同一 session_id に割り当てない**(confirmation bias)。
   - `/done` は **artifact ファイルの実在を確認**してから遷移(嘘の done を弾く)。
4. セッション側スキル(`chiffon-orchestrate` 仮称): register→next→(該当する
   `chiffon-review <action>` 実行)→done→next のループ。`verify` 割り当て時の payload には
   「最初に wip スナップショットを取れ」を必ず含める(review-process.md の手順を毎回想起させる)。
5. フォールバック(`ScheduleWakeup` 20〜30分)と Must fix 無限往復の停止条件・人間エスカレーション。

## 5. 越えてはいけない一線(設計で確定済み、再議論しない)

- サーバーは **core ロジックの正否を判断しない**。ハンドオフの調停のみ。
- サーバーは **git を一切実行しない**。コミット/スナップショットは各セッションが
  `chiffon-review` 経由で行い、review-process.md の破壊的 git 禁止がそのまま生きる。
- verifier と fixer は **必ず別セッション**。FSM 上、同一セッションに verify と fix を
  割り当てられないようにする。

## 6. 未決事項(設計 §9 — 新リポジトリで詰める)

- セッションの**起動**を誰がやるか。初期は人間が 2 セッション開いて register が単純(推奨)。
- `Must fix` ループの停止条件(往復上限・人間エスカレーション閾値)。
- long-poll のコネクション上限・タイムアウト値・サーバープロセスのライフサイクル/ヘルスチェック。

## 7. 連絡事項

- ChiffonDB リポジトリ側は別ブランチ(`feature/variable-topology`)で進行中レビューサイクルの
  **未コミット作業ツリー**を持っている。オーケストレーターは独立リポジトリで作るので衝突しないが、
  PoC の疎通テストで ChiffonDB セッションを使う場合はその作業ツリーを巻き込まないこと
  (破壊的 git 厳禁)。

<!-- triad動作テスト用のダミー追記(2026-07-02, worker-1) -->
<!-- 本行は /done の message フィールドのテストとして worker セッションが追記した。 -->
<!-- 本行はサーバー再起動後の再登録・3回目のダミーサイクルの疎通確認として追記した。 -->
