//! **本物のクラウドへ課金しないための道具** (§54)。
//!
//! テストは必ずここの偽物を使う:
//!
//! | 偽物 | 何の代わりか |
//! |---|---|
//! | [`FakeHttpClient`] | Provider API (台本どおりに応答し、送った要求を覚える) |
//! | [`FakeTransport`] | SSH / ローカル実行 (**何も実行しない**) |
//! | [`FakeProvider`] | 実行先の湧き出し |
//!
//! ## 覚えることが要る理由
//!
//! 「断ったはず」を確かめるには、**断ったつもりで送っていないこと**を
//! 見なければならない。返り値だけを見ると、`DELETE` を送ってから
//! エラーを返す実装でも緑になる。だから偽物は**呼ばれた記録**を持つ。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::model::{
    Capabilities, CloudError, EventSink, ExecRequest, ExecResult, ExecutionTarget, ProbeResult,
    ProviderId, RemotePath, TargetCapacity, TargetEndpoint, TargetId, TargetLifecycle,
    TransportKind,
};
use super::provider::http::{HttpClient, HttpRequest, HttpResponse};
use super::provider::{ExecutionProvider, ProvisionSpec, ProvisioningMode};
use super::transport::ExecutionTransport;

// ───────────────────── 置き場の差し替え ─────────────────────

/// `~/.zaivern/cloud` を一時ディレクトリへ向ける番人。
///
/// **実 `~/.zaivern` を絶対に触らない** — 複数インスタンス同時編集が前提の
/// リポジトリでは、他人の生きた台帳を壊しうる。差し替えは**このスレッドだけ**
/// なので、並列に走る他のテストを巻き込まない。
pub struct HomeGuard {
    dir: PathBuf,
}

impl HomeGuard {
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        super::store::set_test_dir(None);
        // 自分が作ったものだけを消す (パターン検索で見つけたものは消さない)
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 一時的な置き場を用意する。
pub fn home_guard(tag: &str) -> HomeGuard {
    let dir = crate::test_util::unique_temp_dir("zv-cloud", tag).join("cloud");
    std::fs::create_dir_all(&dir).expect("置き場を作れる");
    super::store::set_test_dir(Some(dir.clone()));
    HomeGuard { dir }
}

// ───────────────────── 実行先の雛形 ─────────────────────

/// [`target`] / [`ssh_target`] に渡す指定。
#[derive(Debug, Clone)]
pub struct TargetOpts {
    pub max_jobs: u16,
    pub active_jobs: u16,
    pub cpu_cores: Option<u16>,
    pub memory_mib: Option<u64>,
    pub lifecycle: TargetLifecycle,
    pub port: u16,
    pub identity_file: Option<PathBuf>,
    pub provider: ProviderId,
}

impl Default for TargetOpts {
    fn default() -> Self {
        Self {
            max_jobs: 2,
            active_jobs: 0,
            cpu_cores: Some(4),
            memory_mib: Some(8192),
            lifecycle: TargetLifecycle::Ready,
            port: 22,
            identity_file: None,
            provider: ProviderId::new("static-ssh"),
        }
    }
}

/// 試験用の SSH 実行先。**ID は `id-<name>`** で、`name` から決まる
/// (テストの期待値が読みやすい。製品の ID 生成とは別物)。
pub fn ssh_target(host: &str, opts: TargetOpts) -> ExecutionTarget {
    let mut t = target("dev", opts);
    if let TargetEndpoint::Ssh { host: h, .. } = &mut t.endpoint {
        *h = host.to_string();
    }
    t
}

/// 試験用の実行先。
pub fn target(name: &str, opts: TargetOpts) -> ExecutionTarget {
    ExecutionTarget {
        id: TargetId::new(format!("id-{name}")),
        name: name.to_string(),
        provider: opts.provider.clone(),
        transport: TransportKind::Ssh,
        endpoint: TargetEndpoint::Ssh {
            host: "example.com".to_string(),
            user: "zaivern".to_string(),
            port: opts.port,
            identity_file: opts.identity_file.clone(),
        },
        capabilities: Capabilities {
            os: super::model::OsFamily::Linux,
            arch: super::model::Architecture::X86_64,
            cpu_cores: opts.cpu_cores,
            memory_mib: opts.memory_mib,
            ..Capabilities::default()
        },
        capacity: TargetCapacity::busy(opts.max_jobs, opts.active_jobs),
        lifecycle: opts.lifecycle,
        managed: false,
        labels: BTreeMap::new(),
        billing: super::model::BillingModel::Unknown,
        provider_ref: None,
        note: String::new(),
        generation: 0,
    }
}

// ───────────────────── 偽 HTTP ─────────────────────

/// 送られた要求の記録 (**ヘッダは覚えない** — 覚えるとテストの中に秘密が溜まる)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCall {
    pub method: &'static str,
    pub url: String,
    pub body: Option<String>,
}

