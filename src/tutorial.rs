//! 初回起動ガイドツアー — 機能の「場所」を光らせながら全機能を案内する。
//!
//! ## 何をするモジュールか
//! 1. **アンカー登録簿**: 各 UI が描画中に [`anchor`] を呼んで「自分はこの矩形にいる」
//!    と申告する。ツアーはその矩形を見てスポットライトを当てる。呼ばれていない
//!    (= パネルが閉じている / タブが非アクティブ) 場合でも壊れない。
//! 2. **スポットライト描画**: 画面全体を暗くし、対象の矩形だけ**塗らない**ことで
//!    「穴」を作る (シェーダ不要、egui 0.29 の `rect_filled` 4 枚で足りる)。
//!    対象の周りにはアニメーションするフォーカスリング、隣に説明カードを出す。
//! 3. **手順表はデータ**: [`STEPS`] という `const` テーブル 1 本。コードは散らばらない。
//! 4. **ホストへの依頼**: 「これから説明するパネルを開いておいて」は
//!    [`TutorialAction`] を**返す**だけ。自分では開かない (app.rs の状態を触らない)。
//!
//! ## 設計メモ
//! - **描画は毎フレーム、判断は純粋関数**。配置計算 ([`place_callout`])・
//!   進行 ([`Tutorial::next`] 等)・アンカー欠落の自動送り ([`MissingTracker`])・
//!   永続化 ([`load_from`] / [`save_to`]) はすべて egui 非依存でテストできる。
//! - **絶対にユーザーを閉じ込めない**。スキップは常に 1 クリック (Esc) で届き、
//!   オーバーレイは背面のクリックを奪わない (`interactable(false)` の暗幕)。
//! - **アンカーが永遠に現れない手順**でも止まらない。数秒フォールバック表示した
//!   あと自動で次へ進む。
//! - 文字列は [`crate::i18n::tr`] を通す。この方式は「日本語の原文そのものが
//!   辞書キー」なので、`i18n.rs` 側に足すものは何も無い (言語プラグインの
//!   `lang/*.toml` に原文キーを足すだけで英語化できる)。

// app.rs への配線 (anchor 呼び出し / overlay 呼び出し / パレットコマンド) が入るまで、
// この API 群はどこからも呼ばれない。配線が済んだらこの allow は外して良い。
#![allow(dead_code)]

use crate::i18n::tr;
use crate::keybinds::{BindAction, Keybinds};
use eframe::egui;
use egui::{Color32, Id, Pos2, Rect, Rounding, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ── 定数 ─────────────────────────────────────────────────────

/// 説明カードの基準幅 (ウィンドウが狭ければ縮む)。
const CARD_W: f32 = 340.0;
/// カードの高さの初期見積り (実測値が入るまでの 1 フレームだけ使う)。
const CARD_H_GUESS: f32 = 190.0;
/// 画面端から空ける余白。
const MARGIN: f32 = 12.0;
/// 対象矩形とカードの間隔。
const GAP: f32 = 14.0;
/// フォーカスリングが対象からはみ出す量。
const RING_PAD: f32 = 4.0;
/// アンカーが見つからないまま何秒経ったら自動で次へ進むか。
const MISSING_ANCHOR_TIMEOUT: f64 = 4.0;
/// アニメーション中の再描画間隔 (静止時のコストを払わないため 30fps に抑える)。
const REPAINT_MS: u64 = 33;
/// 手順表の版。テーブルを大幅改訂したら上げると、既読の人にも一度だけ出せる。
const STEPS_VERSION: u32 = 1;
/// 永続化ファイル名 (`~/.zaivern/` 直下)。
const STATE_FILE: &str = "tutorial.toml";

// ── アンカー ─────────────────────────────────────────────────

/// スポットライトを当てられる UI 要素の識別子。
///
/// UI 側は描画中に [`anchor`] でここへ矩形を申告する。**申告が無くても良い**
/// (パネルを閉じていれば単に光らないだけ)。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AnchorId {
    /// 画面上部のツールバー全体
    Toolbar,
    /// メニューバー (表示メニュー等)
    MenuBar,
    /// 最下部のステータスバー
    StatusBar,
    /// サイドバーのファイルツリー本体
    FileTree,
    /// エディタのタブ列
    EditorTabs,
    /// エディタ本文 (ビューア含む)
    EditorBody,
    /// ファイル内検索バー (⌘F)
    EditorFind,
    /// サイドバーの検索タブ
    SearchTab,
    /// コマンドパレットの入力欄
    CommandPalette,
    /// 下部のターミナル / エージェントパネル
    TerminalPanel,
    /// エージェント起動ボタン
    NewAgentButton,
    /// 権限モード切替ボタン (🛡 / ⚡)
    PermissionMode,
    /// Cockpit ボタン
    CockpitButton,
    /// フリート看板ボタン
    KanbanButton,
    /// ツールバーの「デッキ」ボタン (縦 1 本のエージェント管理)。
    DeckButton,
    /// サイドバーのセッション (過去の会話) タブ
    SessionsTab,
    /// サイドバーの Git タブ
    GitTab,
    /// サイドバーの GitHub タブ
    GitHubTab,
    /// 差分ビュー (PR / レースの diff タブ)
    DiffView,
    /// サイドバーのプラグインタブ
    PluginsTab,
    /// スマホリモート (📱) ボタン
    RemoteButton,
    /// 音声入力 (🎤) ボタン
    VoiceButton,
    /// デスクトップペット 🐾
    Pet,
    /// テーマ / 設定メニュー
    ThemeMenu,
}

impl AnchorId {
    /// 配線仕様・デバッグ表示に使う安定した文字列キー。
    pub fn key(self) -> &'static str {
        use AnchorId::*;
        match self {
            Toolbar => "toolbar",
            MenuBar => "menu_bar",
            StatusBar => "status_bar",
            FileTree => "file_tree",
            EditorTabs => "editor_tabs",
            EditorBody => "editor_body",
            EditorFind => "editor_find",
            SearchTab => "search_tab",
            CommandPalette => "command_palette",
            TerminalPanel => "terminal_panel",
            NewAgentButton => "new_agent_button",
            PermissionMode => "permission_mode",
            CockpitButton => "cockpit_button",
            KanbanButton => "kanban_button",
            DeckButton => "deck_button",
            SessionsTab => "sessions_tab",
            GitTab => "git_tab",
            GitHubTab => "github_tab",
            DiffView => "diff_view",
            PluginsTab => "plugins_tab",
            RemoteButton => "remote_button",
            VoiceButton => "voice_button",
            Pet => "pet",
            ThemeMenu => "theme_menu",
        }
    }
}

/// 全アンカーの一覧 (配線漏れをテストで検出するために持つ)。
pub const ALL_ANCHORS: &[AnchorId] = &[
    AnchorId::Toolbar,
    AnchorId::MenuBar,
    AnchorId::StatusBar,
    AnchorId::FileTree,
    AnchorId::EditorTabs,
    AnchorId::EditorBody,
    AnchorId::EditorFind,
    AnchorId::SearchTab,
    AnchorId::CommandPalette,
    AnchorId::TerminalPanel,
    AnchorId::NewAgentButton,
    AnchorId::PermissionMode,
    AnchorId::CockpitButton,
    AnchorId::KanbanButton,
    AnchorId::DeckButton,
    AnchorId::SessionsTab,
    AnchorId::GitTab,
    AnchorId::GitHubTab,
    AnchorId::DiffView,
    AnchorId::PluginsTab,
    AnchorId::RemoteButton,
    AnchorId::VoiceButton,
    AnchorId::Pet,
    AnchorId::ThemeMenu,
];

/// アンカー登録簿。`cur` = 今フレームの申告、`prev` = 前フレームの申告。
///
/// オーバーレイが UI より先に描かれても後に描かれても動くよう、**2 フレーム分**
/// 持つ。`cur` を優先し、無ければ `prev` を使う (1 フレームだけ古い矩形が出る
/// 可能性はあるが、パネルは毎フレーム同じ場所にいるので実害はない)。
#[derive(Default, Clone)]
struct Registry {
    cur: HashMap<AnchorId, Rect>,
    prev: HashMap<AnchorId, Rect>,
}

