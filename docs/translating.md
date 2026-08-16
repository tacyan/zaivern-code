# 翻訳の手引き — Zaivern Code を自分の言語にする

この文書は**訳す人**のためのものです。仕組みそのものは
[`docs/i18n.md`](i18n.md)、ファイルの形式は
[`locales/README.md`](../locales/README.md) にあります。

**Rust を書けなくても、アプリを再ビルドできなくても訳せます。**
JSON を 1 枚置くだけです。

---

## 0. 3 分で始める

```sh
zai lang export fr                 # ~/.zaivern/locales/fr.json に雛形が出る
$EDITOR ~/.zaivern/locales/fr.json # 訳す (全部でなくてよい)
zai lang check fr                  # 過不足とプレースホルダを検査
zai lang set fr                    # 切り替える
```

GUI からも同じことができます（`⌘⇧P` → 「表示言語: 翻訳の雛形を書き出す」）。

**訳が足りないぶんは英語で出ます。** 1 行だけ訳したファイルでも今日から使えます。

---

## 1. どれを訳すのか

`locales/<言語ID>.json` は **安定 ID → 訳文** の平の JSON です。

```json
{
  "app.new_session": "New Session",
  "agent.approve_all": "Approve All",
  "cli.lang_install_hint": "  使うには: zai lang set {id}"
}
```

| ファイル | 役割 |
|---|---|
| `en.json` | **基準**。訳が欠けたらここへ落ちる |
| `ja.json` | **原文**。コード自身が書かれている言語 |
| `zh-CN` / `ko` / `pt-BR` / `es` | 同梱の訳 |

**`ja.json` の値は書き換えないでください。** これは「原文」で、
既存 3,300 か所の呼び出しとの橋渡しに使われています（[`docs/i18n.md`](i18n.md) §4）。

### 言語 ID の付け方

`en` / `ja` / `fr` のような**言語コード**か、地域まで要るなら `pt-BR` / `zh-CN`。
ファイル名がそのまま ID になります。`zh_CN.json` と書いても `zh-CN` に正規化されます。

---

## 2. 訳すときの規則

### ① `{name}` は 1 文字も変えない

```json
"{n} 件を {dir} へ保存しました"   →   "Saved {n} files to {dir}"
```

**語順は変えてよい。プレースホルダを消す・増やす・綴りを変えるのは禁止。**
実行時にそこへ値が入るので、消すと**情報が画面から消えます**。

`zai lang check` はこれを検査して、合わなければ**終了コード 1 で落ちます**。

位置指定の `{}` もあります（`"{} を作成できません: {e}"`）。
これは**順番どおりに**値が入るので、順序を入れ替えないでください。

### ② 先頭・末尾の絵文字・記号・空白・改行を保つ

```json
"📣 全エージェントへブロードキャスト"  →  "📣 Broadcast to All Agents"
"  … ほか {n} 件"                    →  "  … {n} more"     ← 先頭の 2 スペースも残す
```

改行 `\n` は**本数と位置**を保ってください。画面の折り返しに合わせてあります。

### ③ 訳さないもの

- コマンド・フラグ・パス: `zai lang install` / `--force` / `~/.zaivern/locales` / `config.toml`
- 製品名: `Zaivern` / `git` / `worktree` / `LSP` / `PTY` / `MCP` / `Tailscale` / `Claude Code` / `Codex`
- 打鍵表記: `⌘⇧C` / `Ctrl+K` / `Esc` / `Enter`
- 環境変数: `ZAIVERN_I18N_TRACE=1`

### ④ ボタンとメニューは短く

原文に句点が無ければ、訳にも付けません。
`✅ 承認` は 1 語（`Approve` / `승인` / `Aprobar`）。

### ⑤ 用語を 1 つに決める

同じ概念に 2 つの訳語を使わないでください。特に:

| 日本語 | 何を指すか |
|---|---|
| エージェント | Claude Code などの CLI そのもの |
| 承認 / 拒否 | エージェントの操作を通す / 止める |
| リース / 担当 | 行域の所有 (`lease` は原語のままの言語もある) |
| 看板 / デッキ / Cockpit | 画面の名前。既存訳に合わせる |

迷ったら**既にある訳を grep してください**。

```sh
grep -n '"agent' locales/en.json | head
```

### ⑥ `_` で始まる鍵は無視されます

翻訳者どうしの申し送りに使えます。

```json
{ "_note": "「担当」は lease と訳し分けないこと", "lease.list": "…" }
```

---

## 3. やってはいけないこと（実際に踏んだ罠）

### 同じ原文に 2 つの訳を付けようとしない

`tr("✅ 承認")` は**ボタン**からも**タブ名**からも呼ばれます。
呼び出し側が同じ文字列を渡す以上、**実行時に区別できません**。
だから辞書に訳を 2 つ持つことは**原理的に不可能**です。

分けたいときは**原文の側を分ける**（`✅ 承認` / `✅ 承認キュー`）ので、
それは Rust を触れる人の仕事です。**Issue で報告してください。**

番人テスト `locale::tests::同じ原文を持つidは訳も一致する` が、
同じ原文に違う訳が入るのを禁じています。

