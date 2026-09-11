# Windows 更新・スマホ接続・小画面 UI の統合検証

2026-09-11、Windows x64、基点 `d78defa`、統合ブランチ `fix/compact-toolbar`。
更新用ワークツリーの変更を保全したまま、このブランチへ取り込んだ。

## 最終的な表示

- Cockpit、看板、デッキ、変更、ペット、スマホ接続、音声入力、配色、言語は常時表示。
- 狭い画面では2段に分ける。8個のメニューは「メニュー」にまとめる。最終指定に合わせて「もっと見る」は撤去し、承認モードも常時表示する。
- ブランチ名は最大14文字に省略し、全名をホバーで表示。極端に狭い領域では横スクロールで各操作へ移動できる。
- エージェント追加メニューとスマホ接続ウィンドウを縦スクロール可能にした。
- スマホ接続ウィンドウは追加指定に合わせてアプリの表示領域全体へ広げた。固定幅を撤去し、タイトル・閉じるボタンを含めて画面内に収め、縦スクロールを維持する。
- Norton の検索欄へ exe パスを貼っていた画像に対応し、検索用アプリ名のコピーと、検索・新規登録・ファイル選択の違いを明記した。Windows の実 exe の FileDescription / ProductName はともに `Zaivern Code`、OriginalFilename は `zai.exe` と確認済み。
- スマホ接続は HTTPS を主操作にし、HTTP は詳細に置く。未導入時の案内、登録する exe のパスとフォルダ、Norton の公式説明へのリンクを表示する。

## 実施した検証

- `tools/verify.sh --bin` を入口に Windows ネイティブでビルド。レイアウト26件、スマホ接続ウィンドウのホイール1件、Tailscale16件、ファイアウォール41件、Windows起動1件、Windows更新2件が成功。
- 常時表示の追加指定後、上部バー2件・UI接続44件・CLI更新3件を再検証し成功。
- 全画面への追加変更後、456×280 / 456×800 / 1920×1050 の表示領域を満たすこと、画面外へはみ出さないこと、ホイールで末尾へ到達できることを同じ egui テストで確認。
- Norton 案内の追加2キーは6言語の `check`、一時辞書への `apply` 後の内容比較、`missing` 0件を確認。全画面変更後の修正版 GUI を再起動した。実スマホからの疎通・Norton 側の登録完了は引き続き未確認。
- `tools/windows-installer-test.ps1` の実行中 exe 置換、共有違反の再試行と時限、コピー失敗・復元、チェックサム17件、模擬更新の検査が成功。ソース更新の一時 cargo root と指定先への配置を4ケース追加。
- `zai i18n check` は6言語とも不足・余分なし。`missing` は0件。新規17キーを除いた一時辞書へ `apply` し、6言語の内容が元の辞書と一致することを確認。
- 専用の一時 `ZAIVERN_HOME` とワークスペースを使った GUI で、1024px幅の2段表示と全指定ボタンの表示を目視確認。通常幅での1段表示も確認。
- 別エージェントの反証レビューで、ソース更新が指定先以外の `.cargo/bin/zai.exe` を更新する問題を発見。更新専用時は一時 cargo root を使うよう修正し、再レビューで解消を確認。

## 実環境と制約

- 公式 winget パッケージのハッシュ検証後、Tailscale 1.102.3 のインストールが完了。導入直後は `NeedsLogin` だったが、その後のユーザーログインにより `Running` と PC・iPhone のオンライン状態を確認した。サンドボックス内の同コマンドはサービスの名前付きパイプへのアクセス拒否となるため、通常権限で確認した。
- 実際のスマホからの HTTPS 疎通・音声入力は未確認。Norton の独自設定は変更していない。
- スマホ接続画面のホイール操作は egui の入力イベントで検証した。実 GUI の接続画面操作は未確認。
- Windows 更新の公開版ダウンロードを伴う統合試験は、取り込み元の [検証記録](windows-update-validation.md) を参照。今回の統合後は再ダウンロードしていない。
- 使用中で削除できない GUID 付き `.old` は残る。安全な所有確認を伴う後日の自動削除は未実装。
- MSVC が `linker_messages` 警告を出力。検証スクリプトの「警告ゼロ」表示とは一致せず、警告ゼロとは扱わない。
- 全画面変更後の通常ビルドで `LNK1318: PDB LIMIT (12)` が発生。起動確認用は `cargo rustc --bin zai -- -C debuginfo=0` を使用する。リポジトリ共通のビルド設定は変更していない。
- Windows のクロス検証入口は cargo-xwin 不在、Linux は Docker 未起動。Windows ネイティブ以外の実 OS・GUI は未確認。
- 既存インストールの zai.exe は置き換えていない。修正版はこのワークツリーの `target/debug/zai.exe`。公開・リリースはしていない。