fn registry_id() -> Id {
    Id::new("zv-tutorial-anchors")
}

/// UI 側から呼ぶ: 「この要素はいまここにいる」と申告する。
///
/// 描画中ならどこで呼んでも良い。異常な矩形 (NaN / 潰れている) は黙って捨てる
/// ので、レイアウト計算前の値を渡しても壊れない。
pub fn anchor(ctx: &egui::Context, id: AnchorId, rect: Rect) {
    if !rect.is_finite() || !rect.is_positive() {
        return;
    }
    ctx.data_mut(|d| {
        let reg = d.get_temp_mut_or_default::<Registry>(registry_id());
        reg.cur.insert(id, rect);
    });
}

/// 登録簿から矩形を取り出し、フレームを 1 つ進める (オーバーレイが毎フレーム 1 回呼ぶ)。
fn take_anchor(ctx: &egui::Context, id: Option<AnchorId>) -> Option<Rect> {
    ctx.data_mut(|d| {
        let reg = d.get_temp_mut_or_default::<Registry>(registry_id());
        let found = id.and_then(|k| reg.cur.get(&k).or_else(|| reg.prev.get(&k)).copied());
        // フレームを回す: 今フレームの申告を「前フレーム」へ送る。
        reg.prev = std::mem::take(&mut reg.cur);
        found
    })
}

// ── ホストへの依頼 ───────────────────────────────────────────

/// サイドバーのタブ (app.rs の `SidebarTab` は private なので、ここでは自前の型で表す)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SidebarTarget {
    Files,
    Search,
    Agents,
    Sessions,
    Plugins,
    Git,
    GitHub,
}

/// 「この手順を説明する前に、これを開いておいてほしい」という依頼。
///
/// ツアーは**自分では実行しない**。[`Tutorial::overlay`] が返すので、ホスト
/// (app.rs) が自分の状態を使って実行する。実行できなくても構わない
/// (アンカーが現れなければフォールバック表示 → 自動送りになる)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TutorialAction {
    /// サイドバーを開き、指定タブへ切り替える
    OpenSidebar(SidebarTarget),
    /// 下部のターミナル / エージェントパネルを開く
    ShowTerminalPanel,
    /// Agent Cockpit を開く
    ShowCockpit,
    /// フリート看板を開く
    ShowKanban,
    /// エージェントデッキ (縦 1 本) を開く
    ShowDeck,
    /// コマンドパレットを開く (コマンドモード)
    OpenPalette,
    /// プロンプトレースの開始フォームを開く
    OpenRaceForm,
    /// スマホリモートの QR 画面を開く
    ShowRemoteQr,
}

// ── 手順表 (データ) ──────────────────────────────────────────

/// 章。手順表は章ごとに**連続**していなければならない (テストで担保)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Chapter {
    Welcome,
    Editor,
    Agents,
    Review,
    Extend,
    Finish,
}

impl Chapter {
    /// 画面に出す章名 (翻訳前の原文)。
    pub fn label(self) -> &'static str {
        match self {
            Chapter::Welcome => "はじめに",
            Chapter::Editor => "エディタ",
            Chapter::Agents => "エージェント",
            Chapter::Review => "レビューと Git",
            Chapter::Extend => "拡張と外の世界",
            Chapter::Finish => "仕上げ",
        }
    }
}

/// 手順 1 つ。**すべてデータ**で、描画コードには何も書かない。
pub struct Step {
    /// 一意な識別子 (再開位置の保存やテストに使う)
    pub id: &'static str,
    pub chapter: Chapter,
    /// 光らせる場所。`None` なら画面中央のカードだけ出す
    pub anchor: Option<AnchorId>,
    pub title: &'static str,
    /// 3 文以内の短い説明
    pub body: &'static str,
    /// キーボードショートカットのチップ (無ければ出さない)。
    ///
    /// **打鍵をここへ書かないこと。** 再割り当てで嘘になり、Windows/Linux では
    /// 表記そのものが違う。打鍵は [`Step::keys`] に `BindAction` で並べる。
    pub hint: Option<&'static str>,
    /// 案内に出す打鍵 (キーバインド表から生成する)。空なら [`Step::hint`] を使う。
    pub keys: &'static [crate::keybinds::BindAction],
    /// 説明の前にホストへ頼みたいこと
    pub pre_action: Option<TutorialAction>,
}