/// 台本どおりに応答する偽 HTTP。
pub struct FakeHttpClient {
    script: Mutex<std::collections::VecDeque<HttpResponse>>,
    calls: Arc<Mutex<Vec<RecordedCall>>>,
}

impl FakeHttpClient {
    pub fn new(responses: Vec<HttpResponse>) -> Self {
        Self {
            script: Mutex::new(responses.into()),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 何度でも同じ応答を返す (再試行の試験用)。
    pub fn repeating(response: HttpResponse, times: usize) -> Self {
        Self::new(vec![response; times])
    }

    /// 送られた要求の記録。
    pub fn calls(&self) -> Arc<Mutex<Vec<RecordedCall>>> {
        self.calls.clone()
    }
}

impl HttpClient for FakeHttpClient {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, CloudError> {
        self.calls
            .lock()
            .expect("記録できる")
            .push(RecordedCall {
                method: request.method,
                url: request.url.clone(),
                body: request.body.clone(),
            });
        let next = self.script.lock().expect("台本を読める").pop_front();
        match next {
            Some(r) => Ok(r),
            // **台本が尽きたら失敗させる。** 黙って空の 200 を返すと、
            // 「呼びすぎ」がテストから見えなくなる。
            None => Err(CloudError::transport(format!(
                "偽 HTTP の台本が尽きました: {} {}",
                request.method, request.url
            ))),
        }
    }
}

/// **決めた場所で止まる HTTP。** 競合の順序を `sleep` ではなく門で作る。
///
/// ## なぜ `sleep` でも [`std::sync::Barrier`] でもないのか
///
/// * `sleep` で順序を作ると、遅い機械では順序が入れ替わって「たまに緑」になる
/// * `Barrier` は**時限を持たない**。相手が門に辿り着かない書き方に壊した
///   瞬間、テストが**永久に固まる** (実際にサボタージュ検証で固まった)。
///   固まる関門は、そのうち誰も見なくなる
///
/// だから時限つきのチャネルで作る。**待ちきれなければ理由を書いて落ちる。**
pub struct GatedHttpClient {
    entered_tx: std::sync::mpsc::Sender<()>,
    release_rx: Mutex<std::sync::mpsc::Receiver<()>>,
    gate_on: &'static str,
    get_response: HttpResponse,
    gated_response: HttpResponse,
    calls: Arc<Mutex<Vec<RecordedCall>>>,
}

/// [`GatedHttpClient`] の外側 (試験本体が持つ)。
pub struct Gate {
    entered_rx: std::sync::mpsc::Receiver<()>,
    release_tx: std::sync::mpsc::Sender<()>,
}

/// 門で待つ上限。**CI のジョブ上限より十分内側**で撃つ。
const GATE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

impl Gate {
    /// 相手が「止まる要求」に入るまで待つ。
    pub fn wait_entered(&self) {
        self.entered_rx
            .recv_timeout(GATE_WAIT)
            .expect("相手が門に入らないまま時間切れ (順序の前提が崩れている)");
    }

    /// 時限を指定して待つ。**この門が本当に諦めることを試験するための入口。**
    pub fn wait_entered_within(&self, d: std::time::Duration) -> bool {
        self.entered_rx.recv_timeout(d).is_ok()
    }

    /// 相手を解放する。
    pub fn release(&self) {
        // 相手がもう居なくても落とさない (先に失敗しているだけ)
        let _ = self.release_tx.send(());
    }
}

/// 探りの途中で必ず止まる Transport。
///
/// **「読んでからネットワークで確かめ、書き戻す」あいだに別の操作が入る**
/// という順序を、眠りの長さに頼らず決定的に作るために使う。
pub struct GatedTransport {
    entered_tx: std::sync::mpsc::Sender<()>,
    release_rx: Mutex<std::sync::mpsc::Receiver<()>>,
    probe: ProbeResult,
}

impl GatedTransport {
    /// 返すのは `(Transport, 門)`。`probe` に入ると門が開くのを待つ。
    pub fn new(probe: ProbeResult) -> (Self, Gate) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        (
            Self {
                entered_tx,
                release_rx: Mutex::new(release_rx),
                probe,
            },
            Gate {
                entered_rx,
                release_tx,
            },
        )
    }
}

impl super::transport::ExecutionTransport for GatedTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Ssh
    }

    fn probe(&self, _target: &ExecutionTarget) -> Result<ProbeResult, CloudError> {
        let _ = self.entered_tx.send(());
        let _ = self
            .release_rx
            .lock()
            .expect("読める")
            .recv_timeout(GATE_WAIT);
        Ok(self.probe.clone())
    }

    fn exec(
        &self,
        _target: &ExecutionTarget,
        _request: &ExecRequest,
        _sink: &mut dyn super::model::EventSink,
    ) -> Result<super::model::ExecResult, CloudError> {
        Err(CloudError::unsupported("門つきの Transport は実行しません"))
    }

    fn upload(
        &self,
        _target: &ExecutionTarget,
        _source: &std::path::Path,
        _destination: &RemotePath,
    ) -> Result<(), CloudError> {
        Err(CloudError::unsupported("門つきの Transport は送りません"))
    }

    fn download(
        &self,
        _target: &ExecutionTarget,
        _source: &RemotePath,
        _destination: &std::path::Path,
    ) -> Result<(), CloudError> {
        Err(CloudError::unsupported("門つきの Transport は受けません"))
    }
}

