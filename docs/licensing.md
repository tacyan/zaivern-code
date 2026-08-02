# ライセンス認証 — 完全オフライン (通信ゼロ)

Zaivern Code の Pro ライセンスは **Ed25519 の公開鍵署名**で検証する。
アプリは認証サーバへ問い合わせない。起動時も適用時もパケットは 1 バイトも出ない。

- 検証の実装: `src/license.rs`
- キーの発行ツール: `tools/licgen/` (**独立クレート**。`zai` バイナリには含まれない)
- 到達経路: コマンドパレット (⌘⇧P) → 「ライセンスキーを入力…」

---

## 1. 仕組み

```text
ZVL1.<Base64URL_nopad(payload_json)>.<Base64URL_nopad(ed25519_signature 64B)>
^^^^ 形式バージョン
```

- 署名対象は `"ZVL1." + <payload セグメントの ASCII 文字列>`。
  Base64 を解いた結果ではなく**綴じられた文字列**に署名するので、
  パディングや再エンコードの揺れが検証に影響しない。
- ペイロード (JSON):

  | キー | 型 | 意味 |
  |---|---|---|
  | `sub` | string | 購入者の識別子 (メールアドレス・購入 ID など) |
  | `tier` | string | 等級。`"pro"` など。`"free"` は Pro 扱いにならない |
  | `iat` | number | 発行時刻 (Unix 秒)。**検証には使わない** (端末の時計ずれで正規キーを弾かないため) |
  | `exp` | number \| null | 失効時刻 (Unix 秒)。`null` は無期限 |
  | `seats` | number | 座席数 (表示のみ。オフラインでは強制できない) |

- 知らないキーは無視するので、後から項目を足しても古いアプリは壊れない。
- 空白・改行は貼り付け時に落とす。接頭辞の大文字小文字は問わない。

### 保存場所

`~/.zaivern/license.key` (パスは `config::zaivern_dir()` 由来 = `dirs::home_dir()`)。

- unix: 保存時に `0600` を試みる (失敗しても続行)。
- Windows: `%USERPROFILE%\.zaivern` の既定 ACL が既に本人限定なので、権限操作はしない。

---

## 2. できないこと — **失効 (revoke) できない**

オフライン検証は「発行済みの署名が数学的に正しいか」しか判定できない。
サーバに問い合わせないので、**一度発行したキーを後から無効化する手段は無い**。
返金・不正共有・鍵の流出のいずれにも、事後には対処できない。

対処は 1 つだけ:

> **期限付き (`exp`) で発行する。**

`--never` (無期限) で発行できるようにはしてあるが、原則使わないこと。
無期限キーが 1 本流出したら、その鍵ペアで発行した全キーを捨てて
公開鍵を差し替える (= 全ユーザーの再発行) 以外に手が無い。

この限界はアクティベーション画面にも明記してある (隠さない)。

---

## 3. 鍵ペアの作り方 (販売者が 1 回だけ行う)

```sh
cargo run --manifest-path tools/licgen/Cargo.toml -- \
    keygen --out ~/zaivern-licgen.secret
```

出力:

```text
秘密鍵: /Users/you/zaivern-licgen.secret  ← リポジトリの外の安全な場所へ。失うと再発行できません
公開鍵 (ZAIVERN_LICENSE_PUBKEY): 3a7f…(16 進 64 桁)
```

### 秘密鍵の扱い

- **リポジトリに置かない。** `.gitignore` が `*.pem` / `*.key` / `*_secret*` /
  `*.secret` / `licgen_key*` を弾くが、そもそも別の場所 (パスワードマネージャ・
  ハードウェアトークン・オフラインの暗号化ボリューム) に保管する。
- **失うと再発行できない。** 公開鍵を差し替えることになり、発行済みキーが全部無効になる。
- **リリースバイナリには入らない。** `tools/licgen` は空の `[workspace]` を持つ
  独立クレートなので、リポジトリ直下の `cargo build --release` はビルドしない。

確認方法 (秘密鍵が出荷物に混ざっていないこと):

```sh
cargo build --release
strings target/release/zai | grep -i "BEGIN.*PRIVATE"   # 何も出なければ OK
```

---

## 4. 公開鍵をビルドへ埋め込む