/// ツアー本体。README の機能リファレンスを一巡する。
pub const STEPS: &[Step] = &[
    // ── はじめに ──
    Step {
        id: "welcome",
        chapter: Chapter::Welcome,
        anchor: None,
        title: "Zaivern Code へようこそ",
        body: "この操縦席の計器を、場所を光らせながら一巡します。\n所要 2 分ほど。いつでも「スキップ」で終われます。",
        hint: Some("→ / Enter で次へ"),
        keys: &[],
        pre_action: None,
    },
    Step {
        id: "layout",
        chapter: Chapter::Welcome,
        anchor: Some(AnchorId::Toolbar),
        title: "上部ツールバー",
        body: "Cockpit・看板・権限モード・📱・🎤 など、よく使うものが並びます。\nここが操縦席の計器盤です。",
        hint: None,
        keys: &[],
        pre_action: None,
    },
    // ── エディタ ──
    Step {
        id: "file_tree",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::FileTree),
        title: "ファイルツリーと git の色",
        body: "変更のあるファイルには M / A / U / D / R / C のバッジが付き、たたんだ親フォルダにも色が乗ります。\n右クリックで新規・名前変更・削除・「@パスをエージェントへ送信」。\nタブを切り替えるとツリーが自動追従します。",
        hint: None,
        keys: &[BindAction::FocusExplorer],
        pre_action: Some(TutorialAction::OpenSidebar(SidebarTarget::Files)),
    },
    Step {
        id: "editor_tabs",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::EditorTabs),
        title: "タブとエディタの基本",
        body: "syntect の構文ハイライト、行番号ガター、未保存マーク (●)。\n行ガターは git 差分 (緑=追加 / 黄=変更) と LSP 診断 (赤 / 黄) で色が付きます。",
        hint: None,
        keys: &[BindAction::Save, BindAction::CloseTab],
        pre_action: None,
    },
    Step {
        id: "viewers",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::EditorBody),
        title: "画像・PDF・プレビュー",
        body: "png/jpg/gif/webp/ico は画像ビューア (⌘スクロールで 0.05〜32 倍)。\nPDF はページ区切り付きの読み取り専用テキストに展開されます。\nMarkdown / HTML はレンダリング表示に切り替えられます。",
        hint: None,
        keys: &[],
        pre_action: None,
    },
    Step {
        id: "view_options",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::MenuBar),
        title: "折り返しと空白文字",
        body: "表示メニュー (またはコマンドパレット) から「折り返し切替」「空白文字表示切替」。\nスペースは ·、タブは → で見えるようになります。\n初期値は config.toml、プロジェクト単位の上書きは .zaivern.toml です。",
        hint: None,
        keys: &[],
        pre_action: None,
    },
    Step {
        id: "find",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::EditorFind),
        title: "ファイル内検索",
        body: "ヒット件数を出しながら、該当行を画面中央へジャンプします。\nエージェント端末にフォーカスがあるときは、スクロールバック全文の検索になります。",
        hint: None,
        keys: &[BindAction::Find],
        pre_action: None,
    },
    Step {
        id: "search",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::SearchTab),
        title: "横断検索と置換",
        body: "ワークスペース全体を検索し、正規表現・glob の絞り込み・一括置換ができます。\n複数フォルダを開いていれば全ルートを横断します。",
        hint: None,
        keys: &[BindAction::GlobalSearch],
        pre_action: Some(TutorialAction::OpenSidebar(SidebarTarget::Search)),
    },
    Step {
        id: "palette",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::CommandPalette),
        title: "コマンドパレット",
        body: "入力欄 1 つでファイル・コマンド・エージェント・worktree を横断します。\n無印=ファイル、> =コマンド、@ =エージェント、# =git worktree。",
        hint: None,
        keys: &[BindAction::PaletteFiles, BindAction::PaletteCommands],
        pre_action: Some(TutorialAction::OpenPalette),
    },
    // ── エージェント ──
    Step {
        id: "terminal",
        chapter: Chapter::Agents,
        anchor: Some(AnchorId::TerminalPanel),
        title: "エージェント端末",
        body: "本物の PTY なので、日本語 IME もカラーも普通に動きます。\nファイルをドラッグ&ドロップ、⌘V でクリップボードの画像を貼ると @パス が入ります (送信はしません)。",
        hint: None,
        keys: &[BindAction::ToggleTerminal],
        pre_action: Some(TutorialAction::ShowTerminalPanel),
    },
    Step {
        id: "new_agent",
        chapter: Chapter::Agents,
        anchor: Some(AnchorId::NewAgentButton),
        title: "エージェントを起動する",
        body: "29 種の CLI エージェントを内蔵カタログで認識します (Claude Code / Codex / Cursor ほか)。\nプリセットは config.toml の [[agents]] にいくつでも追加でき、env でアカウントも分けられます。",
        hint: None,
        keys: &[BindAction::NewAgent],
        pre_action: None,
    },
    Step {
        id: "permission",
        chapter: Chapter::Agents,
        anchor: Some(AnchorId::PermissionMode),
        title: "権限モードと ⚡自動YES",
        body: "🛡承認 / ⚡全自動 / 👾Agent優先 の 3 モードをワンクリックで切り替え、実行中のセッションにも一括送信できます。\n対話プロンプトへの自動応答は別スイッチ「⚡ 自動YES」で、既定はオフです。\n最後の YES は、必ずあなたのものです。",
        hint: None,
        keys: &[],
        pre_action: None,
    },
    Step {
        id: "cockpit",
        chapter: Chapter::Agents,
        anchor: Some(AnchorId::CockpitButton),
        title: "Agent Cockpit と一斉送信",
        body: "走っている全エージェントがライブ端末のグリッドで並び、上の入力欄から全員へ一斉送信できます。\n停滞・ループ・異常終了は見張りが検知してあなたへ上げます (勝手に打ち込みはしません)。\n「💡 スーパーエージェント」で 1 体を指揮官に指名することもできます。",
        hint: None,
        keys: &[BindAction::ToggleCockpit],
        pre_action: Some(TutorialAction::ShowCockpit),
    },
    Step {
        id: "deck",
        chapter: Chapter::Agents,
        anchor: Some(AnchorId::DeckButton),
        title: "エージェントデッキ",
        body: "稼働中・過去のセッション・新規起動を縦 1 本にまとめた画面です。\n↑↓ で選ぶと右 (狭い画面では下) にその端末が出ます。\n積み上げモードにすると複数のセッションを上下に同時表示できます。",
        hint: None,
        keys: &[],
        pre_action: Some(TutorialAction::ShowDeck),
    },
    Step {
        id: "kanban",
        chapter: Chapter::Agents,
        anchor: Some(AnchorId::KanbanButton),
        title: "フリート看板",
        body: "待機 / 思考中 / 編集中 / 実行中 / 検証中 / 承認待ち / 停滞 / 完了 の 8 レーンで全機を俯瞰します。\n縦モードならカードを選ぶと下にライブ端末が出て、↑↓ で移動・Enter でそのまま入力できます。\n推定には ≈、確かな根拠には ✓ が付きます。",
        hint: None,
        keys: &[BindAction::ToggleKanban],
        pre_action: Some(TutorialAction::ShowKanban),
    },
    Step {
        id: "race",
        chapter: Chapter::Agents,
        anchor: Some(AnchorId::CommandPalette),
        title: "プロンプト・ファンアウトレース",
        body: "1 つの指示を 2〜4 体へ同時に渡し、それぞれ専用の git worktree で走らせます。\n走行中から競合ファイルを突き合わせ、採用は merge、破棄は worktree ごと消して残骸を残しません。",
        hint: None,
        keys: &[],
        pre_action: Some(TutorialAction::OpenRaceForm),
    },
    // ── レビューと Git ──
    Step {
        id: "sessions",
        chapter: Chapter::Review,
        anchor: Some(AnchorId::SessionsTab),
        title: "過去の会話と再開",
        body: "フォルダごとの会話履歴が残り、開き直すと前回のタブがスクロールバックごと戻ります。\nclaude は --continue、codex は resume --last が自動で付くので、CLI 側の会話も続きから始まります。",
        hint: None,
        keys: &[],
        pre_action: Some(TutorialAction::OpenSidebar(SidebarTarget::Sessions)),
    },
    Step {
        id: "git_panel",
        chapter: Chapter::Review,
        anchor: Some(AnchorId::GitTab),
        title: "Git パネル",
        body: "変更ファイルの一覧・ステージ・コミット・ブランチ操作をここから。\ngit status はバックグラウンドスレッドで 2 秒ごとに取り直すので、大きなリポジトリでもフレームは落ちません。",
        hint: None,
        keys: &[],
        pre_action: Some(TutorialAction::OpenSidebar(SidebarTarget::Git)),
    },
    Step {
        id: "diff_review",
        chapter: Chapter::Review,
        anchor: Some(AnchorId::DiffView),
        title: "差分へのインラインコメント",
        body: "差分タブでは行をクリックしてその場にコメントを書けます。解決 / 未解決も管理できます。\n未解決のコメントはまとめて 1 通のプロンプトになり、そのままエージェントへ渡せます。",
        hint: None,
        keys: &[],
        pre_action: None,
    },
    Step {
        id: "github_pr",
        chapter: Chapter::Review,
        anchor: Some(AnchorId::GitHubTab),
        title: "GitHub と worktree",
        body: "gh 経由で PR / Issue を一覧し、差分をインライン diff で読めます (追加の認証設定は不要)。\nIssue の「⚡ 着手」で専用 worktree の作成・ワークスペース追加・エージェント起動まで繋がります。",
        hint: None,
        keys: &[],
        pre_action: Some(TutorialAction::OpenSidebar(SidebarTarget::GitHub)),
    },
    // ── 拡張と外の世界 ──
    Step {
        id: "plugins",
        chapter: Chapter::Extend,
        anchor: Some(AnchorId::PluginsTab),
        title: "プラグイン",
        body: "worktrees・diff-review・tasks・quick-actions などが標準で同梱され、初回起動から有効です。\n中身はただのシェルスクリプトなので、読んで真似して書き換えられます (Rust の再ビルド不要)。\n.zvplug でエクスポート / インストールもできます。",
        hint: None,
        keys: &[],
        pre_action: Some(TutorialAction::OpenSidebar(SidebarTarget::Plugins)),
    },
    Step {
        id: "remote",
        chapter: Chapter::Extend,
        anchor: Some(AnchorId::RemoteButton),
        title: "スマホリモート",
        body: "📱 の QR を読むだけで、同じ Wi-Fi のスマホがリモコンになります (承認・指示出し・編集まで)。\n認証は起動ごとのランダムトークンで、届く範囲は LAN 内のみ。\nWindows は同じ画面から受信許可を作れます。",
        hint: None,
        keys: &[],
        pre_action: Some(TutorialAction::ShowRemoteQr),
    },
    Step {
        id: "voice",
        chapter: Chapter::Extend,
        anchor: Some(AnchorId::VoiceButton),
        title: "音声入力",
        body: "🎤 を押すと、話した内容が入力欄へ流れ込み続けます (⏹ で停止)。\nEnter は送らないので、目で見て直して納得してから自分で送信します。\n宛先は 🎯 アクティブ / 📣 全エージェントを、録音したまま切り替えられます。",
        hint: None,
        keys: &[],
        pre_action: None,
    },
    Step {
        id: "pet",
        chapter: Chapter::Extend,
        anchor: Some(AnchorId::Pet),
        title: "通知と相棒 🐾ザイガニ",
        body: "承認待ちになるとポップアップ・効果音・バブルから ✔承認 / ✖拒否 ができます。\nwebhook_url を書いておけば ntfy / Slack / Discord で外出先へも届きます。\n見た目 4 種とサイズ 3 段階、クリックで Cockpit が開きます。",
        hint: None,
        keys: &[],
        pre_action: None,
    },
    // ── 仕上げ ──
    Step {
        id: "personalize",
        chapter: Chapter::Finish,
        anchor: Some(AnchorId::ThemeMenu),
        title: "テーマとキーバインド",
        body: "テーマはダーク 7・ライト 4 を内蔵 + VS Code 互換テーマ JSON をそのまま読み込めます。\nショートカットは config.toml の [keybindings] で全部差し替えられます。",
        hint: None,
        keys: &[],
        pre_action: None,
    },
    Step {
        id: "cli",
        chapter: Chapter::Finish,
        anchor: Some(AnchorId::StatusBar),
        title: "ターミナルからの操作 (zai)",
        body: "zai open / prompt / run / notify で、動いているこのエディタを外から操作できます。\nzai status は起動中のインスタンスを一覧し、1 つも無ければ終了コード 1 を返します。",
        hint: None,
        keys: &[],
        pre_action: None,
    },
    Step {
        id: "finish",
        chapter: Chapter::Finish,
        anchor: None,
        title: "ここまでです",
        body: "あとは走らせるだけ。困ったらコマンドパレット (下の打鍵) から「チュートリアルを再開」でいつでも戻ってこられます。\nよい夜を。",
        hint: None,
        keys: &[BindAction::PaletteCommands],
        pre_action: None,
    },
];

