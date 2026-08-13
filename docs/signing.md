# コード署名を有効にする手順

`.github/workflows/release.yml` には署名の受け口が入っている。
**secrets が未設定のあいだは丸ごと skip される**ので、今のリリースは壊れない。
証明書を入れた瞬間に有効になり、署名されたかどうかは毎リリースの
ワークフローサマリ（`Signing report`）に必ず出る。

> ⚠ この文書に書かれた手順は **まだ一度も実行されていない**。
> 証明書が無いと最後まで通せないため、初回は必ず捨てタグ（例 `v0.0.0-signtest`）で
> 一度回して確かめること。

---

## なぜ要るのか

| 無いと起きること | 影響を受ける人 |
| --- | --- |
| macOS: 「開発元を確認できないため開けません」 | ブラウザからダウンロードした利用者 |
| Windows: SmartScreen の「WindowsによってPCが保護されました」 | 同上 |
| 企業端末の実行ブロック（AppLocker / WDAC / MDM） | 法人利用の全員 |

`curl \| sh` 経由の導入は quarantine 属性が付かないため警告が出にくいが、
**GUI アプリとして配るなら署名は避けて通れない**。有償販売・法人導入を
考えるなら、チェックサム検証の次に優先度が高い。

---

## macOS（Developer ID + 公証）

### 必要なもの

- Apple Developer Program（年 US$99）
- **Developer ID Application** 証明書（`Mac Developer` ではない。種類を間違えると公証で弾かれる）
- 公証用の App Store Connect **App-Specific Password**

### secrets

| 名前 | 中身 |
| --- | --- |
| `MACOS_CERT_P12` | 証明書 + 秘密鍵を `.p12` で書き出し、**base64 にしたもの** |
| `MACOS_CERT_PASSWORD` | その `.p12` のパスワード |
| `MACOS_SIGN_IDENTITY` | `Developer ID Application: Your Name (TEAMID)` |
| `MACOS_NOTARY_APPLE_ID` | Apple ID（メールアドレス） |
| `MACOS_NOTARY_TEAM_ID` | 10 桁の Team ID |
| `MACOS_NOTARY_PASSWORD` | App-Specific Password |

`.p12` の作り方（キーチェーンアクセスから書き出したあと）:

```sh
base64 -i DeveloperID.p12 | pbcopy   # これを MACOS_CERT_P12 に貼る
```

### ワークフローがやること

1. 専用の一時キーチェーンを作る（ランナーの login キーチェーンを汚さない）
2. `codesign --force --timestamp --options runtime`
   — Hardened Runtime とタイムスタンプは**公証の必須条件**
3. `codesign --verify --strict` で自己検証
4. `ditto -c -k` で zip に包み `xcrun notarytool submit --wait`
   — notarytool は `.zip` / `.pkg` / `.dmg` しか受け取らない（`.tar.gz` は通らない）
5. 一時キーチェーンを消す

### staple しない理由

`xcrun stapler` はバンドル（`.app` / `.dmg` / `.pkg`）にしかチケットを貼れない。
配布物は単体の実行ファイルなので、公証チケットは Gatekeeper がオンラインで
照会する。将来 `.app` バンドルを配るようになったら staple を足すこと。

---

## Windows（Authenticode）

### 必要なもの

2023 年 6 月以降、コード署名証明書の秘密鍵は **FIPS 140-2 レベル 2 以上の
ハードウェア**に置くことが必須になった。したがって選択肢は実質 2 つ:

1. **Azure Trusted Signing**（月額。CI から使いやすく、いちばん安い）
2. **クラウド HSM 付きの OV/EV 証明書**（DigiCert KeyLocker 等）

「PFX ファイルをそのまま secrets に置く」は、**もう新規発行では選べない**。
下の `WINDOWS_CERT_PFX` 経路は、既存の PFX を持っている場合や
社内 CA を使う場合のためのもの。

### secrets（PFX 経路）

| 名前 | 中身 |
| --- | --- |
| `WINDOWS_CERT_PFX` | `.pfx` を base64 にしたもの |
| `WINDOWS_CERT_PASSWORD` | その `.pfx` のパスワード |

```powershell
[Convert]::ToBase64String([IO.File]::ReadAllBytes("cert.pfx")) | Set-Clipboard
```

### Azure Trusted Signing へ移す場合

`Sign (Windows Authenticode)` ステップを
`azure/trusted-signing-action`（要 SHA 固定）へ差し替え、
secrets を `AZURE_TENANT_ID` / `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET` /
`AZURE_ENDPOINT` / `AZURE_CODE_SIGNING_NAME` / `AZURE_CERT_PROFILE_NAME` にする。
**そのときも `uses:` はコミット SHA へ固定すること。**

---

## Linux

OS 側に統一された署名の仕組みが無い。配布物の身元は
**SLSA build provenance attestation**（`gh attestation verify`）で担保する。
これは 3 OS すべてに既に入っており、証明書を 1 円も買わずに今日から効いている。

将来 `.deb` / `.rpm` を配るなら、そのときリポジトリ鍵（`dpkg-sig` / `rpm --addsign`）を
用意する。単一バイナリ配布のあいだは不要。

---

## 有効化したあとに必ず確かめること

```sh
# macOS: 署名と公証
codesign --verify --strict --verbose=2 ./zai
spctl --assess --type execute --verbose ./zai   # "accepted" になること

# Windows
signtool verify /pa /v .\zai.exe

# 3 OS 共通: 来歴
gh attestation verify zai-<tag>-<label>.tar.gz --repo tacyan/zaivern-code
```

そして **`Signing report` が「未署名」を出していないこと**を確認する。
サマリは secrets が消えた日に静かに無署名へ戻るのを検出するためにある。
