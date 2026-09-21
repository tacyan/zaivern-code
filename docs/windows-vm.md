# Windows 検証 VM の起動・再利用

2026-09-21 のズーム不具合検証に使ったローカル VM。仮想ディスクは一時フォルダから保存済みで、再インストールは不要。

## 起動

この VM を作成した macOS ホストで、メインのリポジトリルートから実行する（隔離ワークツリー内ではない）。

```sh
.Codex/vms/windows10-repro/start-vm.sh
```

QEMU のウィンドウに Windows が表示される。ターミナルは起動中そのままにしておく。Windows のスタートメニューから「電源 → シャットダウン」で終了する。ウィンドウを閉じる、Ctrl+C を押すなどの強制終了は通常の終了方法として使わない。同じ仮想ディスクを二重起動しない。

画面を開かず自動検証する場合:

```sh
.Codex/vms/windows10-repro/start-vm.sh --headless
```

ローカルアカウントは `Repro`。自動ログインされない場合のパスワードは VM フォルダの `credentials.txt` に保存してある（所有者だけが読める設定）。公開・コミットしない。

## アプリの起動

Windows の PowerShell で:

```powershell
$env:ZAIVERN_HOME = 'C:\repro\home-fixed'
$env:GALLIUM_DRIVER = 'llvmpipe'
& 'C:\repro\fixed-app\zai.exe' 'C:\repro\workspace'
```

修正前の v0.24.5 は `C:\repro\app\zai.exe`、比較用設定は `C:\repro\home-baseline` と `C:\repro\home-large` に残している。検証時は専用の `ZAIVERN_HOME` を指定する。

## 構成と保存先

- 保存先: メインリポジトリの `.Codex/vms/windows10-repro/`（Git 対象外）。ディスク、起動スクリプト、検証用スクリプト、画面キャプチャを保存。
- ホスト: Intel Mac、QEMU 11.0.1、Hypervisor.framework（`-machine q35,accel=hvf`）。このスクリプトは同じホスト向け。Apple Silicon への移植は未検証。
- CPU: `qemu64,-svm`、4 vCPU。メモリ: 8 GiB。
- OS: Windows 10 Pro 日本語、22H2、ビルド 19045。
- 仮想ディスク: `windows-standard-cpu.qcow2`、仮想容量 64 GB、保存時の実使用量約 12 GB。
- 表示: 1920×1200、Windows スケーリング 150%（アプリで DPI 144 を確認）。
- 描画: アプリ同梱の Mesa llvmpipe（ソフトウェア OpenGL）。実 GPU の検証ではない。
- ネットワーク: `-nic none`。ゲストに NIC を接続していない。
- インストール元: ホストの `~/Downloads/Win10_22H2_Japanese_x64v1.iso`。インストール済みなので通常起動には不要。
- 起動時の CD: `labels.iso`（修正済みアプリ、ボリューム名 `ZAI_LABEL`）。インストーラや無人インストール用の秘密情報はマウントしない。

この環境では `-cpu host` でインストーラが異常終了し、上記 CPU 設定でインストール・検証できた。ほかの環境でも同じ現象になるとは限らない。

## 自動操作とビルドの差し替え

ローカルの `qmp.py` は QMP ソケット経由のスクリーンショット・キー入力、`guest.py` はシリアル経由の PowerShell 実行に使った作業用スクリプト。保守対象のリポジトリツールではない。Windows 起動後、シリアル制御エージェントの起動を待って使う。`guest.py` を同時実行しない。

```sh
vm_dir="$PWD/.Codex/vms/windows10-repro"
python3 "$vm_dir/qmp.py" query-status
python3 "$vm_dir/qmp.py" shot current.png
python3 "$vm_dir/guest.py" "$vm_dir/bootstrap-control.ps1"
```

`bootstrap-control.ps1` は GUI 操作用の型をゲスト PowerShell に定義する。VM 再起動後は再実行が必要。`shutdown.ps1` はこの初期化と検証アプリの起動を前提にするので、通常は Windows の電源メニューから終了する。

新しい実行ファイルは `tools/windows-check.sh --build` で作成し、読み取り専用 ISO に入れてゲストへ渡す。VM 停止中に `labels.iso` を同名の新しい ISO に置き換える場合、ボリューム名を `ZAI_LABEL` にする。ゲストのアプリを終了してから `deploy-labels.ps1` を `guest.py` で実行すると、CD から `C:\repro\fixed-app\zai.exe` へコピーし起動できる。ビルド成果物の保存先は `ZAIVERN_WINDOWS_TARGET` で分離できる。

## 今回の再現・確認結果

修正前では全体ズーム 50% と文字倍率 300% の組み合わせで、メニューなどだけが大きい状態を再現。文字倍率を 100% に戻すと設定は保存されるが、再起動まで表示が戻らなかった。修正後は同じプロセスのまま表示が戻ることを Windows 上で確認した。元の報告者の PC の設定は未取得のため、同じ原因だったとは断定しない。

JIS 配列と US 配列の両方で、全体ズームが `Ctrl+0` → 100%、`Ctrl+Shift+-` → 90%、各配列の `Ctrl+Shift+プラスのキー` → 100% となることを確認。Windows の表示メニューにも `Ctrl+Shift++` / `Ctrl+Shift+-` が表示されることを確認した。

- 修正ブランチ: `fix/windows-text-zoom`、土台コミット: `2889e9f`。
- メニュー表示まで検証した実行ファイル SHA-256: `2990B98E00B2EB3B3B587D7999DFB0F5B3DB19C452E677D056EC6A34C895638E`。
- 証跡: VM フォルダの `large-menu.png`、`after-text-reset.png`、`fixed-after-reset.png`、`fixed-jis-minus.png`、`fixed-us-minus.png`、`fixed-shortcut-labels.png`。詳細の作業記録は `reproduction.md`。
- Windows 11、実 GPU、元の報告者の PC は未確認。

回帰テストは `keybinds::zoom_shortcut_regression_tests` の 3 件と `app::frame_update::text_scale_sync_regression_tests` の 1 件。既存の `.github/workflows/test.yml` の Linux / macOS / Windows マトリクスに自動的に含まれる。CI と同じ nextest プロファイル・除外条件で追加 4 件のローカル成功を確認した。GitHub Actions のリモート実行は未実施。

```sh
cargo nextest run --locked --profile ci -E 'not test(/^terminal::[a-z0-9_]*pty[a-z0-9_]*::/) and (test(zoom_shortcut_regression_tests) or test(text_scale_sync_regression_tests))'
```

関連検証は macOS のキー関連 85 件と文字倍率 1 件、Linux のキー関連 84 件（最終テスト修正後に追加 3 件も再実行）と文字倍率 1 件、Windows のクロスビルドが成功。クロスビルドと上記 Windows GUI の実行確認は別々に実施した。