impl GatedHttpClient {
    pub fn new(
        gate_on: &'static str,
        get_response: HttpResponse,
        gated: HttpResponse,
    ) -> (Self, Gate) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        (
            Self {
                entered_tx,
                release_rx: Mutex::new(release_rx),
                gate_on,
                get_response,
                gated_response: gated,
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            Gate {
                entered_rx,
                release_tx,
            },
        )
    }

    pub fn calls(&self) -> Arc<Mutex<Vec<RecordedCall>>> {
        self.calls.clone()
    }
}

impl HttpClient for GatedHttpClient {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, CloudError> {
        self.calls
            .lock()
            .expect("記録できる")
            .push(RecordedCall {
                method: request.method,
                url: request.url.clone(),
                body: request.body.clone(),
            });
        if request.method == self.gate_on {
            // 外へ「入った」と知らせ、解放されるまで待つ (**時限つき**)
            let _ = self.entered_tx.send(());
            let rx = self.release_rx.lock().expect("読める");
            if rx.recv_timeout(GATE_WAIT).is_err() {
                return Err(CloudError::timeout(
                    "門が解放されないまま時間切れ (試験の順序が崩れている)",
                ));
            }
        } else {
            return Ok(self.get_response.clone());
        }
        Ok(self.gated_response.clone())
    }
}

// ───────────────────── 偽 Transport ─────────────────────

