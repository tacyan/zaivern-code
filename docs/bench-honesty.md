# ベンチと実バイナリ試験が「静かな嘘」をつかないための決まり

このリポジトリのベンチは製品の主張（「衝突ゼロ」）を支える数字を出す。
数字が嘘だと気付けないまま主張だけが残るのが最悪の失敗なので、
**測る前に「測れる状態か」を確かめる**ことをすべてのベンチの前提にする。

以下は 2026-08-11 に**実際に起きた** 4 件の事故と、その恒久対策である。

---

## 事故1 — `--version` の照合では古いバイナリを捕まえられない

### 何が起きたか

`guard::tests::本物のフック越しに離れた行域はコミットでき重なると止まる` が
「はみ出したのに通った」で赤くなった。原因はロジックではなく、
**ソースが 06:00 なのに `target/debug/zai` が 02:40 のビルド**だったこと。

決定的なのは次の点である:

```
$ ./target/debug/zai --version
Zaivern Code 0.14.0        ← 期待どおり
$ ls -la target/debug/zai
-rwxr-xr-x ... 8月 11 02:40 target/debug/zai   ← 中身は 4 時間前
```

**版は完全に一致している。** CLAUDE.md が定めていた「`--version` を照合して
違えば飛ばす」は、この形の事故を **1 件も捕まえられない**。

### なぜ普通に起こるのか

`cargo test` も `cargo test --bin zai --no-run` も **bin を作らない**。
`tools/verify.sh` は後者しか呼ばないので、**通常の開発ループでは
`target/debug/zai` は一度も更新されない**。前の実行の残骸がそこに残り続ける。

### 対策 — `crate::test_util::real_zai`

実バイナリを使うテストは、この 1 つの関所を通る。

| 判定 | 意味 | 振る舞い |
| --- | --- | --- |
| `Usable` | 版が一致し、ソースより新しい | 使う |
| `Missing` | 実行ファイルが無い | `[skip]` + 理由 |
| `WrongVersion` | `--version` が動かない／版違い | `[skip]` + 理由 |
| `Stale` | **版は同じだがソースより古い** | `[skip]` + 理由 |
| `Unmeasurable` | ソースツリーが隣に無く古さを測れない | `[warn]` を出して使う |

**黙って赤にしない・黙って緑にしない。** 使えないときは必ず
「誰が新しいのか」「どうすれば直るのか」まで stderr へ出す。

```
[skip] test_util の自己点検: 隣の zai がソースより古いです
       (plugins.rs の方が新しい (ソース 1786398218s / バイナリ 1786383600s))。
       版は合っているので `--version` の照合では捕まりません。
       `cargo build --bin zai` を先に走らせること
```

判定そのもの（[`judge_zai`]）は I/O を持たない純粋関数なので、
全 OS で表としてテストに固定してある。

### mtime とビルド ID — どちらを採るか（実測して決めた）

| 案 | 費用 (128 ファイル / 12.4MB) | 確実さ | 実現性 |
| --- | --- | --- | --- |
| A. mtime 比較 | **0.51 ms** | 内容が同じでも mtime が動けば「古い」と誤検出しうる | 追加の仕組み無し |
| B. 内容ハッシュ (ビルド ID) | 62.9 ms (**124 倍**) | 内容そのもの | `build.rs` で埋め、`cli.rs` で出す必要がある |

**A を採った。** 理由は 2 つ:

1. 費用が 124 倍違う。しかも B は「バイナリ側にも同じ ID を焼く」ため
   `build.rs` と `cli.rs` の両方に手が要る（この作業単位では
   どちらも別のエージェントが編集中で触れない）。
2. A の誤りは**安全側に倒れる**。誤検出は「使えるのに skip する」であって、
   「古いのに使う」ではない。B が防げて A が防げないのは
   「内容が同じなのに mtime だけ新しい」場合だけで、これは skip が増えるだけ。

**A の既知の弱点（正直に書く）**: `git checkout` / `git merge` はソースの
mtime を更新するので、内容が変わっていなくても skip になる。実際にこの作業中、
`git checkout -- src/plugins.rs` の直後に関所が発火した。直し方は
`cargo build --bin zai` を 1 回走らせるだけ。

**将来 B へ移るなら**: `build.rs` が `cargo:rustc-env=ZAIVERN_BUILD_ID=<hash>` を
出し、`zai --version`（か専用の隠しフラグ）がそれを表示し、
`judge_zai` の `version_line` 照合をその ID との一致に差し替えればよい。
判定が純粋関数に分かれているので、差し替えは 1 箇所で済む。

