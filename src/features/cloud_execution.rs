//! # ☁ Cloud Execution Provider — 計算資源を差し替え可能にする層
//!
//! ```text
//! Task / State → Context Engine → Agent Provider → [Cloud Execution] → 実行先
//!                                                        ├─ Scheduler
//!                                                        ├─ Execution Provider
//!                                                        ├─ Execution Target
//!                                                        └─ Execution Transport
//! ```
//!
//! ## 何のためにあるか
//!
//! ここは**新しい開発環境ではない**。既存の Control Plane
//! ([`crate::agents::AgentManager`] / [`crate::supervisor`] / [`crate::approvals`])
//! の**下へ 1 枚**、「その仕事をどの機械で走らせるか」を差し替えられる層を
//! 敷くだけの仕事である。エージェントの状態機械も承認キューもここには無い。
//!
//! ## 3 つを混ぜない (この層の中心的な約束)
//!
//! | 層 | 責務 | v1 の実装 |
//! |---|---|---|
//! | [`provider`] | **実行先をどこから持ってくるか**だけ | Local / StaticSsh / Hetzner |
//! | [`transport`] | **そこでどうコマンドを走らせるか**だけ | Local / Ssh |
//! | [`scheduler`] | **どれを選ぶか**だけ (純関数) | 能力 → 空き → 費用 |
//!
//! Hetzner の Provider は**コマンドを 1 つも実行しない** (VM を作る・消す・
//! [`model::ExecutionTarget`] へ変換するまで)。走らせるのは
//! [`transport::ssh::SshTransport`] の仕事で、これは Hetzner を 1 バイトも
//! 知らない。だから「SSH で入れる Linux」でありさえすれば、Provider 固有の
//! 実装が無くても Zaivern の実行先になる (§52 — v1.0 最大の価値)。
//!
//! ## エージェント名を知らない
//!
//! コア (`model` / `scheduler` / `registry` / `store` / `transport` /
//! `provider` / `git_workspace` / `runner` / `redact`) には、エージェントの
//! 実行ファイル名が 1 つも出てこない。番人は
//! [`tests::コアはエージェント名を知らない`]。
//!
//! 起動する中身は [`command::LaunchSpec`] として**外から**渡ってくる。
//! これは既存の [`crate::agents`] カタログが作るもので、こちらへ写経しない
//! (写経した瞬間に、エージェントが 1 つ増えるたびにこの層も増える)。
//!
//! ## Context Engine を呼ばない
//!
//! この層は [`crate::context`] を 1 度も呼ばない
//! ([`tests::コアはコンテキストエンジンを呼ばない`] が番人)。
//! 呼ぶのは上位の Orchestrator で、Cloud へ届くのは**もう出来上がった**
//! [`command::LaunchSpec`] だけ。こうしておくと Context Engine /
//! Agent Provider / Execution Provider を独立に交換できる。
//!
//! ## 勝手に何もしない (安全側の既定)
//!
//! * **有料 VM を勝手に作らない。** `--target auto` は「いま Ready な実行先
//!   から選ぶ」だけで、Provision には明示操作が要る (§33)。
//! * **秘密を保存しない。** 保存してよいのは環境変数の**名前**とパスだけ (§40)。
//! * **`StrictHostKeyChecking=no` を書かない** ([`transport::ssh`] の番人)。
//! * **利用者の `main` を触らない。** 結果はリモート追跡枝として持ち帰るだけで、
//!   merge / rebase / push はこの層の仕事ではない (§28)。

use crate::feature::{Entry, Feature, Setting, SettingValue};

// **`#[path]` で実体を明示する。** このファイル自身が build.rs の生成した
// `#[path]` 越しに読み込まれており、そうやって読まれたモジュールの子は
// **宣言側のディレクトリ (`src/features/`) から**探される — 素直に
// `pub mod model;` と書くと `src/features/model.rs` を探して E0583 で落ちる。
// `agents.rs` の `#[path = "approvals.rs"]` と同じ流儀。
#[path = "cloud_execution/cli.rs"]
pub mod cli;
#[path = "cloud_execution/command.rs"]
pub mod command;
#[path = "cloud_execution/git_workspace.rs"]
pub mod git_workspace;
#[path = "cloud_execution/model.rs"]
pub mod model;
#[path = "cloud_execution/panel.rs"]
pub mod panel;
#[path = "cloud_execution/provider/mod.rs"]
pub mod provider;
#[path = "cloud_execution/redact.rs"]
pub mod redact;
#[path = "cloud_execution/registry.rs"]
pub mod registry;
#[path = "cloud_execution/runner.rs"]
pub mod runner;
#[path = "cloud_execution/scheduler.rs"]
pub mod scheduler;
#[path = "cloud_execution/store.rs"]
pub mod store;
#[path = "cloud_execution/transport/mod.rs"]
pub mod transport;