/// 実行の台本 1 件。
#[derive(Debug, Clone)]
pub struct FakeRun {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl FakeRun {
    pub fn ok(stdout: &str) -> Self {
        Self {
            exit_code: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    pub fn fail(code: i32, stderr: &str) -> Self {
        Self {
            exit_code: code,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }
}

/// **何も実行しない** Transport。走らせようとしたものを覚えるだけ。
pub struct FakeTransport {
    script: Mutex<std::collections::VecDeque<FakeRun>>,
    /// 台本が尽きたあとの既定 (指定しなければ成功)。
    default_run: FakeRun,
    pub executed: Arc<Mutex<Vec<ExecRequest>>>,
    pub uploads: Arc<Mutex<Vec<(PathBuf, String)>>>,
    pub downloads: Arc<Mutex<Vec<(String, PathBuf)>>>,
    probe: Mutex<ProbeResult>,
}

impl Default for FakeTransport {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl FakeTransport {
    pub fn new(script: Vec<FakeRun>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            default_run: FakeRun::ok(""),
            executed: Arc::new(Mutex::new(Vec::new())),
            uploads: Arc::new(Mutex::new(Vec::new())),
            downloads: Arc::new(Mutex::new(Vec::new())),
            probe: Mutex::new(ProbeResult {
                reachable: true,
                latency_ms: 1,
                capabilities: Capabilities {
                    os: super::model::OsFamily::Linux,
                    arch: super::model::Architecture::X86_64,
                    cpu_cores: Some(4),
                    memory_mib: Some(8192),
                    ..Capabilities::default()
                },
                shell: "/bin/sh".into(),
                kernel: "test".into(),
                error: String::new(),
            }),
        }
    }

    /// 走らせようとしたコマンドを、人が読める 1 行の並びで返す。
    pub fn commands(&self) -> Vec<String> {
        self.executed
            .lock()
            .expect("読める")
            .iter()
            .map(|r| r.display())
            .collect()
    }
}

impl ExecutionTransport for FakeTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Ssh
    }

    fn probe(&self, _target: &ExecutionTarget) -> Result<ProbeResult, CloudError> {
        Ok(self.probe.lock().expect("読める").clone())
    }

    fn exec(
        &self,
        _target: &ExecutionTarget,
        request: &ExecRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ExecResult, CloudError> {
        self.executed
            .lock()
            .expect("記録できる")
            .push(request.clone());
        let run = self
            .script
            .lock()
            .expect("台本を読める")
            .pop_front()
            .unwrap_or_else(|| self.default_run.clone());
        sink.on_stdout(run.stdout.as_bytes());
        sink.on_stderr(run.stderr.as_bytes());
        Ok(ExecResult {
            exit_code: Some(run.exit_code),
            duration_ms: 0,
        })
    }

    fn upload(
        &self,
        _target: &ExecutionTarget,
        source: &Path,
        destination: &RemotePath,
    ) -> Result<(), CloudError> {
        self.uploads
            .lock()
            .expect("記録できる")
            .push((source.to_path_buf(), destination.as_str().to_string()));
        Ok(())
    }

    fn download(
        &self,
        _target: &ExecutionTarget,
        source: &RemotePath,
        destination: &Path,
    ) -> Result<(), CloudError> {
        self.downloads
            .lock()
            .expect("記録できる")
            .push((source.as_str().to_string(), destination.to_path_buf()));
        Ok(())
    }
}

// ───────────────────── 偽 Provider ─────────────────────

/// 決まった実行先を返す Provider。**作った / 消した記録**を持つ。
pub struct FakeProvider {
    id: ProviderId,
    mode: ProvisioningMode,
    targets: Mutex<Vec<ExecutionTarget>>,
    pub provisioned: Arc<Mutex<Vec<ProvisionSpec>>>,
    pub destroyed: Arc<Mutex<Vec<TargetId>>>,
}

impl FakeProvider {
    pub fn new(id: &str, targets: Vec<ExecutionTarget>) -> Self {
        Self {
            id: ProviderId::new(id),
            mode: ProvisioningMode::Static,
            targets: Mutex::new(targets),
            provisioned: Arc::new(Mutex::new(Vec::new())),
            destroyed: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn dynamic(mut self) -> Self {
        self.mode = ProvisioningMode::Dynamic;
        self
    }
}

impl ExecutionProvider for FakeProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn mode(&self) -> ProvisioningMode {
        self.mode
    }

    fn list_targets(&self) -> Result<Vec<ExecutionTarget>, CloudError> {
        Ok(self.targets.lock().expect("読める").clone())
    }

    fn provision(&self, spec: &ProvisionSpec) -> Result<ExecutionTarget, CloudError> {
        if self.mode == ProvisioningMode::Static {
            return Err(CloudError::unsupported("この Provider は作れません"));
        }
        self.provisioned
            .lock()
            .expect("記録できる")
            .push(spec.clone());
        let mut t = target(&spec.name, TargetOpts::default());
        t.provider = self.id.clone();
        t.managed = true;
        t.lifecycle = TargetLifecycle::Provisioning;
        self.targets.lock().expect("書ける").push(t.clone());
        Ok(t)
    }

    fn destroy(&self, target: &ExecutionTarget) -> Result<(), CloudError> {
        if !target.managed {
            return Err(CloudError::security(
                "Zaivern が作っていない実行先は消しません",
            ));
        }
        self.destroyed
            .lock()
            .expect("記録できる")
            .push(target.id.clone());
        self.targets
            .lock()
            .expect("書ける")
            .retain(|t| t.id != target.id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn 偽httpは台本どおりに答えて記録する() {
        let c = FakeHttpClient::new(vec![HttpResponse {
            status: 200,
            body: "{}".into(),
        }]);
        let calls = c.calls();
        let r = c
            .send(&HttpRequest::get("https://x/y", Duration::ZERO))
            .expect("答える");
        assert_eq!(r.status, 200);
        assert_eq!(calls.lock().expect("読める").len(), 1);
        // 台本が尽きたら失敗する (呼びすぎがテストから見える)
        assert!(c
            .send(&HttpRequest::get("https://x/y", Duration::ZERO))
            .is_err());
    }

    #[test]
    fn 偽transportは何も実行しない() {
        let t = FakeTransport::new(vec![FakeRun::ok("done")]);
        let mut sink = super::super::model::CollectSink::default();
        let target = target("a", TargetOpts::default());
        let req = ExecRequest::new("rm", vec!["-rf".into(), "/".into()]);
        let r = t.exec(&target, &req, &mut sink).expect("走る");
        assert!(r.ok());
        assert_eq!(sink.stdout_text(), "done");
        assert_eq!(t.commands(), vec!["rm -rf /"]);
    }

    /// **門は固まらない。** 相手が来なければ諦める。
    ///
    /// 最初の版は [`std::sync::Barrier`] で作っていて、相手が門へ辿り着かない
    /// 書き方に壊した瞬間、テストが永久に固まった (サボタージュ検証で実際に
    /// 固まった)。固まる関門は、そのうち誰も見なくなる。
    #[test]
    fn 門は相手が来なければ諦める() {
        let (_http, gate) = GatedHttpClient::new(
            "DELETE",
            HttpResponse {
                status: 200,
                body: "{}".into(),
            },
            HttpResponse {
                status: 200,
                body: "{}".into(),
            },
        );
        // 誰も門に入らない → 待ちきらずに false で戻る (固まらない)
        assert!(!gate.wait_entered_within(Duration::from_millis(50)));
        // 解放は、相手が居なくても落ちない
        gate.release();
    }

    /// 門に入れば待たずに戻る (空振りしていないことの裏取り)。
    #[test]
    fn 門に入れば待たずに戻る() {
        let (http, gate) = GatedHttpClient::new(
            "DELETE",
            HttpResponse {
                status: 200,
                body: "get".into(),
            },
            HttpResponse {
                status: 200,
                body: "deleted".into(),
            },
        );
        let http = Arc::new(http);
        let worker = {
            let http = http.clone();
            std::thread::spawn(move || {
                http.send(&HttpRequest::delete("https://x/y", Duration::ZERO))
            })
        };
        assert!(gate.wait_entered_within(Duration::from_secs(30)), "門に入っていない");
        gate.release();
        let res = worker.join().expect("終わる").expect("答える");
        assert_eq!(res.body, "deleted");

        // 止める対象でない方法は素通りする
        let got = http
            .send(&HttpRequest::get("https://x/y", Duration::ZERO))
            .expect("答える");
        assert_eq!(got.body, "get");
    }

    #[test]
    fn 置き場は一時ディレクトリを指す() {
        let g = home_guard("self-check");
        assert_eq!(super::super::store::cloud_dir(), g.dir());
        // 実 ~/.zaivern を指していない
        let real = crate::config::zaivern_dir().join("cloud");
        assert_ne!(super::super::store::cloud_dir(), real);
    }

    #[test]
    fn 番人を落としたら差し替えも戻る() {
        let real = crate::config::zaivern_dir().join("cloud");
        {
            let _g = home_guard("self-check-drop");
            assert_ne!(super::super::store::cloud_dir(), real);
        }
        assert_eq!(super::super::store::cloud_dir(), real);
    }
}