公開鍵は**ビルド時の環境変数**から埋め込む (`option_env!`)。

```sh
ZAIVERN_LICENSE_PUBKEY=3a7f…64桁 cargo build --release
```

- 環境変数が無いビルドでは公開鍵が全ゼロの番兵になり、
  **どのキーも有効にならない** (開発ビルドが誤って Pro を解錠しない)。
  アクティベーション画面にもその旨が出る。
- 16 進が壊れていれば**コンパイルエラー**になる (const 評価中の panic)。
  打ち間違えた公開鍵で出荷することが構造的に起きない。
- 公開鍵は秘密ではないので値をコミットしても害は無いが、鍵ペアを作れるのは
  販売者だけなので、リポジトリには番兵だけを置いている。

---

## 5. キーを発行する

```sh
cargo run --manifest-path tools/licgen/Cargo.toml -- issue \
    --secret ~/zaivern-licgen.secret \
    --sub buyer@example.com \
    --tier pro \
    --days 365 \
    --seats 1
```

標準出力にキーが 1 行だけ出る (そのまま購入者へ渡せる)。
`issue` は出力前に**自分で署名を検証する**ので、壊れたキーを配ることはない。

| オプション | 既定 | 意味 |
|---|---|---|
| `--sub <文字列>` | (必須) | 購入者の識別子 |
| `--tier <名前>` | `pro` | 等級。`free` は Pro 扱いにならない |
| `--days <日数>` | `365` | 今日から N 日で失効 |
| `--exp <unix秒>` | — | 失効時刻を直接指定 (`--days` より優先) |
| `--never` | — | 無期限。**失効できない**ので原則使わない (警告が出る) |
| `--seats <人数>` | `1` | 座席数 (表示のみ) |

その他:

```sh
# 秘密鍵から公開鍵を出し直す
licgen pubkey --secret ~/zaivern-licgen.secret

# 発行したキーを検証側と同じ手順で確かめる
licgen verify --pubkey <hex64> --key "ZVL1.…"
```

---

## 6. 出荷前の自己点検

発行ツールと検証側は**別クレート**で、Base64 の設定を意図的に重複させている
(共有すると本体の `crate::config` まで引き込んでしまい、分離の意味が消えるため)。
ズレを検出する仕掛けを 2 つ用意してある。

1. `src/license.rs` の `format_is_stable_across_issuer`
   — Base64URL・パディング無しの綴じ方を固定ベクタで止める。常に走る。
2. `src/license.rs` の `issued_key_from_licgen_verifies`
   — **本物の発行キー**を検証側に通す。既定では何もせず、環境変数を渡したときだけ走る。

```sh
PK=$(cargo run -q --manifest-path tools/licgen/Cargo.toml -- pubkey --secret ~/zaivern-licgen.secret)
KEY=$(cargo run -q --manifest-path tools/licgen/Cargo.toml -- issue \
        --secret ~/zaivern-licgen.secret --sub qa@example.com --days 1)

ZAIVERN_LICENSE_PUBKEY="$PK" ZAIVERN_TEST_LICENSE_KEY="$KEY" \
    cargo test license::
```

---

## 7. 機能ゲートの方針

- **未ライセンスでもアプリは完全に動く。** 既存の無料機能は 1 つもゲートしない。
  ライセンスは Pro 機能を「解錠する」方向にのみ働く。
- 判定は `license::is_pro(&LicenseStatus)` の **1 関数だけ**を通す。
  判定を各所へ散らすと「片方だけ直し忘れる」が必ず起きる。
- 現在 `is_pro` を見ているのはステータスバーの `✨ Pro` バッジ 1 か所だけ
  (未ライセンス時は 1 ピクセルも出さない)。

## 8. UI

コマンドパレット → **「ライセンスキーを入力…」** でダイアログが開く。

- 現在の状態: 未ライセンス / 有効 (購入者・等級・期限・座席) / 期限切れ / 形式不正 / 署名不正
- 保存済みキーは**先頭 6 文字…末尾 4 文字**に伏せて表示する (全体は出さない)
- 「通信ゼロ」「失効できない」「無料で全機能使える」を画面に明記する
- 「購入する」ボタンは**まだ無い** (販売 URL が未確定のため)