#[cfg(test)]
#[path = "cloud_execution/test_support.rs"]
pub mod test_support;

pub use cli::{cli_main, HELP};
// **平らに出すのは、この層の外 (`src/cli.rs` / 将来の Orchestrator) が
// 名前で呼ぶものだけ。** 使われていない再エクスポートを並べると
// 「繋がっている」という嘘になる — 中の型は `crate::features::cloud_execution::model::…`
// で届く。

/// 実行先を選ぶときに、ローカルとリモートのどちらを先に見るか。
pub const KEY_PREFER: &str = "cloud_execution.prefer";
/// SSH の接続と 1 コマンドの待ち時間 (秒)。**永久待ちを作らないための上限**。
pub const KEY_SSH_TIMEOUT: &str = "cloud_execution.ssh_timeout_secs";
/// Provider API (HTTP) の待ち時間 (秒)。
pub const KEY_API_TIMEOUT: &str = "cloud_execution.api_timeout_secs";
/// 新しく足した実行先の既定の同時実行枠。
pub const KEY_DEFAULT_MAX_JOBS: &str = "cloud_execution.default_max_jobs";

/// パレットから開く窓と、この機能が宣言する設定。
///
/// **実体は `panel.rs` / `cli.rs` にある。** ここは登録だけ。
pub const FEATURE: Feature = Feature {
    module: "cloud_execution",
    entries: &[Entry {
        icon: "☁",
        label: "クラウド実行",
        id: "cloud_execution.panel",
    }],
    dispatch: |_app, ctx, id| match id {
        "cloud_execution.panel" => {
            panel::open(ctx.clone());
            true
        }
        _ => false,
    },
    draw: Some(panel::draw),
    settings: &[
        Setting {
            key: KEY_PREFER,
            label: "実行先の好み",
            help: "local = 手元を先に見る / remote = リモートを先に見る / any = 区別しない。能力と空きで絞ったあとの並べ替えにしか使いません。",
            default: SettingValue::Text("local"),
        },
        Setting {
            key: KEY_SSH_TIMEOUT,
            label: "SSH の待ち時間 (秒)",
            help: "接続と 1 コマンドの上限です。ここを 0 にはできません (永久待ちを作らないため)。",
            default: SettingValue::Int(30),
        },
        Setting {
            key: KEY_API_TIMEOUT,
            label: "クラウド API の待ち時間 (秒)",
            help: "Provider の HTTP 要求 1 本の上限です。",
            default: SettingValue::Int(30),
        },
        Setting {
            key: KEY_DEFAULT_MAX_JOBS,
            label: "実行先 1 台あたりの既定の同時実行数",
            help: "1 台 = 1 エージェントに固定しません。CPU から自動推定はせず、指定された値をそのまま使います。",
            default: SettingValue::Int(2),
        },
    ],
    ..Feature::DEFAULT
};

/// 設定から SSH の待ち時間を読む。**0 と負数は許さない** (永久待ちの入口になる)。
pub fn ssh_timeout(cfg: &crate::config::Config) -> std::time::Duration {
    secs_at_least_one(cfg.feature_i64(KEY_SSH_TIMEOUT))
}

/// 設定から Provider API の待ち時間を読む。
pub fn api_timeout(cfg: &crate::config::Config) -> std::time::Duration {
    secs_at_least_one(cfg.feature_i64(KEY_API_TIMEOUT))
}

/// 設定から既定の同時実行枠を読む。
pub fn default_max_jobs(cfg: &crate::config::Config) -> u16 {
    cfg.feature_i64(KEY_DEFAULT_MAX_JOBS).clamp(1, 4096) as u16
}

/// 設定から実行先の好みを読む。
pub fn prefer(cfg: &crate::config::Config) -> scheduler::Prefer {
    scheduler::Prefer::from_id(&cfg.feature_str(KEY_PREFER))
}

fn secs_at_least_one(v: i64) -> std::time::Duration {
    std::time::Duration::from_secs(v.clamp(1, 24 * 3600) as u64)
}

