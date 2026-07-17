---
name: working-log
description: >-
  Resume or record cross-repo session state in ../chiffondb-private/docs/working.md
  (shared by chiffondb / chiffondb-dart / chiffondb-dart-generator). Use this skill
  at the START of a session to catch up on where things stand ("read working.md and
  resume", or any time prior session context in another of these repos might matter),
  and at the END of a session (or a meaningful chunk of work) to append a new entry
  so the next session — in this repo or another — can pick up without re-deriving
  context. Also use it when asked to check progress across the ChiffonDB repos.
---

# working-log

`chiffondb` / `chiffondb-dart` / `chiffondb-dart-generator` の3リポジトリで進行中の
AI セッションが、作業内容を1本の共有ログ `../chiffondb-private/docs/working.md` に
記録する運用に沿って動くためのスキル。**ファイルにしか存在しない指示は見落とされる** —
このスキルはその指示を「今のセッションが実行できる手順」に変換する。

## 大原則

`working.md` 自身の冒頭（運用ルール／エントリ形式／現在のアクション項目）が正。
このスキルはそれを繰り返さず、**読む→書く→コミットする**の手順だけを担う。
運用ルールの詳細（アーカイブ閾値、エントリ形式の項目など）が知りたければ
`working.md` を直接読むこと。

## 再開時（セッション開始時）にやること

1. `../chiffondb-private/docs/working.md` を読む。
   - まず先頭の「現在のアクション項目」で自分のリポジトリ（`chiffondb` /
     `chiffondb-dart` / `chiffondb-dart-generator`）の行を確認する。
   - 次に、自分のリポジトリ名がついた直近のエントリ（先頭から探して最初に
     一致するもの）を読み、`### 現在地` と `### 次にやること（再開手順）` に従う。
   - 判断が必要な分岐は、そのエントリに書かれた判断基準を使う。書かれていない
     独自の判断で上書きしない。
2. 他リポジトリ由来のエントリ（`[meta]` や別 repo タグ）で、自分の作業に影響する
   ものがないか流し読みする。特に「未畳み込み wip コミット」「別セッション由来の
   未コミット差分」など、触ると事故る系の注意書きに注意する。
3. `working.md` に書かれている前提（ブランチ名、直近コミットハッシュ、VERSION 番号
   など）は必ず `git log` / 現在のコードで裏を取ってから使う。ズレていたら
   ズレている旨を新しいエントリに書く（黙って上書きしない）。

## 記録時（セッション終了時、または意味のある区切りごと）にやること

1. `working.md` の「エントリ形式」節のテンプレートに従って新しいエントリを書く。
   次のセッションが**このエントリだけ読んで再開できる**粒度を満たすこと:
   - `### 現在地`: ブランチ／直近コミット／ワークツリーの状態。
   - `### やったこと / 決めたこと`: 変更・決定の要点と理由。
   - `### 次にやること（再開手順）`: 次に読むファイル→具体的な次の一手→
     判断が要る分岐は判断基準まで。省略しない（作業が完全に終わっている場合のみ
     「次なし」と明記）。
   - `### 落とし穴 / 引き継ぎ注意`: ハマりどころ、未解決の課題、暗黙の前提。
2. 新しいエントリは**ファイル先頭（運用ルール節の直後）**に追加する。過去の
   エントリは書き換えない（訂正は新エントリとして書く）。
3. 自分のリポジトリの「現在のアクション項目」の行を更新する（完了したら
   「対応不要」に書き換える）。
4. `../chiffondb-private` ディレクトリで完結してコミットする。**この
   コミットは指示を待たず実行してよい**（`working.md` の運用ルールに明記済み）。
   ```bash
   cd ../chiffondb-private
   git add docs/working.md
   git commit -m "docs: working log — <一行要約>"
   ```
   `git push` は行わない（ユーザーが行う運用）。
5. パスワード／APIキー／トークン等の実際の機密文字列は書かない。それ以外
   （内部事情、他セッションの状況、ハマりどころ）は遠慮なく書いてよい
   （非公開リポジトリのため）。

## エントリ数がすでに20件を超えていたら

`working.md` のアーカイブ運用ルールに従い、古い方から溢れた分を
`docs/archive/working-YYYY-MM.md` に移動する。このスキルの記録作業とは独立した
メンテナンスなので、自分のエントリ追加のついでに気づいたら対応する程度でよい
（必須ではない）。