### 適用状況

| 場所 | 状態 |
| --- | --- |
| `src/test_util.rs` の `real_zai` / `judge_zai` / `zai_gate_at` | 実装済み・テスト済み |
| `src/guard.rs` の `real_zai()` | **未適用**（別エージェントが編集中のため）|
| `src/czero_init.rs` の `built_zai()` | **未適用**（同上）|

`src/guard.rs` と `src/czero_init.rs` は「版だけを見る」旧実装のままなので、
**この 2 つは今もこの事故を素通りさせる**。統合担当が下記の 1 行に置き換えること。

---

## 事故2 — ベンチが能力検査を持っていなかった

`tools/conflict-zero-bench.sh` の段② (guard) は **`zai help` に文字列が
あるかを見るだけ**で、ゲートが 1 件でも実際に止められるかを確かめずに
測っていた。検出は「重なりがあるのに衝突が残った」という**事後判定だけ**なので、
`--overlap 0.0` のように衝突が起きない条件で回すと、ゲートが 1 件も
止めていなくても綺麗な数字が出る。

`tools/coedit-bench.sh` の流儀（exit 20〜23 の事前検査）に揃えた:

| exit | 事前検査で分かること |
| --- | --- |
| 20 | `zai lease enable` が失敗した |
| 21 | `zai lease claim` が失敗した（予約が取れない）|
| 22 | **他人が持っているファイルへの書き込みを通した**（門が無い）|
| 23 | 誰も持っていないファイルへの書き込みを止めた（門が閉じっぱなし）|

検査は**本番と同じ payload の形**でゲートを 2 回叩く。落ちたら
「証明」と言わずに段を skip し、理由を表と JSON の両方へ出す。

---

## 事故3 — 2 つのベンチで `zai` の探索順が逆だった

`conflict-zero-bench.sh` は release→debug、`coedit-bench.sh` は debug→release。
この環境は **release が 0.12.0・debug が 0.14.0** だったので、
**同一セッションで別のバイナリを測っていた**。

### 対策

6 本すべてのベンチが、`# @zai-honesty-begin` 〜 `# @zai-honesty-end` に
挟まれた**1 バイトも違わない共通ブロック**を持つ。

* 探索順は **`ZAIVERN_BIN` → release → debug → PATH** で統一
* 候補は版（`Cargo.toml` と一致）と古さ（ソースより新しい）の両方で篩う
* 落とした候補は**理由を積む**（`zai_note`）。黙って次へ行かない
* **使ったバイナリの絶対パスと版を必ず出力に出す**（`zai_identity`）。
  `conflict-zero-bench.sh` は JSON にも `zai` / `zai_version` / `zai_note` を残す

口約束にしないため、`test_util::zai_gate_tests::全ベンチのzai決定ブロックは1バイトも違わない`
が 6 本を機械照合する。ブロックを直すときは 6 本すべてへ同じ内容を反映すること。

### なぜ関数を共有せずに複製しているのか

`tools/` に共通の被 source ファイルを置くのが素直だが、この作業単位では
新規ファイルの追加が `docs/bench-honesty.md` に限られていた。
**複製を選ぶ代わりに、複製が食い違ったら落ちるテストを置いた**。
共通ファイルへ寄せられるようになったら、上のテストは
「1 本の実体を全員が source していること」の検査へ書き換えればよい。

---

## 事故4 — `| head` を挟むと `$?` はそちらのものになる

終了コードを見る位置にパイプを置かない。見るならパイプを外すか
`${PIPESTATUS[0]}`（bash）を使う。

監査した結果、**ベンチ 6 本の中に「パイプの後ろで `$?` を判定する」誤りは
1 件も無かった**。`| head` / `| tail` / `| grep -c` はすべて
`var=$(... | ...)` の形で**標準出力だけを取り出す**用途で、終了コードは見ていない。

担当外で 1 件だけ疑わしいものが残っている（下記「引き継ぎ」参照）。

---

## 作業中に踏んだ罠（対策に含めた）

### 変数の直後に日本語を置くと macOS の `/bin/sh` が壊れる

```sh
zai_reject "... 例: $_one。\`cargo build --bin zai\` ..."   # ✗
zai_reject "... 例: ${_one}。\`cargo build --bin zai\` ..." # ✓
```