#[cfg(test)]
mod tests {
    /// **コアがエージェントを知らないことを、ソースの走査で固定する。**
    ///
    /// `if agent == "…"` の類が 1 つでも入ると、この層は「どのエージェントでも
    /// 使える実行層」ではなくエージェントごとの分岐の置き場になる。
    /// 規約はテストで強制しないと必ず腐るので番人にする (§56)。
    ///
    /// 対象は**コアだけ** — アダプタ (`cli.rs` / `panel.rs` / この登録ファイル)
    /// と `test_support.rs` は層の外側なので除く。
    #[test]
    fn コアはエージェント名を知らない() {
        // 実行ファイル名はカタログが真実の在り処。写経すると増えたときにずれる。
        // **3 文字以下は見ない** — `pi` / `amp` のような短い名前は普通の識別子
        // としても出てくるので、咎めると番人を黙らせるために変数名が歪む。
        const MIN_DISTINCTIVE: usize = 4;
        let bins: Vec<&'static str> = crate::agents::AGENT_CATALOG
            .iter()
            .map(|a| a.bin)
            .filter(|b| b.len() >= MIN_DISTINCTIVE)
            .collect();
        assert!(bins.len() >= 20, "見ている名前が {} 件しかない", bins.len());
        assert!(bins.contains(&"claude") && bins.contains(&"codex") && bins.contains(&"cursor-agent"));

        // **番人が空回りしていないことを、先に証明する。**
        let broken = "fn pick(bin: &str) -> u8 { if bin == \"claude\" { 1 } else { 0 } }";
        assert!(
            bins.iter()
                .any(|b| mentions_identifier(&product_code(broken), b)),
            "番人が空回りしている (分岐を仕込んでも捕まらない)"
        );
        let ok = "fn f() {}\n// claude からも使える\n#[cfg(test)]\nmod t { const A: &str = \"codex\"; }";
        assert!(
            !bins
                .iter()
                .any(|b| mentions_identifier(&product_code(ok), b)),
            "説明とテストまで咎めている"
        );

        let mut checked = 0usize;
        for (path, src) in core_sources() {
            let src = product_code(&src);
            checked += 1;
            for bin in &bins {
                assert!(
                    !mentions_identifier(&src, bin),
                    "{} に {bin:?} が出てくる。\n\
                     Cloud Execution はエージェントに依存しない層で、\n\
                     エージェントごとの分岐はここに置けない\n\
                     (起動する中身は command::LaunchSpec として外から渡すこと)",
                    path
                );
            }
        }
        assert!(checked >= 10, "コアのファイルが {checked} 件しか無い");
    }

    /// **コアが Context Engine を呼ばないことを固定する** (§38)。
    ///
    /// Cloud 側から [`crate::context`] を呼ぶと、「文脈をどう畳むか」の判断が
    /// 実行層へ漏れて 2 か所に散る。呼ぶのは上位の Orchestrator だけ。
    #[test]
    fn コアはコンテキストエンジンを呼ばない() {
        // 番人が空回りしていないことを先に証明する
        let broken = "let e = crate::context::ContextEngine::new(w);";
        assert!(
            product_code(broken).contains("crate::context"),
            "番人が空回りしている"
        );
        for (path, src) in core_sources() {
            let src = product_code(&src);
            assert!(
                !src.contains("crate::context") && !src.contains("super::super::context"),
                "{path} が Context Engine を呼んでいる。\n\
                 Cloud は出来上がった LaunchSpec を実行するだけの層で、\n\
                 文脈をどう畳むかを決めるのは上位の Orchestrator である"
            );
        }
    }

    /// 走査対象 (コアのソース) を集める。アダプタとテスト補助は除く。
    fn core_sources() -> Vec<(String, String)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/features/cloud_execution");
        const ADAPTERS: &[&str] = &["cli.rs", "panel.rs", "test_support.rs"];
        let mut out = Vec::new();
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d)
                .expect("src/features/cloud_execution を読める")
                .flatten()
            {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().and_then(|s| s.to_str()) != Some("rs") {
                    continue;
                }
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if ADAPTERS.contains(&name) {
                    continue;
                }
                let raw = std::fs::read_to_string(&p).expect("読める");
                out.push((p.display().to_string(), raw));
            }
        }
        out
    }

    /// 製品コードだけを残す (コメント行と `#[cfg(test)]` 以降を落とす)。
    ///
    /// **改行は先に正規化する** — Windows のチェックアウトは CRLF なので、
    /// 正規化しないと行の切り出しが 1 バイトずれる。
    fn product_code(src: &str) -> String {
        let text = src.replace("\r\n", "\n");
        let body = match text.find("#[cfg(test)]") {
            Some(at) => &text[..at],
            None => &text[..],
        };
        body.lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("/*") && !t.starts_with('*')
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 語が**識別子として**現れるか (前後が識別子文字なら別の語)。
    fn mentions_identifier(src: &str, word: &str) -> bool {
        let bytes = src.as_bytes();
        let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
        let mut from = 0usize;
        while let Some(rel) = src[from..].find(word) {
            let at = from + rel;
            let before_ok = at == 0 || !ident(bytes[at - 1]);
            let end = at + word.len();
            let after_ok = end >= bytes.len() || !ident(bytes[end]);
            if before_ok && after_ok {
                return true;
            }
            from = at + 1;
        }
        false
    }
}