### 状態と操作を同じ語にしない

`追加` (Add) と `追加済み` (Added) は別物です。日本語側で分けてあるので、
訳でも分けてください（`Add` / `Added`）。

### 画面の名前を勝手に作らない

看板のレーン名（`承認待ち` など）は**サーバが配る名前**で、
画面側で作り直すことを番人が禁じています。訳語だけを変えてください。

---

## 4. 検査する

```sh
zai lang check fr        # 入っている fr.json を検査
zai lang check ./fr.json # ファイルを直接検査
```

出るもの:

| 見出し | 意味 | 直す? |
|---|---|---|
| 訳が無い | 基準にあるのに未訳 | **任意**（英語で出る） |
| 基準に無い鍵 | 綴り間違い / 古い鍵 | **直す** |
| プレースホルダ不一致 | `{name}` が基準と違う | **必ず直す** |
| 空の訳 | 値が空白だけ | **必ず直す** |

**1 つでも残っていれば終了コード 1** です（fail-closed）。

### 訳漏れを画面から拾う

静的には辿れない文字列（表に置かれた状態ラベルなど）があります。
実行時に集めてください。

```sh
ZAIVERN_I18N_TRACE=1 zai .
# 画面をひととおり触ってから
# ⌘⇧P → 「表示言語: 訳が無い文字列を書き出す」
```

`~/.zaivern/locales/missing-<id>.json` に出ます。

---

## 5. 配る

### 自分のリポジトリで配る

`locales/<id>.json` を置いた GitHub リポジトリを公開するだけです。

```sh
zai lang install fr --from あなた/zaivern-lang-fr
zai lang install fr --from あなた/zaivern-lang-fr --ref preview   # ブランチ指定
```

**配布元は決め打ちされていません。** 既定はこのビルドの配布元で、
`ZAIVERN_LANG_REPO` でも差し替えられます。

`install` は**置く前に検査**します（JSON の形 / 空でないこと /
プレースホルダの一致 / 既存を黙って上書きしない）。
書き込みは一時ファイル + 差し替えなので、途中で切れても半端が残りません。

### プラグインとして配る

```toml
# plugin.toml
[plugin]
name = "french-mode"
version = "1.0.0"
default_enabled = false      # 入れただけで言語が変わるのは驚きなので false

[language]
id = "fr"
name = "Français"
locales = "locales"          # このディレクトリ直下に fr.json を置く
```

`.zvplug` 1 個で配れます。

### 本体へ入れてもらう

`locales/<id>.json` を足す Pull Request を送ってください。
入れる条件は 1 つだけ:

```sh
zai lang check <id>   # ✅ 過不足なし
```

同梱言語になると、**バイナリに埋め込まれて**追加ファイル無しで全員が使えます。

---

## 6. 同梱の訳を直したい

**同じ ID を書いたファイルを置くだけで上書きできます。** 再ビルドは要りません。

```json
// ~/.zaivern/locales/en.json — この 1 行だけ
{ "app.settings": "Preferences" }
```

残りは同梱のまま使われます。直した結果が良ければ、その 1 行の
Pull Request を送ってください。

置き場は 2 か所あり、**先に来たほうが勝ちます**:

1. `~/.zaivern/locales/<id>.json`
2. `~/.config/zaivern/locales/<id>.json`（Windows は `%APPDATA%\zaivern\locales`）

---

## 7. 開発者向け — 文字列を足したとき

```sh
zai lang missing                    # 画面に出るのに辞書へ無いものを出す
zai lang missing src /tmp/d.json    # 翻訳へ回せる形で書き出す
zai lang apply /tmp/d-translated.json
zai lang check ja
```

新しいコードは**安定 ID** を渡してください（`tr("agent.approve")`）。
原文を直しても訳が生き残ります。

番人テストが落ちたら、**直すのは `locales/*.json` であってテストではありません**。

| テスト | 守るもの |
|---|---|
| `locale::tests::全同梱言語のキー集合が一致する` | 6 枚の鍵が完全に同じ |
| `locale::tests::訳は空でなくプレースホルダも一致する` | `{name}` が全言語で同じ集合 |
| `locale::tests::ソースのtrリテラルはすべて同梱辞書から引ける` | **画面に日本語が残らない** |
| `locale::tests::同じ原文を持つidは訳も一致する` | 逆引きの勝者が変わっても表示が変わらない |
| `tutorial::tests::チュートリアルの全文言が辞書にある` ほか 2 本 | 案内文が漏れない |
| `remote::tests::同梱の全言語でスマホ画面の文言が入る` | スマホも同じ言語になる |

---

## 8. 困ったら

- 仕組みを知りたい → [`docs/i18n.md`](i18n.md)
- ファイルの形式だけ知りたい → [`locales/README.md`](../locales/README.md)
- 「同じ原文なのに訳し分けたい」→ **Issue で報告**（原文の側を分ける必要があります）
- 訳が反映されない → `zai lang` で**いまどの言語が効いているか**を確認。
  言語パックのプラグインは**同時に 1 つだけ**有効になります