// ── カードの配置計算 (純粋関数) ──────────────────────────────

/// カードを対象のどちら側に置いたか。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Right,
    Left,
    Below,
    Above,
    /// どこにも入らなかった / 対象が無い → 画面中央
    Center,
}

/// 配置の結果。`rect` は**必ず画面内**に収まっている。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Placement {
    pub side: Side,
    pub rect: Rect,
}

/// 値を [lo, hi] に収める (hi < lo でも lo を返して壊れない)。
fn clamp(v: f32, lo: f32, hi: f32) -> f32 {
    if hi < lo {
        lo
    } else {
        v.clamp(lo, hi)
    }
}

/// 矩形を画面内へ押し込む (画面よりカードが大きい場合は左上に寄せる)。
fn clamp_rect(r: Rect, screen: Rect, margin: f32) -> Rect {
    let x = clamp(
        r.min.x,
        screen.min.x + margin,
        screen.max.x - margin - r.width(),
    );
    let y = clamp(
        r.min.y,
        screen.min.y + margin,
        screen.max.y - margin - r.height(),
    );
    Rect::from_min_size(Pos2::new(x, y), r.size())
}

/// 対象の隣にカードを置く。**画面外へは絶対に出さない**。
///
/// 右 → 左 → 下 → 上 の順に「入る側」を探し、どこにも入らなければ中央に置く。
/// `card` が画面より大きい場合は画面に合わせて縮める。
pub fn place_callout(target: Option<Rect>, screen: Rect, card: Vec2) -> Placement {
    // カードが画面に入らないなら縮める (小さいウィンドウ対応)。
    let w = card.x.min((screen.width() - 2.0 * MARGIN).max(1.0));
    let h = card.y.min((screen.height() - 2.0 * MARGIN).max(1.0));
    let size = Vec2::new(w, h);

    let center = || Placement {
        side: Side::Center,
        rect: clamp_rect(
            Rect::from_center_size(screen.center(), size),
            screen,
            MARGIN,
        ),
    };

    let Some(t) = target else { return center() };
    // 対象が画面外にはみ出していても、見えている部分だけを基準にする。
    let t = t.intersect(screen);
    if !t.is_positive() {
        return center();
    }

    let space_right = screen.max.x - t.max.x - GAP - MARGIN;
    let space_left = t.min.x - screen.min.x - GAP - MARGIN;
    let space_below = screen.max.y - t.max.y - GAP - MARGIN;
    let space_above = t.min.y - screen.min.y - GAP - MARGIN;

    let (side, min) = if space_right >= w {
        (
            Side::Right,
            Pos2::new(t.max.x + GAP, t.center().y - h * 0.5),
        )
    } else if space_left >= w {
        (
            Side::Left,
            Pos2::new(t.min.x - GAP - w, t.center().y - h * 0.5),
        )
    } else if space_below >= h {
        (
            Side::Below,
            Pos2::new(t.center().x - w * 0.5, t.max.y + GAP),
        )
    } else if space_above >= h {
        (
            Side::Above,
            Pos2::new(t.center().x - w * 0.5, t.min.y - GAP - h),
        )
    } else {
        return center();
    };

    Placement {
        side,
        rect: clamp_rect(Rect::from_min_size(min, size), screen, MARGIN),
    }
}

/// 対象を「くり抜いた」暗幕を、4 枚の矩形で表す。
///
/// `target` が無ければ画面全体 1 枚。対象が画面外なら普通に全面が暗くなる。
/// 暗幕の濃さ。
///
/// 既定のテーマは既に暗いので、そこへ濃い黒を重ねると説明中だけ画面が
/// ほとんど読めなくなる (実際に「説明する時に暗くなる」と指摘を受けた)。
/// 焦点はリングと吹き出しで示し、暗幕は「周囲を少し落とす」程度に留める。
pub fn dim_alpha(dark_theme: bool) -> u8 {
    if dark_theme {
        56
    } else {
        96
    }
}

pub fn dim_rects(target: Option<Rect>, screen: Rect) -> Vec<Rect> {
    let Some(t) = target
        .map(|t| t.intersect(screen))
        .filter(|t| t.is_positive())
    else {
        return vec![screen];
    };
    let mut out = Vec::with_capacity(4);
    // 上
    if t.min.y > screen.min.y {
        out.push(Rect::from_min_max(
            screen.min,
            Pos2::new(screen.max.x, t.min.y),
        ));
    }
    // 下
    if t.max.y < screen.max.y {
        out.push(Rect::from_min_max(
            Pos2::new(screen.min.x, t.max.y),
            screen.max,
        ));
    }
    // 左
    if t.min.x > screen.min.x {
        out.push(Rect::from_min_max(
            Pos2::new(screen.min.x, t.min.y),
            Pos2::new(t.min.x, t.max.y),
        ));
    }
    // 右
    if t.max.x < screen.max.x {
        out.push(Rect::from_min_max(
            Pos2::new(t.max.x, t.min.y),
            Pos2::new(screen.max.x, t.max.y),
        ));
    }
    out.retain(|r| r.is_positive());
    out
}

// ── アンカー欠落の自動送り (純粋) ────────────────────────────

/// 「アンカーが現れないまま何秒経ったか」を数え、時間切れで自動送りを促す。
///
/// パネルを開く依頼をホストが実行できなかった場合でも、ツアーが**そこで止まらない**
/// ようにするための保険。
#[derive(Default, Clone, Copy, Debug)]
pub struct MissingTracker {
    since: Option<f64>,
}

impl MissingTracker {
    /// 毎フレーム呼ぶ。`true` を返したら自動で次の手順へ進める。
    pub fn observe(&mut self, now: f64, anchor_present: bool) -> bool {
        if anchor_present {
            self.since = None;
            return false;
        }
        match self.since {
            None => {
                self.since = Some(now);
                false
            }
            Some(t0) => now - t0 >= MISSING_ANCHOR_TIMEOUT,
        }
    }

    /// 手順が変わったら呼ぶ。
    pub fn reset(&mut self) {
        self.since = None;
    }

