//! 🕸 メッシュ (Erlang 風プロセス通信層) の**登録だけ**。実体は `src/mesh.rs`。
//!
//! このファイルを置くだけで機能が繋がる。`main.rs` の `mod` 一覧にも
//! `feature.rs` のレジストリにも触らない (build.rs が `src/features/*.rs` を
//! 走査して拾う)。
//!
//! ## なぜ `#[path]` で実体を引き込むのか
//!
//! 実体は CLAUDE.md の指示どおり `src/mesh.rs` に置いている。ただし
//! `src/<名前>.rs` をクレートへ入れるには通常 `main.rs` の `mod` 一覧へ
//! 1 行足す必要があり、**それはまさに並列ブランチが取り合う共有行**である。
//! `#[path]` を使うと、`main.rs` を 1 バイトも触らずに実体を
//! `crate::features::mesh::imp` として取り込める (`train` / `semconf` と同じ形)。
//!
//! 実体が要求する共有面は**ゼロ**: `app.rs` / `palette.rs` / `feature.rs` /
//! `config.rs` / `keybinds.rs` / `main.rs` / `build.rs` / `cli.rs` の
//! どれも触っていない。設定も打鍵も持たない。
//!
//! ## 姉妹機能との住み分け
//!
//! * `lease.rs` — ファイル単位で「同じものを 2 人に触らせない」台帳
//! * `region.rs` — 行域の**重なり**を解釈する側
//! * `conflict.rs` — 起きてしまった衝突を早く見せる側
//! * `train.rs` — 見つけた後に順番を決めて統合する側
//! * `mesh.rs` — その全部の前段。**誰が生きていて、誰に何を伝えるか**を
//!   Erlang のプロセスモデル (Pid / メールボックス / link / monitor /
//!   監視ツリー) で持つ。GUI が落ちていても、短命な `zai hook` からでも成立する。
//!
//! ## 統合担当への申し送り
//!
//! CLI (`zai mesh spawn|list|whereis|register|send|recv|monitor|link|reap|ping`)
//! の入口は [`cli_main`] として公開してある。`src/cli.rs` は共有ファイルなので
//! **こちらでは配線していない** — サブコマンドの分岐へ次の 1 行を足すと繋がる:
//!
//! ```ignore
//! "mesh" => return Some(crate::features::mesh::cli_main(&args[1..])),
//! ```
//!
//! **欲しい打鍵は無い。** パレット (⌘⇧P → 「メッシュ」) の 1 経路だけにしてある
//! (CLAUDE.md「同じ操作への到達経路が 3 つあるなら 2 つ削る」)。

#[path = "../mesh.rs"]
mod imp;

pub use imp::FEATURE;

/// `zai mesh <sub>` の入口。`src/cli.rs` の dispatch から呼ばれる。
///
/// **まだ配線されていない** (共有ファイルなので統合担当が 1 行入れる)。
/// それまでは呼び手がテストしか居ないため、`dead_code` を明示的に許す —
/// 上の 1 行が入った時点でこの属性は消してよい。
pub use imp::cli_main;

/// メッシュの公開型。**橋 (`crate::negomesh`) がここから引く。**
///
/// 実体は `src/mesh.rs` だが `#[path]` で私有 `imp` として取り込んでいるので、
/// 外から使える名前をここで 1 か所だけ開く (2 か所に写すとズレる)。
pub use imp::{backoff, Mesh, Msg, Pid, SpawnOpts};

/// **台帳 (`crate::lease`) がここから引く 1 本の橋。**
///
/// `zai hook` は短命プロセスなので自分の PID を台帳へ書いても意味が無い。
/// メッシュに載っているエージェント本体の OS PID を引ければ、
/// `lease::gc_in` が「死んだ持ち主のリース」を TTL (30 分) より前に返せる。
pub use imp::linked_os_pid;