`$_one。` と書くと macOS の `/bin/sh` (bash 3.2) が `。` の**先頭バイトを
変数名に取り込み**、`_one\xe3: unbound variable` で `set -u` が落ちる。

**落ちるのが「古いバイナリを弾く経路だけ」だったのが最悪の性質**で、
普段の実行では一度も通らず、いちばん大事な場面でだけ死ぬ。
実際にこの作業中、古いバイナリを置いた実演で初めて発覚した。
**日本語のメッセージに変数を埋めるときは必ず `${...}` で囲む。**

### `cargo fmt` はクレート全体を整形する

`cargo fmt -- src/test_util.rs` と書いても `--` の後ろは rustfmt への
引数であって対象の限定ではない。**担当外のファイルまで整形されて
diff に入る**（実際に `src/plugins.rs` が混入した）。
自分のファイルだけを整形するなら `rustfmt --edition 2021 src/<file>.rs`。

### `tools/verify.sh --bin` の費用（実測）

| | 1 回目 | 2 回目 |
| --- | --- | --- |
| `cargo test --bin zai --no-run` | 44 s | 94 s |
| そのあと `cargo build --bin zai` | **+6 s** | **+22 s** |

3 倍ばらついたのは同居している他のビルドと取り合ったため。
どちらも `Compiling` 行とバイナリの mtime が動いたことを確認済みで、
**空振りではない**（「速すぎる＝走っていない」を疑って測り直した）。

---

## 引き継ぎ（担当外なので触れなかったもの）

### 1. `src/guard.rs` / `src/czero_init.rs` へ関所を通す（各 1 行）

どちらも「版だけを見る」旧実装なので、事故1をいま**素通りさせる**。

* `src/guard.rs` の `fn real_zai() -> Option<PathBuf>`（本文を置き換える）

  ```rust
  fn real_zai() -> Option<PathBuf> {
      crate::test_util::real_zai("実フック試験")
  }
  ```

* `src/czero_init.rs` の `fn built_zai() -> Option<PathBuf>`（同じく本文を置き換える）

  ```rust
  fn built_zai() -> Option<PathBuf> {
      crate::test_util::real_zai("実バイナリでのマージ試験")
  }
  ```

置き換えたあと、両ファイルの `use std::process::Command;` などが
不要になっていないか（`never used` 警告）を確認すること。

### 2. `tools/windows-check.sh:226-230` — 関数の返り値がパイプ末尾のもの

```sh
CARGO_TARGET_DIR="$target" cargo xwin test --bin zai --target "$triple" \
    --no-run --message-format=json 2>/dev/null \
    | grep -o '"executable":"[^"]*\.exe"' \
    | tail -1 \
    | sed 's/^"executable":"//; s/"$//'
```

**cargo が失敗しても `sed` は成功する**ので、この関数は 0 を返して
「実行ファイルが見つからなかった」を「空文字列で成功」として伝える。
`tools/windows-check.sh` は担当外なので直していない。

### 3. `tools/verify.sh --lint` を塞いでいる 3 件（すべて HEAD 由来・担当外）

| 場所 | 症状 | 直し方 |
| --- | --- | --- |
| `src/plugins.rs:1365` | `cargo fmt --all --check` が落ちる | `plugin_data_dir` の 3 行を 1 行へ（rustfmt の出力どおり）|
| `src/negotiate.rs:2611` | clippy `needless_update` | `..crate::feature::Feature::DEFAULT` の行を消す |
| `src/union.rs:2859` | clippy `needless_update` | 同上 |

`e5879a6` 以降ずっとこの状態で、**誰の変更とも無関係に
`tools/verify.sh --lint` が最初の段で赤くなる**。
この 3 件を一時的に当てた状態では、clippy も警告もテストも全部緑だった
（証跡はコミットメッセージ参照）。担当外なので恒久的な修正は入れていない。

---

## ベンチを 1 本足すときの決まり

1. `# @zai-honesty-begin` 〜 `# @zai-honesty-end` の共通ブロックを**そのまま**貼る
   （`$root` と `$target_dir` を先に決めておく）
2. その計測に要る機能の適格判定を関数として書き、`zai_pick <関数名>` を呼ぶ
3. `zai_identity "$zai"` を出力の先頭付近へ入れる
4. **測る前に「守りたい性質が実際に働くか」を検査する**。
   検査に落ちたら「証明」と言わずに理由を出して降りる
5. 検査の終了コードは 20 番台に割り当て、`case` で人間の言葉へ写す