    /// フォールバック表示に入っているか (テスト・表示用)。
    pub fn waiting(&self) -> bool {
        self.since.is_some()
    }
}

// ── 永続化 ───────────────────────────────────────────────────

/// `~/.zaivern/tutorial.toml` の中身。
///
/// **本来は config.toml のキー 1 つ (`show_tutorial` / `tutorial_done`) にすべき**
/// だが、config.rs は今このブランチで別の担当が触っているため、独立ファイルに
/// 逃がしている。config.toml へ移すなら `[general] tutorial_done = true` を
/// 推奨する (このファイルは移行後に読むだけにして捨てられる)。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Persisted {
    /// 一度でも最後まで見た / スキップしたか
    pub done: bool,
    /// 見たときの手順表の版
    pub version: u32,
}

impl Default for Persisted {
    fn default() -> Self {
        Self {
            done: false,
            version: 0,
        }
    }
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
}

/// 状態を読む。ファイルが無い / 壊れている / 読めない → **初回扱い** (既定値)。
pub fn load_from(dir: &Path) -> Persisted {
    std::fs::read_to_string(state_path(dir))
        .ok()
        .and_then(|s| toml::from_str::<Persisted>(&s).ok())
        .unwrap_or_default()
}

/// 状態を書く。失敗しても黙って諦める (チュートリアルのために起動を止めない)。
pub fn save_to(dir: &Path, st: &Persisted) {
    if let Ok(s) = toml::to_string_pretty(st) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(state_path(dir), s);
    }
}

/// 初回起動か? (= 自動でツアーを出すべきか)
///
/// 手順表の版が上がっていたら、既読の人にももう一度出す。
pub fn should_autostart() -> bool {
    should_autostart_in(&crate::config::zaivern_dir())
}

/// [`should_autostart`] のディレクトリ指定版 (テスト用)。
pub fn should_autostart_in(dir: &Path) -> bool {
    let st = load_from(dir);
    !st.done || st.version < STEPS_VERSION
}

// ── 本体 ─────────────────────────────────────────────────────

/// ツアーの状態。app.rs はこれを 1 つ持つだけで良い。
pub struct Tutorial {
    active: bool,
    idx: usize,
    /// スキップ確認を出しているか (最初の手順ではいきなり終了するので出さない)
    confirm_skip: bool,
    missing: MissingTracker,
    /// この手順に入ったとき 1 回だけホストへ返す依頼
    pending: Option<TutorialAction>,
    /// 状態ファイルの置き場所 (テストで差し替えられるようにフィールドで持つ)
    dir: PathBuf,
}

impl Default for Tutorial {
    fn default() -> Self {
        Self::new()
    }
}

/// [`Tutorial::next`] の結果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Nav {
    /// 次の手順へ進んだ
    Moved,
    /// 最後まで見終わって終了した
    Completed,
}

impl Tutorial {
    /// 既定の場所 (`~/.zaivern/`) を使うツアーを作る。まだ開始はしない。
    pub fn new() -> Self {
        Self::in_dir(crate::config::zaivern_dir())
    }

    /// 状態ファイルの置き場所を指定して作る (テスト用)。
    pub fn in_dir(dir: PathBuf) -> Self {
        Self {
            active: false,
            idx: 0,
            confirm_skip: false,
            missing: MissingTracker::default(),
            pending: None,
            dir,
        }
    }

    /// 初回起動なら自動で開始する。開始したら `true`。
    pub fn autostart(&mut self) -> bool {
        if should_autostart_in(&self.dir) {
            self.start();
            true
        } else {
            false
        }
    }

    /// 最初から開始する。
    pub fn start(&mut self) {
        self.active = true;
        self.idx = 0;
        self.confirm_skip = false;
        self.missing.reset();
        self.pending = STEPS.first().and_then(|s| s.pre_action);
    }

    /// あとから開き直す (コマンドパレットの「チュートリアルを再開」)。
    ///
    /// 位置は 0 に戻るが、既読フラグは**消さない** (次の起動でまた勝手に出ると
    /// うるさいため)。
    pub fn restart(&mut self) {
        self.mark_done();
        self.start();
    }

    /// いま表示中か。
    pub fn active(&self) -> bool {
        self.active
    }