Norton の登録・自動検出仕様は [公式のプログラム制御の説明](https://support.norton.com/sp/ja/jp/home/current/solutions/v20240108181338560) を確認した。Norton 自体への登録成功・許可設定の変更は未確認で、案内追加を登録完了とは扱わない。
## Norton 一覧に出ない問題への追加修正

- Windows の更新情報取得を PowerShell への委譲から既存の ureq に変更し、実行中の zai.exe 自身が配布元へ HTTPS 通信するようにした。スマホ接続画面にも「Norton 検出用の通信を行う」を追加した。更新・設定変更は行わない。
- 最初のネイティブ HTTPS 試行では `UnknownIssuer` を再現した。Windows 限定で platform-verifier を有効化し、Windows の証明書ストアによる証明書・接続先名の検証を利用する。HTTPS 限定、全体20秒の時限、本文10MBの上限を維持する。
- 修正版の実バイナリで `zai update --check` を実行し、公式配布元の v0.24.2 を取得して「最新です」、終了コード0を確認した。専用の一時 ZAIVERN_HOME を使用した。
- Norton の保存済みルールを読み取り専用で確認し、今回のワークツリーの `target/debug/zai.exe` が `Zaivern Code` / `zai.exe` として記録されていた。HTTPS 送信の許可ルールも存在する。Norton の設定ファイルへ書き込みは行っていない。一覧画面への反映とスマホからの実接続は未確認。
- 通信成功が受信許可の判定を上書きしない回帰テストを追加した。ファイアウォール42件、CLI更新3件、全画面・ホイール1件が成功。6言語の check、新規4キーの一時辞書への apply と内容比較、missing 0件を確認した。
- `tools/verify.sh` は MSVC の PDB 上限で失敗し、デバッグ情報を減らした再試行ではディスク容量不足が発生した。今回生成した4つの incremental キャッシュだけを削除し、`--config 'profile.dev.package.zaivern-code.debug=0' --config 'profile.dev.incremental=false'` によりビルド・テストが成功した。リポジトリの共通プロファイルは変更していない。MSVC の linker_messages 警告は残る。
- 別エージェントの反証レビューで HTTP へのリダイレクト許可と検出完了後の無時限プロセス待ちを指摘され、HTTPS限定と結果型の分離により修正した。Windows の証明書・接続先名検証を維持することも再レビューした。新規 Windows 依存の宣言 MSRV は1.88以内。rustc 1.88 と他OSでの実行は未確認。
- 最新の修正版 GUI を起動し、ウィンドウの作成と応答を確認した。GUI の見た目・操作完了をプロセス確認だけで保証しない。公開・リリースおよび既存インストールの置換は行っていない。

## HTTPS 証明書未取得時の成功表示を修正

- `fix/compact-toolbar` / 009fd26 を基点に修正。証明書取得失敗を警告付き成功として扱っていたため、スマホで開けない URL と QR が表示されていた。証明書取得成功と Serve 設定成功の両方が揃った場合だけ URL を出すようにした。
- 失敗時は HTTPS の待受を維持して URL / QR を隠し、「HTTPS を再試行」を表示する。LAN の平文公開へ自動変更しない。準備失敗後も、この起動が作成した Serve の解除操作を維持する。
- `tailscale cert` の stdout は秘密鍵を含み得るため、失敗表示へ流さない。stderr が空なら一般の失敗文言を表示する。「2回目からは一瞬」という説明を撤回し、6言語へ新規3キーを追加した。
- Tailscale 18件、全画面ホイール1件が成功。証明書失敗→URL非公開→再試行成功、cleanup の保持、秘密鍵 stdout の非表示を検証した。別エージェントによる反証レビュー後の再レビューで追加の重大指摘なし。
- Windows ネイティブでビルド成功。前項の容量対策2設定を cargo ラッパー関数で引き継ぎ、`tools/verify.sh --quick` も成功。6言語 check、新規3キーの一時辞書への apply と内容一致、missing 0件を確認。リンク時の linker_messages 警告は残る。他OSでの実行は未確認。

### 実接続の診断と未完了事項

- Tailscale は Running、PC・iPhone はオンラインで、iPhone への tailscale ping が成功した。Serve は HTTPS 443 を今回のアプリの 127.0.0.1:8900 へ転送し、アプリ本体は HTTP 200 で応答した。
- スマホ側の TLS 接続は tailscaled の証明書取得処理まで到達したが、ACME の注文が invalid となるエラーを確認。その後は `SetDNS ... 500 Internal Server Error, failed to create DNS record` を確認した。実際のスマホからの HTTPS 接続は復旧していない。
- PC 自身からの tailnet IP:443 接続ではアクセス拒否も観測した。これだけでスマホからの到達不能や Norton の因果関係を断定しない。
- 公開証明書の発行者から Norton の HTTPS 検査を確認した。Norton の正式な設定画面で ACME 発行先だけを一時的に除外し、公開証明書の発行者が Let's Encrypt へ変わることを確認した。ただし証明書用 DNS 登録の500エラーは解消しなかったため、今回追加した除外を画面から削除した。既存の除外・ウイルス対策・ファイアウォールの設定は保全した。
- 管理者承認後に Tailscale サービスを再起動し Running へ復帰したが、証明書発行エラーは残る。ノートンの内部DBは編集していない。診断時に証明書秘密鍵の本文を表示・保存していない。
- 類似するサービス側エラーは [Tailscale #20823](https://github.com/tailscale/tailscale/issues/20823) にも報告されている。同一原因・回復時間は未確定で、待てば必ず復旧するとは保証しない。

### 続報（同日）: Norton を除外しても再現。サーバー側障害と判断し支援依頼を送信

- GitHub #20823 の全文を確認。報告者は macOS クライアントと無関係な Linux Kubernetes operator（別 ACME アカウント）の両方で同一の `SetDNS ... 500` を確認しており、AV/TLS 検査が一切無い環境でも発生し、10 時間後に何もせず自然回復したと明記している。**クライアント固有ではない**と報告側で断定済み。
- Norton の Web/Mail Shield が `acme-v02.api.letsencrypt.org` だけでなく、このマシンの通常の制御プレーン接続 (`login.tailscale.com`, `controlplane.tailscale.com`) の TLS も横取りしていることを直接の TLS プローブで新たに確認した（`google.com` は横取りされない＝ドメイン選択的）。SetDNS の実宛先はこの制御プレーンであるため、検証価値のある未検証の仮説だった。
- ユーザー本人が Norton の正規の Safe Web 除外設定から上記 3 ホスト（ACME + 2 制御プレーンホスト）を同時に除外。直後の TLS プローブで 3 ホストとも Let's Encrypt 発行の正規証明書が見えることを確認（横取りが外れたことの確認）。
- その状態で `tailscale cert` を計 3 回実行（1 回目は待ち時間が長く background 実行）。**3 回とも同一の `acme: order ... status: invalid` で失敗**（毎回新しい注文 ID）。Norton 除外は結論後に元へ戻された。
- 以上により、**Norton は原因ではないと確定**（除外してもしなくても同じ失敗）。GitHub #20823 の知見と整合し、Tailscale 制御プレーン側の DNS 書き込みパス不具合（断続的）である可能性が高いという結論に至った。
- ユーザーの明示的許可を得て、更新済みの問い合わせ文を `tailscale.com/contact/support` のフォームへ実際に送信した（ユーザーの Tailscale アカウントでログイン済みの状態、ブラウザ自動操作）。AI 自動応答（kapa.ai）は「既知の Tailscale 側バックエンド障害の兆候と一致し、知識ベースだけでは現在進行中の障害か確認できない、直接の support request が適切」と回答。「No, I still need help」を選んでフォームへ戻り、実際に送信して「Thank you for contacting our support team. We will get back to you shortly.」の確認画面を確認した。送信内容・Bug Report ID (`BUG-206867c248ec...`) は `.Codex/tailscale-https-support-request.txt` に保存済み。
- **未解決事項**: Tailscale サポートからの回答待ち。回答が来る、または `status.tailscale.com` に関連インシデントが出るまで、この Windows ノードでのスマホ HTTPS 接続は復旧していない。アプリ側の対応（1f57da4: 証明書未取得時は成功扱いにしない）は正しく機能しており、これ以上のアプリ側変更でこの障害は解消しない。
