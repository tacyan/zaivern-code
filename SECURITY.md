# セキュリティポリシー

## 脆弱性の報告

**GitHub の非公開報告を使ってください** —
[Security → Report a vulnerability](https://github.com/tacyan/zaivern-code/security/advisories/new)

公開 Issue には書かないでください。修正版を出す前に手口が広まります。

- 一次応答: 3 営業日以内
- 修正版の目標: 深刻度 High 以上は 14 日以内
- 修正の公開時に、希望があれば謝辞へ記載します

## 対象バージョン

最新のリリースのみを支援します。古い版へのバックポートは行いません
（単一バイナリ配布で、`zai update` を実行すればその場で最新になるため）。

## 配布物の検証

リリースには `checksums.txt`・SBOM・ビルド来歴 (provenance) が付きます。

### 1. SHA-256（`install.sh` / `install.ps1` は自動でこれを行う）

ワンライナーのインストーラは、**展開する前に** `checksums.txt` と突き合わせます。
一致しなければ展開も実行もせずに中止します（fail-closed）。手で確かめる場合:

```sh
# macOS / Linux
curl -fsSLO https://github.com/tacyan/zaivern-code/releases/download/<tag>/checksums.txt
curl -fsSLO https://github.com/tacyan/zaivern-code/releases/download/<tag>/zai-<tag>-macos-arm64.tar.gz
shasum -a 256 -c checksums.txt --ignore-missing
```

```powershell
# Windows
(Get-FileHash -Algorithm SHA256 .\zai-<tag>-windows-x86_64.zip).Hash.ToLower()
# checksums.txt の該当行と一致することを確認する
```

### 2. ビルド来歴（provenance）

「この配布物は、このリポジトリの、このコミットから、GitHub のランナー上で
作られた」ことを Sigstore の署名付きで確認できます。

```sh
gh attestation verify zai-<tag>-macos-arm64.tar.gz --repo tacyan/zaivern-code
```

### 3. SBOM

`zai-<tag>-sbom.cdx.json`（CycloneDX 1.5）に、その版が取り込んでいる
全依存が入っています。新しい脆弱性が公表されたとき、こちらの発表を待たずに
影響の有無を判定できます。

### 4. コード署名について（現状）

**macOS の Developer ID 署名 / 公証、Windows の Authenticode 署名は
まだ有効になっていません。** ワークフロー側の受け口は用意してあり、
証明書を secrets に入れた時点で有効になります（`docs/signing.md`）。
リリースごとに、署名されたかどうかはワークフローのサマリに必ず出ます。

そのため現時点では、ブラウザでダウンロードした場合に macOS Gatekeeper /
Windows SmartScreen の警告が出ることがあります。上記 1〜3 で身元を確認できます。

## 供給網の防御（このリポジトリが実際にやっていること）

| 対象 | 手段 | 場所 |
| --- | --- | --- |
| 配布物の改竄 | 展開前の SHA-256 照合（fail-closed） | `install.sh` / `install.ps1` |
| ビルドの出所 | SLSA build provenance attestation | `.github/workflows/release.yml` |
| 依存の脆弱性 | `cargo audit` + `cargo deny`（週次 + PR） | `.github/workflows/security.yml` |
| 依存の素性 | crates.io 以外からの取得を禁止 | `deny.toml` (`sources`) |
| ライセンス | 許可リスト方式 | `deny.toml` (`licenses`) |
| Actions の改竄 | 全 `uses:` をコミット SHA へ固定 | 全ワークフロー |
| Actions の権限 | 既定ゼロ + ジョブ単位で最小付与 | 全ワークフロー |
| Actions の設定ミス | CodeQL (`actions`) | `.github/workflows/security.yml` |
| 依存の陳腐化 | Dependabot（cargo / github-actions） | `.github/dependabot.yml` |
| 秘密の混入 | Secret scanning + push protection | リポジトリ設定 |

## アプリ自身の攻撃面

Zaivern Code は GUI・HTTP サーバ・PTY・Git・シェル・ファイル書き換え・SSH・
ライセンス処理を持ちます。通常のデスクトップアプリより面が広いことを前提に:

- **スマホリモート (`remote.rs`, TCP 8899〜)** は既定で起動しません。
  有効にしたときだけ待ち受け、Windows ではファイアウォール規則を
  明示的に作らない限り外から届きません（`zai firewall allow`）。
- **承認 (approval)** はネイティブ UI のキューと永続ポリシー、監査ログを持ちます。
  エージェントへバイパスフラグを注入する方式は採っていません。
- **ライセンス署名の検証は Ed25519 の検証のみ**（秘密鍵はバイナリに入りません）。