    /// 現在の手順 (非表示なら `None`)。
    pub fn step(&self) -> Option<&'static Step> {
        if self.active {
            STEPS.get(self.idx)
        } else {
            None
        }
    }

    /// 現在位置 (0 始まり)。
    pub fn index(&self) -> usize {
        self.idx
    }

    /// 進捗表示用の文字列 (例: `3 / 26`)。
    pub fn progress(&self) -> String {
        format!("{} / {}", self.idx + 1, STEPS.len())
    }

    /// 次へ。最後の手順で呼ぶと完了して終了する。
    pub fn next(&mut self) -> Nav {
        self.confirm_skip = false;
        if self.idx + 1 < STEPS.len() {
            self.idx += 1;
            self.on_step_changed();
            Nav::Moved
        } else {
            self.complete();
            Nav::Completed
        }
    }

    /// 戻る。最初の手順では何も起きない。
    pub fn back(&mut self) {
        self.confirm_skip = false;
        if self.idx > 0 {
            self.idx -= 1;
            self.on_step_changed();
        }
    }

    /// スキップ (即終了)。既読フラグは立てる。
    pub fn skip(&mut self) {
        self.active = false;
        self.confirm_skip = false;
        self.pending = None;
        self.missing.reset();
        self.mark_done();
    }

    /// 最後まで見終わった。
    pub fn complete(&mut self) {
        self.skip();
    }

    /// Esc を押されたとき。最初の手順なら即終了、それ以降は確認を出す。
    ///
    /// 確認中にもう一度 Esc なら終了する (2 回押せば必ず抜けられる)。
    pub fn request_skip(&mut self) {
        if self.idx == 0 || self.confirm_skip {
            self.skip();
        } else {
            self.confirm_skip = true;
        }
    }

    /// スキップ確認を出しているか。
    pub fn confirming(&self) -> bool {
        self.confirm_skip
    }

    fn on_step_changed(&mut self) {
        self.missing.reset();
        self.pending = STEPS.get(self.idx).and_then(|s| s.pre_action);
    }

    fn mark_done(&mut self) {
        save_to(
            &self.dir,
            &Persisted {
                done: true,
                version: STEPS_VERSION,
            },
        );
    }

    /// ホストへの依頼を 1 回だけ取り出す。
    pub fn take_action(&mut self) -> Option<TutorialAction> {
        self.pending.take()
    }

    // ── 描画 ──────────────────────────────────────────────

    /// 毎フレーム 1 回、**すべての UI を描き終わったあと**に呼ぶ。
    ///
    /// 返り値はホストに実行してほしい依頼 (無ければ `None`)。非表示のときは
    /// 何も描かず、キー入力も一切奪わない。
    pub fn overlay(
        &mut self,
        ctx: &egui::Context,
        theme: &crate::theme::Theme,
        keys: &Keybinds,
    ) -> Option<TutorialAction> {
        if !self.active {
            return None;
        }
        let Some(step) = STEPS.get(self.idx) else {
            // 手順表が空 / 位置が壊れている → 閉じ込めないよう終了する。
            self.skip();
            return None;
        };

        let screen = ctx.screen_rect();
        let target = take_anchor(ctx, step.anchor);

        // アンカーが要るのに現れない手順は、しばらく待って自動で次へ。
        let now = ctx.input(|i| i.time);
        let present = step.anchor.is_none() || target.is_some();
        if self.missing.observe(now, present) {
            let act = self.pending.take();
            self.next();
            ctx.request_repaint();
            return act;
        }

        self.paint_dim(ctx, theme, target, screen);
        if let Some(t) = target {
            self.paint_ring(ctx, theme, t, now);
        }
        self.card(ctx, theme, step, target, screen, keys);

        // リングのアニメーションのため、控えめな間隔で再描画を促す。
        ctx.request_repaint_after(std::time::Duration::from_millis(REPAINT_MS));

        self.handle_keys(ctx);
        self.pending.take()
    }

    /// 暗幕。`interactable` な Area ではなく素の painter なので、背面のクリックは
    /// そのまま通る (= ツアー中でもアプリを触れる)。
    fn paint_dim(
        &self,
        ctx: &egui::Context,
        theme: &crate::theme::Theme,
        target: Option<Rect>,
        screen: Rect,
    ) {
        let p = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            Id::new("zv-tutorial-dim"),
        ));
        let dim = Color32::from_black_alpha(dim_alpha(theme.dark));
        for r in dim_rects(target, screen) {
            p.rect_filled(r, Rounding::ZERO, dim);
        }
    }

    /// 呼吸するフォーカスリング。
    fn paint_ring(&self, ctx: &egui::Context, theme: &crate::theme::Theme, t: Rect, now: f64) {
        let p = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            Id::new("zv-tutorial-ring"),
        ));
        // 0..1 を行き来する脈動。時刻ベースなのでフレームレートに依存しない。
        let pulse = ((now * 2.2).sin() as f32) * 0.5 + 0.5;
        let pad = RING_PAD + pulse * 3.0;
        let ring = t.expand(pad);
        let rounding = Rounding::same(6.0);
        // 外側にうっすら、内側にくっきりの 2 重線。
        p.rect_stroke(
            ring.expand(3.0),
            rounding,
            Stroke::new(2.5_f32, theme.accent.gamma_multiply(0.45 + pulse * 0.35)),
        );
        p.rect_stroke(ring, rounding, Stroke::new(2.0_f32, theme.accent));
    }

    /// 説明カード。位置は前フレームの実測サイズから決める (1 フレームで収束する)。
    fn card(
        &mut self,
        ctx: &egui::Context,
        theme: &crate::theme::Theme,
        step: &'static Step,
        target: Option<Rect>,
        screen: Rect,
        keys: &Keybinds,
    ) {
        let size_id = Id::new(("zv-tutorial-card-size", step.id));
        let measured: Vec2 = ctx
            .data(|d| d.get_temp::<Vec2>(size_id))
            .unwrap_or(Vec2::new(CARD_W, CARD_H_GUESS));
        let want = Vec2::new(CARD_W.min(screen.width() - 2.0 * MARGIN), measured.y);
        let place = place_callout(target, screen, want);

        let inner_w = place.rect.width();
        let resp = egui::Area::new(Id::new("zv-tutorial-card"))
            .order(egui::Order::Foreground)
            .fixed_pos(place.rect.min)
            .interactable(true)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(theme.panel)
                    .stroke(Stroke::new(1.0_f32, theme.accent))
                    .rounding(Rounding::same(10.0))
                    .inner_margin(egui::Margin::same(12.0))
                    .show(ui, |ui| {
                        ui.set_max_width((inner_w - 26.0).max(80.0));
                        self.card_contents(ui, theme, step, keys);
                    });
            });
        let got = resp.response.rect.size();
        if (got.y - measured.y).abs() > 0.5 {
            ctx.data_mut(|d| d.insert_temp(size_id, got));
            ctx.request_repaint();
        }
    }

    fn card_contents(
        &mut self,
        ui: &mut egui::Ui,
        theme: &crate::theme::Theme,
        step: &'static Step,
        keys: &Keybinds,
    ) {
        // ── 見出し行: 章名 + 進捗
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(tr(step.chapter.label()))
                    .small()
                    .color(theme.accent),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(self.progress())
                        .small()
                        .color(theme.text_dim),
                );
            });
        });
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(tr(step.title))
                .strong()
                .size(15.0)
                .color(theme.text),
        );
        ui.add_space(4.0);
        ui.label(egui::RichText::new(tr(step.body)).color(theme.text_dim));

        // 打鍵はキーバインド表から作る (ベタ書きは再割り当てと OS 差で嘘になる)
        let key_chip: Option<String> = if step.keys.is_empty() {
            step.hint.map(tr)
        } else {
            Some(
                step.keys
                    .iter()
                    .map(|a| keys.label(*a))
                    .collect::<Vec<_>>()
                    .join(" / "),
            )
        };
        if let Some(hint) = key_chip {
            ui.add_space(6.0);
            egui::Frame::none()
                .fill(theme.panel_alt)
                .stroke(Stroke::new(1.0_f32, theme.border))
                .rounding(Rounding::same(4.0))
                .inner_margin(egui::Margin::symmetric(6.0, 2.0))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(hint)
                            .small()
                            .monospace()
                            .color(theme.text),
                    );
                });
        }

        // アンカー待ちのときは、その事実を隠さずに書く。
        if self.missing.waiting() {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(tr("(この画面はいま開いていません。まもなく次へ進みます)"))
                    .small()
                    .color(theme.warn),
            );
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        if self.confirm_skip {
            ui.label(egui::RichText::new(tr("チュートリアルを終了しますか?")).color(theme.text));
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button(tr("終了する")).clicked() {
                    self.skip();
                }
                if ui.button(tr("続ける")).clicked() {
                    self.confirm_skip = false;
                }
            });
            return;
        }

        let last = self.idx + 1 >= STEPS.len();
        ui.horizontal_wrapped(|ui| {
            // スキップは常に最初に置く = いつでも 1 クリックで抜けられる。
            if ui
                .button(egui::RichText::new(tr("スキップ")).color(theme.text_dim))
                .on_hover_text(tr("Esc でも終了できます"))
                .clicked()
            {
                self.request_skip();
            }
            if self.idx > 0 && ui.button(tr("← 戻る")).clicked() {
                self.back();
            }
            let next_label = if last { tr("完了") } else { tr("次へ →") };
            if ui.button(next_label).clicked() {
                self.next();
            }
        });
    }

    /// キーボード操作。`consume_key` なのでアプリ側には流れない。
    ///
    /// ただし**入力欄にフォーカスがあるときは一切奪わない**。ツアー中に端末や
    /// 検索欄へ打ち始めた人から Enter / Esc を取り上げないため (スキップは
    /// カードのボタンから常に届く)。
    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let (next, back, esc) = ctx.input_mut(|i| {
            let m = egui::Modifiers::NONE;
            (
                i.consume_key(m, egui::Key::ArrowRight) || i.consume_key(m, egui::Key::Enter),
                i.consume_key(m, egui::Key::ArrowLeft),
                i.consume_key(m, egui::Key::Escape),
            )
        });
        if esc {
            self.request_skip();
        } else if next {
            if self.confirm_skip {
                self.confirm_skip = false;
            } else {
                self.next();
            }
        } else if back {
            self.back();
        }
    }
}

// ── テスト ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h))
    }

    /// 画面内に完全に収まっているか (境界は許容)。
    fn inside(r: Rect, screen: Rect) -> bool {
        r.min.x >= screen.min.x - 0.01
            && r.min.y >= screen.min.y - 0.01
            && r.max.x <= screen.max.x + 0.01
            && r.max.y <= screen.max.y + 0.01
    }

    // ── 手順表の整合性 ──

    #[test]
    fn step_ids_are_unique_and_non_empty() {
        let mut seen = std::collections::HashSet::new();
        for s in STEPS {
            assert!(!s.id.is_empty(), "空の id がある");
            assert!(seen.insert(s.id), "id が重複: {}", s.id);
        }
        assert!(STEPS.len() >= 15, "手順が少なすぎる: {}", STEPS.len());
    }

    #[test]
    fn every_step_has_title_and_body() {
        for s in STEPS {
            assert!(!s.title.trim().is_empty(), "{}: title が空", s.id);
            assert!(!s.body.trim().is_empty(), "{}: body が空", s.id);
            // 「3 文以内」を機械的に担保する (。で数える)。
            let sentences = s.body.matches('。').count();
            assert!(sentences <= 3, "{}: 文が多すぎる ({})", s.id, sentences);
        }
    }

    #[test]
    fn every_referenced_anchor_is_registered() {
        for s in STEPS {
            if let Some(a) = s.anchor {
                assert!(
                    ALL_ANCHORS.contains(&a),
                    "{}: ALL_ANCHORS に無いアンカー {:?}",
                    s.id,
                    a
                );
            }
        }
    }

    #[test]
    fn anchor_list_has_no_duplicates_and_stable_keys() {
        let mut ids = std::collections::HashSet::new();
        let mut keys = std::collections::HashSet::new();
        for a in ALL_ANCHORS {
            assert!(ids.insert(*a), "ALL_ANCHORS が重複: {a:?}");
            assert!(keys.insert(a.key()), "key() が重複: {}", a.key());
            assert!(!a.key().is_empty());
        }
    }

    /// 使われていないアンカーがあると、配線仕様に無駄な指示が残る。
    #[test]
    fn every_anchor_is_used_by_some_step() {
        for a in ALL_ANCHORS {
            assert!(
                STEPS.iter().any(|s| s.anchor == Some(*a)),
                "どの手順からも使われていないアンカー: {a:?}"
            );
        }
    }

    #[test]
    fn chapters_are_contiguous() {
        let mut order: Vec<Chapter> = Vec::new();
        for s in STEPS {
            if order.last() != Some(&s.chapter) {
                assert!(
                    !order.contains(&s.chapter),
                    "章が飛び飛びになっている: {:?} ({})",
                    s.chapter,
                    s.id
                );
                order.push(s.chapter);
            }
        }
        assert!(order.len() >= 3, "章が少なすぎる");
        assert_eq!(order.first(), Some(&Chapter::Welcome));
        assert_eq!(order.last(), Some(&Chapter::Finish));
        for c in &order {
            assert!(!c.label().is_empty());
        }
    }

    // ── 進行の状態機械 ──

    fn tut(tag: &str) -> Tutorial {
        Tutorial::in_dir(unique_temp_dir("zaivern-tutorial-test", tag))
    }

    #[test]
    fn next_walks_to_the_end_then_completes() {
        let mut t = tut("walk");
        t.start();
        assert!(t.active());
        for i in 0..STEPS.len() - 1 {
            assert_eq!(t.index(), i);
            assert_eq!(t.next(), Nav::Moved);
        }
        assert_eq!(t.next(), Nav::Completed);
        assert!(!t.active(), "完了後も表示され続けている");
        assert!(t.step().is_none());
        assert!(!should_autostart_in(&t.dir), "完了でフラグが立っていない");
    }

    #[test]
    fn back_is_bounded_at_zero() {
        let mut t = tut("back");
        t.start();
        t.back();
        t.back();
        assert_eq!(t.index(), 0);
        t.next();
        t.back();
        assert_eq!(t.index(), 0);
    }

    #[test]
    fn skip_works_from_any_step() {
        for i in 0..STEPS.len() {
            let mut t = tut(&format!("skip{i}"));
            t.start();
            for _ in 0..i {
                t.next();
            }
            t.skip();
            assert!(!t.active(), "手順 {i} からスキップできない");
            assert!(!should_autostart_in(&t.dir));
        }
    }

    #[test]
    fn esc_confirms_only_past_the_first_step() {
        let mut t = tut("esc");
        t.start();
        t.request_skip();
        assert!(!t.active(), "最初の手順では確認せず即終了するはず");

        let mut t = tut("esc2");
        t.start();
        t.next();
        t.request_skip();
        assert!(t.active() && t.confirming(), "確認が出ていない");
        // 2 回目で必ず抜けられる = 閉じ込めない
        t.request_skip();
        assert!(!t.active());
    }

    #[test]
    fn navigation_clears_the_skip_confirmation() {
        let mut t = tut("confirm-clear");
        t.start();
        t.next();
        t.request_skip();
        assert!(t.confirming());
        t.next();
        assert!(!t.confirming());
        t.request_skip();
        t.back();
        assert!(!t.confirming());
    }

    #[test]
    fn restart_rewinds_but_keeps_the_done_flag() {
        let mut t = tut("restart");
        t.start();
        t.next();
        t.next();
        t.restart();
        assert_eq!(t.index(), 0);
        assert!(t.active());
        // 既読フラグは残る = 次回起動で勝手に出ない
        assert!(!should_autostart_in(&t.dir));
    }

    #[test]
    fn pre_action_is_handed_over_once_per_step() {
        let mut t = tut("action");
        t.start();
        // 最初の手順に依頼が無いことを確認しつつ、依頼のある手順まで進める
        let mut delivered = 0;
        for _ in 0..STEPS.len() {
            let want = t.step().and_then(|s| s.pre_action);
            let got = t.take_action();
            assert_eq!(got, want, "手順 {} の依頼が一致しない", t.index());
            if got.is_some() {
                delivered += 1;
            }
            // 2 回目は必ず None (1 手順につき 1 回だけ)
            assert_eq!(t.take_action(), None);
            if t.next() == Nav::Completed {
                break;
            }
        }
        assert!(delivered >= 5, "依頼付きの手順が少なすぎる: {delivered}");
    }

    // ── カード配置 ──

    #[test]
    fn callout_stays_on_screen_for_targets_everywhere() {
        let screen = rect(0.0, 0.0, 1200.0, 800.0);
        let card = Vec2::new(CARD_W, 200.0);
        let targets = [
            ("左上", rect(0.0, 0.0, 60.0, 40.0)),
            ("右上", rect(1140.0, 0.0, 60.0, 40.0)),
            ("左下", rect(0.0, 760.0, 60.0, 40.0)),
            ("右下", rect(1140.0, 760.0, 60.0, 40.0)),
            ("上辺中央", rect(570.0, 0.0, 60.0, 30.0)),
            ("下辺中央", rect(570.0, 770.0, 60.0, 30.0)),
            ("左辺中央", rect(0.0, 380.0, 30.0, 40.0)),
            ("右辺中央", rect(1170.0, 380.0, 30.0, 40.0)),
            ("中央", rect(500.0, 350.0, 200.0, 100.0)),
            ("縦長サイドバー", rect(0.0, 0.0, 240.0, 800.0)),
            ("横長ツールバー", rect(0.0, 0.0, 1200.0, 36.0)),
        ];
        for (name, t) in targets {
            let p = place_callout(Some(t), screen, card);
            assert!(inside(p.rect, screen), "{name}: 画面外へ出た {:?}", p.rect);
            assert!(
                p.rect.width() > 0.0 && p.rect.height() > 0.0,
                "{name}: 潰れた"
            );
        }
    }

    #[test]
    fn callout_prefers_the_side_with_room() {
        let screen = rect(0.0, 0.0, 1200.0, 800.0);
        let card = Vec2::new(300.0, 200.0);
        // 左端のサイドバー → 右に出る
        assert_eq!(
            place_callout(Some(rect(0.0, 100.0, 240.0, 600.0)), screen, card).side,
            Side::Right
        );
        // 右端のボタン → 左に出る
        assert_eq!(
            place_callout(Some(rect(1100.0, 100.0, 90.0, 30.0)), screen, card).side,
            Side::Left
        );
        // 画面いっぱいの横帯 (左右に余地なし・上に余地なし) → 下に出る
        assert_eq!(
            place_callout(Some(rect(0.0, 0.0, 1200.0, 40.0)), screen, card).side,
            Side::Below
        );
        // 画面いっぱいの横帯が最下部 → 上に出る
        assert_eq!(
            place_callout(Some(rect(0.0, 760.0, 1200.0, 40.0)), screen, card).side,
            Side::Above
        );
    }

    #[test]
    fn callout_falls_back_to_center_when_nothing_fits() {
        let screen = rect(0.0, 0.0, 1200.0, 800.0);
        let card = Vec2::new(CARD_W, 200.0);
        // 対象が画面より大きい → どこにも入らない
        let p = place_callout(Some(rect(-100.0, -100.0, 1500.0, 1100.0)), screen, card);
        assert_eq!(p.side, Side::Center);
        assert!(inside(p.rect, screen));
        // アンカーが無い場合も中央
        let p = place_callout(None, screen, card);
        assert_eq!(p.side, Side::Center);
        assert!(inside(p.rect, screen));
    }

    #[test]
    fn callout_shrinks_for_tiny_windows() {
        // カードより小さいウィンドウでも、はみ出さず潰れない
        for (w, h) in [(320.0_f32, 240.0_f32), (200.0, 120.0), (60.0, 40.0)] {
            let screen = rect(0.0, 0.0, w, h);
            let card = Vec2::new(CARD_W, 260.0);
            for target in [None, Some(rect(0.0, 0.0, w * 0.5, 20.0))] {
                let p = place_callout(target, screen, card);
                assert!(inside(p.rect, screen), "{w}x{h}: 画面外 {:?}", p.rect);
                assert!(p.rect.width() > 0.0 && p.rect.height() > 0.0);
            }
        }
    }

    #[test]
    fn callout_handles_offscreen_targets() {
        let screen = rect(0.0, 0.0, 1200.0, 800.0);
        let card = Vec2::new(CARD_W, 200.0);
        // スクロールで画面外へ出た要素 → 中央フォールバック
        let p = place_callout(Some(rect(-500.0, -500.0, 100.0, 100.0)), screen, card);
        assert_eq!(p.side, Side::Center);
        assert!(inside(p.rect, screen));
    }

    /// 画面原点が (0,0) でない場合 (egui の screen_rect は常に 0 始まりだが念のため)。
    #[test]
    fn callout_respects_non_zero_origin() {
        let screen = rect(100.0, 50.0, 800.0, 600.0);
        let card = Vec2::new(300.0, 200.0);
        let p = place_callout(Some(rect(100.0, 50.0, 200.0, 40.0)), screen, card);
        assert!(inside(p.rect, screen), "{:?}", p.rect);
    }

    // ── 暗幕のくり抜き ──

    /// 暗幕はテーマに応じて薄くする。
    ///
    /// 既定のダークテーマに濃い黒を重ねると説明中だけ画面が読めなくなる。
    #[test]
    fn 暗幕はダークテーマでは薄い() {
        assert!(dim_alpha(true) < dim_alpha(false), "暗いテーマほど薄く");
        assert!(dim_alpha(true) <= 80, "ダークテーマで濃すぎない");
        assert!(dim_alpha(false) <= 128, "ライトテーマでも真っ暗にはしない");
    }

    #[test]
    fn dim_rects_punch_a_hole_and_never_cover_the_target() {
        let screen = rect(0.0, 0.0, 1000.0, 600.0);
        let t = rect(200.0, 100.0, 300.0, 200.0);
        let rects = dim_rects(Some(t), screen);
        assert_eq!(rects.len(), 4);
        for r in &rects {
            assert!(inside(*r, screen));
            let hit = r.intersect(t);
            assert!(!hit.is_positive(), "対象を覆っている: {r:?}");
        }
        // 対象が無ければ全面 1 枚
        assert_eq!(dim_rects(None, screen), vec![screen]);
    }

    #[test]
    fn dim_rects_handle_edge_targets() {
        let screen = rect(0.0, 0.0, 1000.0, 600.0);
        // 左上に密着 → 上と左の帯が消えて 2 枚
        assert_eq!(
            dim_rects(Some(rect(0.0, 0.0, 200.0, 100.0)), screen).len(),
            2
        );
        // 全画面の対象 → 暗幕なし
        assert!(dim_rects(Some(screen), screen).is_empty());
        // 画面外の対象 → 全面 1 枚
        assert_eq!(
            dim_rects(Some(rect(2000.0, 2000.0, 10.0, 10.0)), screen),
            vec![screen]
        );
    }

    // ── アンカー欠落 ──

    #[test]
    fn missing_tracker_advances_after_the_timeout() {
        let mut m = MissingTracker::default();
        assert!(!m.observe(0.0, false), "いきなり自動送りしてはいけない");
        assert!(m.waiting());
        assert!(!m.observe(MISSING_ANCHOR_TIMEOUT - 0.1, false));
        assert!(
            m.observe(MISSING_ANCHOR_TIMEOUT, false),
            "時間切れで進むはず"
        );
    }

    #[test]
    fn missing_tracker_resets_when_the_anchor_shows_up() {
        let mut m = MissingTracker::default();
        m.observe(0.0, false);
        assert!(m.waiting());
        // 遅れてパネルが開いた
        assert!(!m.observe(1.0, true));
        assert!(!m.waiting());
        // 以後いくら経っても自動送りしない
        assert!(!m.observe(100.0, true));
    }

    #[test]
    fn missing_tracker_reset_clears_the_timer() {
        let mut m = MissingTracker::default();
        m.observe(0.0, false);
        m.reset();
        assert!(!m.waiting());
        assert!(
            !m.observe(MISSING_ANCHOR_TIMEOUT, false),
            "reset 後は数え直し"
        );
    }

    // ── 永続化 ──

    #[test]
    fn persistence_round_trip() {
        let dir = unique_temp_dir("zaivern-tutorial-test", "persist");
        assert!(should_autostart_in(&dir), "初回起動なのに出ない");
        let st = Persisted {
            done: true,
            version: STEPS_VERSION,
        };
        save_to(&dir, &st);
        assert_eq!(load_from(&dir), st);
        assert!(!should_autostart_in(&dir), "2 回目も出てしまう");
    }

    #[test]
    fn missing_state_file_means_first_run() {
        let dir = unique_temp_dir("zaivern-tutorial-test", "missing");
        assert_eq!(load_from(&dir), Persisted::default());
        assert!(should_autostart_in(&dir));
        // 存在しないディレクトリでも壊れない
        let ghost = dir.join("nope").join("deeper");
        assert!(should_autostart_in(&ghost));
    }

    #[test]
    fn corrupt_state_file_falls_back_to_first_run() {
        let dir = unique_temp_dir("zaivern-tutorial-test", "corrupt");
        for junk in ["", "not toml at all {{{", "done = \"yes\"", "\u{0}\u{1}"] {
            std::fs::write(dir.join(STATE_FILE), junk).unwrap();
            assert_eq!(load_from(&dir), Persisted::default(), "junk={junk:?}");
            assert!(should_autostart_in(&dir), "junk={junk:?}");
        }
        // 壊れたファイルの上からでも保存できる
        save_to(
            &dir,
            &Persisted {
                done: true,
                version: STEPS_VERSION,
            },
        );
        assert!(!should_autostart_in(&dir));
    }

    #[test]
    fn a_newer_step_table_shows_the_tour_again() {
        let dir = unique_temp_dir("zaivern-tutorial-test", "version");
        save_to(
            &dir,
            &Persisted {
                done: true,
                version: 0,
            },
        );
        assert!(should_autostart_in(&dir), "版が古ければもう一度出す");
    }

    #[test]
    fn autostart_only_fires_once() {
        let dir = unique_temp_dir("zaivern-tutorial-test", "autostart");
        let mut t = Tutorial::in_dir(dir.clone());
        assert!(t.autostart());
        assert!(t.active());
        t.complete();
        let mut t2 = Tutorial::in_dir(dir);
        assert!(!t2.autostart());
        assert!(!t2.active());
    }

    #[test]
    fn inactive_tour_reports_nothing() {
        let mut t = tut("inactive");
        assert!(!t.active());
        assert!(t.step().is_none());
        assert_eq!(t.take_action(), None);
        // 開始していない状態で next/back を呼んでも壊れない
        t.back();
        assert_eq!(t.index(), 0);
    }

    #[test]
    fn progress_string_is_one_based() {
        let mut t = tut("progress");
        t.start();
        assert_eq!(t.progress(), format!("1 / {}", STEPS.len()));
        t.next();
        assert_eq!(t.progress(), format!("2 / {}", STEPS.len()));
    }
}
