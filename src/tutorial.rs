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
//!
//! ## 案内は 3 層に分ける
//!
//! 「新機能が案内に載っていない」と言われて **[`STEPS`] を伸ばすのは誤り**。
//! 初回起動で 80 枚のカードを送らされるのは競合が最も批判されている作りで、
//! しかも手書きの一覧は次の機能でまた腐る。層を分けて、いちばん腐りやすい
//! 一覧だけを**自動生成**にしてある。
//!
//! | 層 | 実体 | いつ出るか |
//! |----|------|-----------|
//! | 1. 初回ツアー | [`STEPS`] | 初回起動で自動再生。**短いまま増やさない** |
//! | 2. 章立てガイド | [`WALKTHROUGHS`] + [`EXTRA_STEPS`] | ガイドから章を選んだとき。進捗つき・途中再開 |
//! | 3. 全機能索引 | [`index_rows`] (**自動生成**) | ガイドを開いたとき。絞り込み検索つき |
//!
//! 第 3 層は [`crate::feature::REGISTRY`] と [`crate::keybinds::ALL_ACTIONS`]
//! から作る。**`src/features/<名前>.rs` を 1 つ置けば、このファイルを 1 バイトも
//! 触らずに索引へ載る** (このリポジトリの「機能を足すときは共有ファイルを
//! 触らない」規約に案内も乗せる)。番人テスト
//! `索引はレジストリの全機能を必ず載せる` が抜けをゼロで固定している。
//!
//! 説明文は [`FEATURE_NOTES`] で足せるが、**表に無い機能も必ず一覧には出る**
//! (説明が空になるだけ)。ここを取り違えると「新機能に対応していない」が再発する。

use crate::i18n::tr;
use crate::keybinds::{BindAction, Keybinds};
use eframe::egui;
use egui::{Color32, Id, Pos2, Rect, Rounding, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
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
    // **未配線の API。** app.rs から呼ばれていないので `dead_code` が鳴る。
    // モジュール全体に `allow` を掛けると「作ったのに繋いでいない」が
    // 見えなくなるので、**分かっている分だけ**を名指しで黙らせる。
    #[allow(dead_code)]
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
// テストからしか読まないが、**配線漏れの番人そのもの**なので消さない。
#[allow(dead_code)]
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
        body: "あとは走らせるだけ。全機能の索引と章ごとの拾い読みは、下の「全機能ガイド」から (ヘルプ > チュートリアルを再開 でここへも戻れます)。\nよい夜を。",
        hint: None,
        keys: &[BindAction::PaletteCommands],
        pre_action: None,
    },
];

// ── 追加手順 (章立てガイド専用) ──────────────────────────────

/// 章立てガイド ([`WALKTHROUGHS`]) からだけ届く手順。
///
/// **[`STEPS`] には 1 件も足さない。** 初回ツアーは「操縦席の形が分かる」
/// までで終わるべきで、そこへ全機能を積むと 80 枚のカードを送らされる
/// (競合が最も批判されている作り) 。増えた機能はこちらへ足し、
/// [`WALKTHROUGHS`] の章から拾い読みできるようにする。
///
/// id は [`STEPS`] と重複させないこと (番人テストが落とす)。
pub const EXTRA_STEPS: &[Step] = &[
    Step {
        id: "tabs_nav",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::EditorTabs),
        title: "タブを行き来する",
        body: "隣のタブへ、最近使った順で切り替え、閉じたタブを開き直す、までが打鍵で届きます。\n分割していても同じ打鍵で動きます。",
        hint: None,
        keys: &[
            BindAction::NextTab,
            BindAction::SwitchTab,
            BindAction::ReopenClosedTab,
        ],
        pre_action: None,
    },
    Step {
        id: "editing",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::EditorBody),
        title: "行を編む",
        body: "行コメントの切り替え、行の複製、行の上下移動が打鍵で届きます。\n元に戻す / やり直しはタブごとに独立しています。",
        hint: None,
        keys: &[
            BindAction::ToggleComment,
            BindAction::DuplicateLine,
            BindAction::MoveLineDown,
        ],
        pre_action: None,
    },
    Step {
        id: "multi_cursor",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::EditorBody),
        title: "複数キャレット",
        body: "選択した語と同じ次の出現を足していくと、キャレットが増えて同時に打てます。\n同じ名前をまとめて直すときに使います。",
        hint: None,
        keys: &[BindAction::SelectNextOccurrence],
        pre_action: None,
    },
    Step {
        id: "folding",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::EditorBody),
        title: "折りたたみ",
        body: "インデントの構造で折りたたみ、まとめて展開できます。\n長いファイルの全体像を掴むときに使います。",
        hint: None,
        keys: &[BindAction::ToggleFold, BindAction::UnfoldAll],
        pre_action: None,
    },
    Step {
        id: "split_editor",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::EditorTabs),
        title: "エディタの分割",
        body: "右へ / 下へ分割して、ペインの番号で行き来できます。\n差分と実装を並べて見るときに便利です。",
        hint: None,
        keys: &[
            BindAction::SplitEditorRight,
            BindAction::SplitEditorDown,
            BindAction::FocusPane2,
        ],
        pre_action: None,
    },
    Step {
        id: "goto",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::EditorBody),
        title: "行・シンボル・定義へ飛ぶ",
        body: "行と列を指定して飛ぶ、シンボル一覧から飛ぶ、定義へ飛ぶ、対応する括弧へ飛ぶ。\nどれも打鍵ひとつで届きます。",
        hint: None,
        keys: &[
            BindAction::GoToLine,
            BindAction::LspSymbols,
            BindAction::GoToDefinition,
            BindAction::GoToBracket,
        ],
        pre_action: None,
    },
    Step {
        id: "nav_history",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::EditorBody),
        title: "戻る・進む",
        body: "飛んだ先から元の場所へ戻れます。\nブラウザと同じ感覚で行き来できます。",
        hint: None,
        keys: &[BindAction::NavBack, BindAction::NavForward],
        pre_action: None,
    },
    Step {
        id: "bookmarks",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::EditorBody),
        title: "ブックマーク",
        body: "気になる行に印を付け、一覧からいつでも飛べます。\nニーモニック付きで付けておくと 1 文字で呼び出せます。",
        hint: None,
        keys: &[
            BindAction::ToggleBookmark,
            BindAction::MarksPanel,
            BindAction::MarkJump,
        ],
        pre_action: None,
    },
    Step {
        id: "lsp",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::EditorBody),
        title: "補完・整形・名前の変更",
        body: "言語サーバが繋がると、補完・参照検索・名前の変更・整形・クイックフィックスが使えます。\n保存時の自動整形はコマンドパレットから切り替えます。",
        hint: None,
        keys: &[
            BindAction::LspCompletion,
            BindAction::LspRename,
            BindAction::LspFormat,
            BindAction::LspCodeAction,
        ],
        pre_action: None,
    },
    Step {
        id: "problems",
        chapter: Chapter::Editor,
        anchor: None,
        title: "問題パネルとビルドタスク",
        body: "診断をまとめて開き、次 / 前の問題へ順に飛べます。\nビルドタスクの結果もここに集まります。",
        hint: None,
        keys: &[
            BindAction::ToggleProblems,
            BindAction::NextProblem,
            BindAction::RunBuildTask,
        ],
        pre_action: None,
    },
    Step {
        id: "preview",
        chapter: Chapter::Editor,
        anchor: Some(AnchorId::EditorBody),
        title: "Markdown と HTML のプレビュー",
        body: "編集中の内容をそのまま組んだ表示に切り替えられます。\n表・コードブロック・リンクも読める形で出ます。",
        hint: None,
        keys: &[BindAction::ToggleMdPreview],
        pre_action: None,
    },
    Step {
        id: "terminal_multi",
        chapter: Chapter::Agents,
        anchor: Some(AnchorId::TerminalPanel),
        title: "ターミナルを増やす",
        body: "エージェントとは別に、素のシェルを何本でも開けます。\nパネルそのものの開閉も打鍵で届きます。",
        hint: None,
        keys: &[BindAction::NewTerminal, BindAction::ToggleTerminal],
        pre_action: Some(TutorialAction::ShowTerminalPanel),
    },
    Step {
        id: "agents_tab",
        chapter: Chapter::Agents,
        anchor: None,
        title: "サイドバーのエージェント一覧",
        body: "走っている機を縦の一覧で見て、選んだ機の端末へすぐ移れます。\n狭い画面ではこちらのほうが速く辿り着けます。",
        hint: None,
        keys: &[BindAction::ToggleSidebar],
        pre_action: Some(TutorialAction::OpenSidebar(SidebarTarget::Agents)),
    },
    Step {
        id: "follow",
        chapter: Chapter::Agents,
        anchor: Some(AnchorId::TerminalPanel),
        title: "エージェントを追従する",
        body: "出力が進んだ機へ自動で視点を移し、解除すればその場に留まります。\n見失ったら追従を再開できます。",
        hint: None,
        keys: &[BindAction::FollowAgent, BindAction::FollowResume],
        pre_action: None,
    },
    Step {
        id: "unread",
        chapter: Chapter::Agents,
        anchor: Some(AnchorId::TerminalPanel),
        title: "未読を巡回する",
        body: "手が要る機だけを順に回れます。\n今は後回しにしたい機は、未読へ戻して次へ送れます。",
        hint: None,
        keys: &[
            BindAction::NextUnread,
            BindAction::DeferUnread,
            BindAction::ToggleUnread,
        ],
        pre_action: None,
    },
    Step {
        id: "quick_launch",
        chapter: Chapter::Agents,
        anchor: Some(AnchorId::Toolbar),
        title: "起動バー",
        body: "よく使うエージェントを 9 つまで並べ、番号の打鍵で起動できます。\n中身は config.toml のプリセットなので、いくつでも入れ替えられます。",
        hint: None,
        keys: &[BindAction::QuickLaunch1, BindAction::QuickLaunch2],
        pre_action: None,
    },
    Step {
        id: "approvals",
        chapter: Chapter::Agents,
        anchor: Some(AnchorId::CommandPalette),
        title: "承認キューと監査ログ",
        body: "全エージェントの承認待ちを 1 本の列にまとめ、ここで許可 / 拒否します。\n決めた内容は監査ログに残るので、誰が何を通したかを後から辿れます。",
        hint: None,
        keys: &[BindAction::PaletteCommands],
        pre_action: Some(TutorialAction::OpenPalette),
    },
    Step {
        id: "acp",
        chapter: Chapter::Agents,
        anchor: Some(AnchorId::CommandPalette),
        title: "ACP — 構造化プロトコルで繋ぐ",
        body: "画面の文字を読むのではなく、エージェント自身が状態を送ってくる経路です。\n対応している相手なら、停滞や承認待ちの判定が推測ではなくなります。",
        hint: None,
        keys: &[],
        pre_action: Some(TutorialAction::OpenPalette),
    },
    Step {
        id: "git_gutter",
        chapter: Chapter::Review,
        anchor: Some(AnchorId::EditorBody),
        title: "ガターで差分と診断を読む",
        body: "行番号の左に、git の追加 / 変更と言語サーバの警告 / エラーが同時に出ます。\n編集している最中でも、コミット前の差分が分かります。",
        hint: None,
        keys: &[],
        pre_action: None,
    },
    Step {
        id: "diff_nav",
        chapter: Chapter::Review,
        anchor: Some(AnchorId::DiffView),
        title: "差分を渡り歩く",
        body: "次 / 前の変更へ、次 / 前の差分ファイルへ、打鍵だけで移動できます。\n大きな PR でもスクロール位置を探さずに済みます。",
        hint: None,
        keys: &[
            BindAction::DiffNextChange,
            BindAction::DiffPrevChange,
            BindAction::DiffNextFile,
        ],
        pre_action: None,
    },
    Step {
        id: "conflict_zero",
        chapter: Chapter::Review,
        anchor: Some(AnchorId::CommandPalette),
        title: "競合ゼロ — 同じ行を 2 人に配らない",
        body: "並列で走らせる価値は、あとの衝突解決の手間で相殺されます。\n配る前に担当を互いに素へ分け、ファイルと行域の所有を台帳で主張し、実際に書かれた差分だけを見て「一撃で入る」と言い切ります。",
        hint: None,
        keys: &[],
        pre_action: Some(TutorialAction::OpenPalette),
    },
    Step {
        id: "czero_setup",
        chapter: Chapter::Review,
        anchor: Some(AnchorId::CommandPalette),
        title: "このリポジトリを競合ゼロにする",
        body: "導入と自己診断をひと続きで実行し、いま自分がどこまで守られているかを点検できます。\nどのリポジトリでも同じ条件で判定するので、環境ごとの当たり外れがありません。",
        hint: None,
        keys: &[],
        pre_action: Some(TutorialAction::OpenPalette),
    },
    Step {
        id: "merge_train",
        chapter: Chapter::Review,
        anchor: Some(AnchorId::CommandPalette),
        title: "マージトレインと追記の自動マージ",
        body: "順番を決めて 1 本ずつ流すので、止まる回数が減ります。\n一覧への追記のような「足すだけ」の変更は、そもそも衝突させません。",
        hint: None,
        keys: &[],
        pre_action: Some(TutorialAction::OpenPalette),
    },
    Step {
        id: "semantic",
        chapter: Chapter::Review,
        anchor: Some(AnchorId::CommandPalette),
        title: "意味的衝突と衝突レーダー",
        body: "ファイルは違うのに噛み合わない変更を見つけます。\n近い行の衝突は、走行中のワークツリーを突き合わせて先に見せます。",
        hint: None,
        keys: &[],
        pre_action: Some(TutorialAction::OpenPalette),
    },
    Step {
        id: "mesh_spec",
        chapter: Chapter::Review,
        anchor: Some(AnchorId::CommandPalette),
        title: "メッシュと Spec",
        body: "並列エージェントの通信と、落ちた担当の自動解放をここで見ます。\nSpec は仕様と実装のずれ・陳腐化を追いかけます。",
        hint: None,
        keys: &[],
        pre_action: Some(TutorialAction::OpenPalette),
    },
    Step {
        id: "mcp_skills",
        chapter: Chapter::Extend,
        anchor: Some(AnchorId::CommandPalette),
        title: "MCP と Skills",
        body: "MCP サーバの登録と、Skills / slash コマンドの管理を画面から行えます。\nエージェントに増やした道具は、ここで一覧できます。",
        hint: None,
        keys: &[],
        pre_action: Some(TutorialAction::OpenPalette),
    },
    Step {
        id: "keybind_editor",
        chapter: Chapter::Finish,
        anchor: Some(AnchorId::ThemeMenu),
        title: "ショートカットを自分のものにする",
        body: "設定画面から 1 行ずつ割り当てを変えられ、既定へ戻すのもその場でできます。\n変えた行だけが config.toml へ書き戻ります。",
        hint: None,
        keys: &[BindAction::KeybindEditor],
        pre_action: None,
    },
    Step {
        id: "zoom",
        chapter: Chapter::Finish,
        anchor: Some(AnchorId::MenuBar),
        title: "大きさと全画面",
        body: "画面全体のズームと、このファイルだけのズームは別々に効きます。\n全画面へ切り替えても開いているものはそのまま残ります。",
        hint: None,
        keys: &[
            BindAction::ZoomIn,
            BindAction::FileZoomIn,
            BindAction::ToggleFullScreen,
        ],
        pre_action: None,
    },
];

// ── 第 2 層: 章立てガイド (Walkthroughs) ─────────────────────

/// 拾い読み用の章。**手順の実体は持たず、id で [`STEPS`] / [`EXTRA_STEPS`]
/// を参照する。**
///
/// 実体を持たせると同じ説明が 2 か所に増えて必ずズレるので、ここは
/// 並べ替えの表に徹する。1 つの手順が複数の章に出てよい
/// (「画像・PDF」は *エディタ基本* にも *プレビュー* にも要る)。
pub struct Walkthrough {
    /// 進捗の保存キー。**変えると既存ユーザーの進捗が消える**ので固定する。
    pub id: &'static str,
    pub icon: &'static str,
    pub title: &'static str,
    /// 一覧に出す 1 行の要約。
    pub summary: &'static str,
    /// この章で再生する [`Step::id`] の並び。
    pub step_ids: &'static [&'static str],
}

impl Walkthrough {
    /// この章の手順を引く (知らない id は落ちる。番人テストが禁じている)。
    pub fn steps(&self) -> Vec<&'static Step> {
        steps_by_ids(self.step_ids)
    }
}

/// 章立てガイドの目次。
///
/// VS Code の Walkthroughs と同じ形 — 初回ツアーを短く保ったまま、
/// **好きなときに好きな章だけ**拾い読みできる。進捗は章ごとに残る。
pub const WALKTHROUGHS: &[Walkthrough] = &[
    Walkthrough {
        id: "basics",
        icon: "📝",
        title: "エディタ基本",
        summary: "タブ・行の編集・複数キャレット・分割・ビューア",
        step_ids: &[
            "editor_tabs",
            "tabs_nav",
            "editing",
            "multi_cursor",
            "folding",
            "split_editor",
            "viewers",
        ],
    },
    Walkthrough {
        id: "navigate",
        icon: "🔎",
        title: "検索と移動",
        summary: "ファイル内検索・横断検索・パレット・ブックマーク",
        step_ids: &[
            "find",
            "search",
            "palette",
            "goto",
            "nav_history",
            "bookmarks",
        ],
    },
    Walkthrough {
        id: "code",
        icon: "💡",
        title: "コード支援",
        summary: "補完・整形・名前の変更・問題パネル",
        step_ids: &["lsp", "problems", "goto"],
    },
    Walkthrough {
        id: "git",
        icon: "🌿",
        title: "Git と差分",
        summary: "変更の一覧・ガター・差分レビュー・GitHub",
        step_ids: &[
            "git_panel",
            "git_gutter",
            "diff_review",
            "diff_nav",
            "github_pr",
        ],
    },
    Walkthrough {
        id: "agents",
        icon: "🤖",
        title: "AI エージェント運用",
        summary: "起動・権限・Cockpit・看板・レース・承認",
        step_ids: &[
            "new_agent",
            "permission",
            "cockpit",
            "kanban",
            "deck",
            "race",
            "agents_tab",
            "follow",
            "unread",
            "quick_launch",
            "approvals",
            "sessions",
            "acp",
        ],
    },
    Walkthrough {
        id: "terminal",
        icon: "🖥",
        title: "端末",
        summary: "本物の PTY・複数ターミナル・音声入力",
        step_ids: &["terminal", "terminal_multi", "voice"],
    },
    Walkthrough {
        id: "preview",
        icon: "👁",
        title: "プレビュー",
        summary: "Markdown / HTML・画像・PDF・折り返しと空白文字",
        step_ids: &["preview", "viewers", "view_options"],
    },
    Walkthrough {
        id: "czero",
        icon: "🔒",
        title: "競合ゼロ",
        summary: "所有・担当分割・一撃マージの証明・トレイン・レーダー",
        step_ids: &[
            "conflict_zero",
            "czero_setup",
            "merge_train",
            "semantic",
            "mesh_spec",
        ],
    },
    Walkthrough {
        id: "custom",
        icon: "🎨",
        title: "表示のカスタマイズ",
        summary: "テーマ・ショートカット・ズーム・プラグイン・外からの操作",
        step_ids: &[
            "personalize",
            "keybind_editor",
            "zoom",
            "plugins",
            "mcp_skills",
            "remote",
            "pet",
            "cli",
        ],
    },
];

/// id の並びを実体へ解決する ([`STEPS`] を先に、無ければ [`EXTRA_STEPS`])。
fn steps_by_ids(ids: &[&'static str]) -> Vec<&'static Step> {
    ids.iter()
        .filter_map(|id| {
            STEPS
                .iter()
                .find(|s| s.id == *id)
                .or_else(|| EXTRA_STEPS.iter().find(|s| s.id == *id))
        })
        .collect()
}

/// 章の進捗 `(見た手順数, 全手順数)`。**純関数**なのでテーブルで固定できる。
pub fn chapter_progress(seen: &BTreeMap<String, u32>, w: &Walkthrough) -> (usize, usize) {
    let total = w.step_ids.len();
    let done = seen.get(w.id).copied().unwrap_or(0) as usize;
    (done.min(total), total)
}

/// 進捗の印。見終わり / 途中 / 未着手 の 3 状態だけ。
pub fn progress_mark(done: usize, total: usize) -> &'static str {
    if total > 0 && done >= total {
        "✓"
    } else if done > 0 {
        "◔"
    } else {
        "·"
    }
}

// ── 第 3 層: 全機能索引 (自動生成) ───────────────────────────

/// 索引の行がどこから来たか。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowKind {
    /// [`crate::feature::REGISTRY`] の機能
    Feature,
    /// [`crate::keybinds::ALL_ACTIONS`] の組み込み操作
    Action,
}

/// 索引の 1 行。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IndexRow {
    pub kind: RowKind,
    pub icon: String,
    /// 表示名 (翻訳済み)
    pub label: String,
    /// 安定 id (`"lease.list"` / `"toggle_terminal"` 等)。絞り込みにも使う。
    pub id: String,
    /// 打鍵表記。**必ずキーバインド表から作る** (ベタ書きは再割り当てで嘘になる)。
    pub keys: String,
    /// 補足説明。[`FEATURE_NOTES`] に無ければ空。**空でも行は必ず出る。**
    pub note: String,
}

/// レジストリに説明が無い機能へ足す 1 行の補足。
///
/// **ここに無い機能も索引には必ず出る** (説明が空になるだけ)。逆にすると
/// 「新しい機能がガイドから消える」= 今回の要望が再発するので、番人テスト
/// `索引はレジストリの全機能を必ず載せる` が構造で禁じている。
pub const FEATURE_NOTES: &[(&str, &str)] = &[
    (
        "acp.open",
        "画面を読まずに、エージェント自身が送ってくる状態で判断する",
    ),
    (
        "coedit.proof",
        "実際に書かれた差分だけを見て「一撃で入る」と言い切る",
    ),
    (
        "conflict.open",
        "並列ワークツリーの近い行の衝突を、走行中に先に見せる",
    ),
    (
        "czero.open",
        "いま自分がどこまで競合から守られているかを点検する",
    ),
    (
        "czero_init.run",
        "このリポジトリへ競合ゼロを導入し、そのまま自己診断する",
    ),
    ("guard.init", "競合ゼロの関所をこのリポジトリに置く"),
    ("jump.start", "画面に出ている語へ 2 打鍵で飛ぶ"),
    (
        "lease.list",
        "同じファイル・同じ行域を 2 人に配らないための台帳",
    ),
    ("mesh.open", "並列エージェントの通信を 1 枚で見る"),
    ("mesh.reap", "落ちた担当が握ったままの所有を自動で解放する"),
    (
        "negotiate.panel",
        "断らずに、空いている行域へずらして配り直す",
    ),
    (
        "semconf.open",
        "ファイルは違うのに噛み合わない変更を見つける",
    ),
    ("spec.open", "仕様と実装のずれ・陳腐化を追いかける"),
    ("spec.stale", "陳腐化した仕様だけを絞って見る"),
    ("split.open", "配る前に、衝突し得ない担当表を作る"),
    (
        "train.open",
        "順番を決めて 1 本ずつ流す。止まる回数を減らす",
    ),
    (
        "union.open",
        "一覧への追記のような「足すだけ」の変更を衝突させない",
    ),
];

/// [`FEATURE_NOTES`] から補足を引く (無ければ空文字)。
fn note_of(id: &str) -> &'static str {
    FEATURE_NOTES
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, v)| *v)
        .unwrap_or("")
}

/// [`crate::feature::REGISTRY`] の全機能を索引の行にする。
///
/// **`src/features/<名前>.rs` を 1 つ置いたら、このファイルを 1 バイトも
/// 触らずに索引へ載る。** これがこの層の存在理由で、手書きの一覧は必ず腐る
/// (今回の要望がまさにそれだった)。
pub fn feature_rows() -> Vec<IndexRow> {
    let mut out = Vec::new();
    for f in crate::feature::REGISTRY {
        for e in f.entries {
            out.push(feature_row(e.icon, e.label, e.id));
        }
    }
    out
}

/// 登録 1 件を索引の行にする。
///
/// **説明表 ([`FEATURE_NOTES`]) を引くのは `note` 欄だけ**で、行を出すか
/// 出さないかの判断には一切使わない。ここが今回の要望の核心 — 「説明を
/// 書いた機能だけ案内に出す」にすると、次に足された機能がまた案内から消える。
///
/// 関数へ切り出してあるのは、**説明が無い場合でも行ができる**ことを実在の
/// 登録内容に依存せず固定するため。レジストリ全件にたまたま説明が付いて
/// いる間は、件数比較だけのテストは何も検査しない (実際に空回りしていた
/// のを、わざと壊す反証で見つけた)。
fn feature_row(icon: &str, label: &str, id: &str) -> IndexRow {
    IndexRow {
        kind: RowKind::Feature,
        icon: icon.to_string(),
        label: tr(label),
        id: id.to_string(),
        // 打鍵は機能側の宣言 (`Feature::binds`) から引く。宣言が無ければ
        // 空 = コマンドパレットから届く、という意味になる。
        keys: crate::keybinds::feature_default_binding(id)
            .map(crate::keybinds::format_binding)
            .unwrap_or_default(),
        note: tr(note_of(id)),
    }
}

/// 組み込み操作 ([`crate::keybinds::ALL_ACTIONS`]) を索引の行にする。
pub fn action_rows(keys: &Keybinds) -> Vec<IndexRow> {
    crate::keybinds::ALL_ACTIONS
        .iter()
        .map(|a| IndexRow {
            kind: RowKind::Action,
            icon: "·".to_string(),
            label: tr(crate::keybinds::action_label(*a)),
            id: crate::keybinds::config_name(*a).to_string(),
            // `Keybinds` から引くので、config で再割り当てされても嘘にならない。
            keys: keys.label(*a),
            note: String::new(),
        })
        .collect()
}

/// 索引の全行 (機能 → 組み込み操作の順)。
pub fn index_rows(keys: &Keybinds) -> Vec<IndexRow> {
    let mut out = feature_rows();
    out.extend(action_rows(keys));
    out
}

/// 絞り込み。名前・id・補足・打鍵のどれかに含まれれば残す (大小を無視)。
///
/// **純関数**なので、日本語・英語・空・複数語の振る舞いを表で固定できる。
pub fn row_matches(row: &IndexRow, needle: &str) -> bool {
    let n = needle.trim().to_lowercase();
    if n.is_empty() {
        return true;
    }
    let hay = format!(
        "{} {} {} {}",
        row.label.to_lowercase(),
        row.id.to_lowercase(),
        row.note.to_lowercase(),
        row.keys.to_lowercase()
    );
    n.split_whitespace().all(|t| hay.contains(t))
}

/// 章が絞り込みに引っかかるか (章名・要約・含まれる手順の見出しで判定)。
pub fn chapter_matches(w: &Walkthrough, needle: &str) -> bool {
    let n = needle.trim().to_lowercase();
    if n.is_empty() {
        return true;
    }
    let mut hay = format!("{} {} {}", tr(w.title), tr(w.summary), w.id).to_lowercase();
    for s in w.steps() {
        hay.push(' ');
        hay.push_str(&tr(s.title).to_lowercase());
    }
    n.split_whitespace().all(|t| hay.contains(t))
}

// ── ガイドの配置計算 (純粋関数) ──────────────────────────────

/// ガイドの最大寸法。狭い窓ではここまで縮む。
const GUIDE_MAX_W: f32 = 720.0;
const GUIDE_MAX_H: f32 = 620.0;
/// ガイドと画面端の余白。
const GUIDE_MARGIN: f32 = 16.0;
/// 見出し行と絞り込み欄の高さ。
const GUIDE_HEAD_H: f32 = 30.0;
const GUIDE_SEARCH_H: f32 = 28.0;
/// ガイドの枠の内側余白。
const GUIDE_PAD: f32 = 10.0;

/// ガイドの区画。**すべて画面内に収まり、互いに重ならない** (テストで固定)。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct GuideLayout {
    /// 枠全体
    pub window: Rect,
    /// 見出し + 閉じるボタン
    pub header: Rect,
    /// 絞り込み欄
    pub search: Rect,
    /// 本文 (章 + 索引のスクロール領域)
    pub body: Rect,
}

/// 画面の大きさからガイドの区画を決める。
///
/// 極端に低い窓 (1200x300 等) では見出しと絞り込みを比率で削り、**本文の
/// 高さを必ず残す**。どの寸法でも画面外へは出さない。
pub fn guide_layout(screen: Rect) -> GuideLayout {
    let m = GUIDE_MARGIN
        .min(screen.width() * 0.2)
        .min(screen.height() * 0.2)
        .max(0.0);
    let w = (screen.width() - 2.0 * m).clamp(1.0, GUIDE_MAX_W);
    let h = (screen.height() - 2.0 * m).clamp(1.0, GUIDE_MAX_H);
    let window = clamp_rect(
        Rect::from_center_size(screen.center(), Vec2::new(w, h)),
        screen,
        0.0,
    );
    // 縦の取り分。合計が枠を超えないよう比率で頭打ちにする。
    let head = GUIDE_HEAD_H.min(h * 0.25);
    let search = GUIDE_SEARCH_H.min(h * 0.25);
    let top = window.min.y;
    let header = Rect::from_min_size(window.min, Vec2::new(window.width(), head));
    let search_r = Rect::from_min_size(
        Pos2::new(window.min.x, top + head),
        Vec2::new(window.width(), search),
    );
    let body = Rect::from_min_max(Pos2::new(window.min.x, top + head + search), window.max);
    GuideLayout {
        window,
        header,
        search: search_r,
        body,
    }
}

/// 索引 1 行の列幅。**足しても可用幅を超えない** (テストで固定)。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RowCols {
    pub icon: f32,
    pub label: f32,
    pub keys: f32,
}

/// 可用幅から列幅を決める。打鍵欄は残り幅の 4 割まで。
pub fn row_cols(avail: f32, want_keys: f32) -> RowCols {
    let avail = avail.max(0.0);
    let icon = 18.0_f32.min(avail * 0.2);
    let rest = (avail - icon).max(0.0);
    let keys = want_keys.max(0.0).min(rest * 0.4);
    RowCols {
        icon,
        label: (rest - keys).max(0.0),
        keys,
    }
}

/// 長い文字列を字数で詰める (全文はホバーで見せる)。
///
/// **バイトではなく文字で数える** — 日本語を途中で割ると panic する。
pub fn ellipsize(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 幅から「1 行に入るおおよその字数」を出す (等幅でない文字も混ざるので概算)。
///
/// egui の `Label::truncate` と二重に効かせる。truncate だけだと**ホバーで
/// 全文を出す口が無い**ので、こちらで詰めた文字列 + `on_hover_text(全文)`
/// を使う。
pub fn fit_chars(width: f32, char_w: f32) -> usize {
    if !width.is_finite() || width <= 0.0 || char_w <= 0.0 {
        return 0;
    }
    (width / char_w).floor().max(0.0) as usize
}

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
    /// 章立てガイド ([`WALKTHROUGHS`]) の進捗。`章 id -> 到達した手順数`。
    ///
    /// 構造体に `#[serde(default)]` が付いているので、**この欄が無い古い
    /// `tutorial.toml` もそのまま読める** (既存ユーザーの `done` /
    /// `version` を巻き戻さない)。章 id は固定なので、章の中身を並べ替えても
    /// 進捗は生き残る。
    pub chapters: BTreeMap<String, u32>,
}

impl Default for Persisted {
    fn default() -> Self {
        Self {
            done: false,
            version: 0,
            chapters: BTreeMap::new(),
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
// 未配線: app.rs は `Tutorial::autostart` 経由で入るのでこちらを呼んでいない。
#[allow(dead_code)]
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
    /// 再生中の手順列。初回ツアーは [`STEPS`] 全体、章立てガイドはその章だけ。
    ///
    /// **`STEPS` を直に見ない**ことで、章の再生と初回ツアーが同じ状態機械で
    /// 動く (進む / 戻る / スキップ / 依頼の受け渡しを 2 度書かない)。
    playlist: Vec<&'static Step>,
    /// 章から始めたときの章 id (進捗の保存先)。初回ツアーなら `None`。
    chapter_id: Option<&'static str>,
    /// 全機能ガイドを開いているか (開いている間ツアーのカードは重ねない)
    guide: bool,
    /// 索引の絞り込み文字列
    filter: String,
    /// 章の進捗 (`章 id -> 到達した手順数`)
    seen: BTreeMap<String, u32>,
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
        let seen = load_from(&dir).chapters;
        Self {
            active: false,
            idx: 0,
            confirm_skip: false,
            missing: MissingTracker::default(),
            pending: None,
            dir,
            playlist: Vec::new(),
            chapter_id: None,
            guide: false,
            filter: String::new(),
            seen,
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
        self.play(STEPS.iter().collect(), None);
    }

    /// 章立てガイドの 1 章を再生する。**前回の続きから**始まる。
    ///
    /// 再開位置は「最後に見たカード」= `done - 1`。*次の未読*から始めると、
    /// 読みかけで閉じた 1 枚が二度と出てこない (途中で閉じるのはたいてい
    /// 読み終わっていないから)。見終わっている章をもう一度選んだときだけ
    /// 先頭へ戻す (読み返しのため)。
    pub fn start_chapter(&mut self, w: &'static Walkthrough) {
        let (done, total) = chapter_progress(&self.seen, w);
        let at = if done >= total {
            0
        } else {
            done.saturating_sub(1)
        };
        self.play(w.steps(), Some(w.id));
        self.idx = at.min(self.playlist.len().saturating_sub(1));
        self.pending = self.playlist.get(self.idx).and_then(|s| s.pre_action);
    }

    /// 全機能ガイドを開く。ツアー中なら**その場で一時停止**する (閉じれば戻る)。
    pub fn open_guide(&mut self) {
        self.guide = true;
        self.confirm_skip = false;
    }

    /// 手順列を差し替えて再生を始める。
    fn play(&mut self, list: Vec<&'static Step>, chapter: Option<&'static str>) {
        self.playlist = list;
        self.active = !self.playlist.is_empty();
        self.idx = 0;
        self.confirm_skip = false;
        self.missing.reset();
        self.chapter_id = chapter;
        self.guide = false;
        self.pending = self.playlist.first().and_then(|s| s.pre_action);
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
    // **未配線の API。** app.rs から呼ばれていないので `dead_code` が鳴る。
    // モジュール全体に `allow` を掛けると「作ったのに繋いでいない」が
    // 見えなくなるので、**分かっている分だけ**を名指しで黙らせる。
    #[allow(dead_code)]
    pub fn active(&self) -> bool {
        self.active
    }

    /// 現在の手順 (非表示なら `None`)。
    // **未配線の API。** app.rs から呼ばれていないので `dead_code` が鳴る。
    // モジュール全体に `allow` を掛けると「作ったのに繋いでいない」が
    // 見えなくなるので、**分かっている分だけ**を名指しで黙らせる。
    #[allow(dead_code)]
    pub fn step(&self) -> Option<&'static Step> {
        if self.active {
            self.playlist.get(self.idx).copied()
        } else {
            None
        }
    }

    /// 現在位置 (0 始まり)。
    // **未配線の API。** app.rs から呼ばれていないので `dead_code` が鳴る。
    // モジュール全体に `allow` を掛けると「作ったのに繋いでいない」が
    // 見えなくなるので、**分かっている分だけ**を名指しで黙らせる。
    #[allow(dead_code)]
    pub fn index(&self) -> usize {
        self.idx
    }

    /// 進捗表示用の文字列 (例: `3 / 26`)。
    pub fn progress(&self) -> String {
        format!("{} / {}", self.idx + 1, self.playlist.len())
    }

    /// 次へ。最後の手順で呼ぶと完了して終了する。
    pub fn next(&mut self) -> Nav {
        self.confirm_skip = false;
        if self.idx + 1 < self.playlist.len() {
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
        self.record_chapter();
        self.active = false;
        self.confirm_skip = false;
        self.pending = None;
        self.missing.reset();
        self.mark_done();
    }

    /// 最後まで見終わった。
    pub fn complete(&mut self) {
        // 章を最後まで見たら「見終わり」として残す (途中で抜けたら現在位置まで)。
        if self.chapter_id.is_some() {
            self.idx = self.playlist.len().saturating_sub(1);
        }
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
    // **未配線の API。** app.rs から呼ばれていないので `dead_code` が鳴る。
    // モジュール全体に `allow` を掛けると「作ったのに繋いでいない」が
    // 見えなくなるので、**分かっている分だけ**を名指しで黙らせる。
    #[allow(dead_code)]
    pub fn confirming(&self) -> bool {
        self.confirm_skip
    }

    fn on_step_changed(&mut self) {
        self.missing.reset();
        self.pending = self
            .playlist
            .get(self.idx)
            .copied()
            .and_then(|s| s.pre_action);
        self.record_chapter();
    }

    fn mark_done(&mut self) {
        // **読んでから 2 欄だけ書き換える。** 丸ごと書くと章の進捗が消える。
        let mut st = load_from(&self.dir);
        st.done = true;
        st.version = STEPS_VERSION;
        save_to(&self.dir, &st);
    }

    /// 章の進捗を「到達した手順数」で更新する (**減らさない**)。
    ///
    /// 初回ツアー ([`STEPS`]) の再生中は何もしない。
    fn record_chapter(&mut self) {
        let Some(id) = self.chapter_id else { return };
        let reached = (self.idx as u32).saturating_add(1);
        if reached <= self.seen.get(id).copied().unwrap_or(0) {
            // 前に見たところより手前 = 書かない (毎フレーム書かないための番人でもある)
            return;
        }
        self.seen.insert(id.to_string(), reached);
        let mut st = load_from(&self.dir);
        st.chapters.insert(id.to_string(), reached);
        save_to(&self.dir, &st);
    }

    /// ホストへの依頼を 1 回だけ取り出す。
    // **未配線の API。** app.rs から呼ばれていないので `dead_code` が鳴る。
    // モジュール全体に `allow` を掛けると「作ったのに繋いでいない」が
    // 見えなくなるので、**分かっている分だけ**を名指しで黙らせる。
    #[allow(dead_code)]
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
        // 全機能ガイドはツアーと独立に開く。開いている間はカードを重ねない。
        if self.guide {
            self.draw_guide(ctx, theme, keys);
            return self.pending.take();
        }
        if !self.active {
            return None;
        }
        let Some(step) = self.playlist.get(self.idx).copied() else {
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
            crate::perf::repaint(ctx, "tutorial");
            return act;
        }

        self.paint_dim(ctx, theme, target, screen);
        if let Some(t) = target {
            self.paint_ring(ctx, theme, t, now);
        }
        self.card(ctx, theme, step, target, screen, keys);

        // リングのアニメーションのため、控えめな間隔で再描画を促す。
        crate::perf::repaint_after(
            ctx,
            std::time::Duration::from_millis(REPAINT_MS),
            "tutorial",
        );

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
            crate::perf::repaint(ctx, "tutorial");
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
            // 全機能ガイドへの導線。**最後のカードだけに置くと、途中で抜けた
            // 人には一生見えない**ので全カードに出す (行は折り返す)。
            if ui
                .button(tr("📖 全機能ガイド"))
                .on_hover_text(tr("章ごとの拾い読みと、全機能の索引を開きます"))
                .clicked()
            {
                self.open_guide();
            }
        });
    }

    /// 全機能ガイド — 章立ての拾い読みと、レジストリから作った索引を 1 枚で出す。
    ///
    /// **索引は手書きしない** ([`index_rows`])。`src/features/<名前>.rs` が
    /// 1 つ増えたら、このファイルを触らずに載る。
    fn draw_guide(&mut self, ctx: &egui::Context, theme: &crate::theme::Theme, keys: &Keybinds) {
        let screen = ctx.screen_rect();
        let lay = guide_layout(screen);

        // 背面はツアーと同じ濃さだけ落とす (真っ黒にしない)。
        let p = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            Id::new("zv-guide-dim"),
        ));
        let dim = Color32::from_black_alpha(dim_alpha(theme.dark));
        for r in dim_rects(None, screen) {
            p.rect_filled(r, Rounding::ZERO, dim);
        }

        let rows = index_rows(keys);
        let seen = self.seen.clone();
        // 借用を跨がせないため、絞り込み文字列は複製して最後に書き戻す。
        let mut filter = std::mem::take(&mut self.filter);
        let mut close = false;
        let mut start: Option<&'static Walkthrough> = None;
        let inner_w = (lay.window.width() - 2.0 * GUIDE_PAD).max(60.0);
        let body_h = (lay.body.height() - GUIDE_PAD).max(40.0);

        egui::Area::new(Id::new("zv-tutorial-guide"))
            .order(egui::Order::Foreground)
            .fixed_pos(lay.window.min)
            .interactable(true)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(theme.panel)
                    .stroke(Stroke::new(1.0_f32, theme.accent))
                    .rounding(Rounding::same(10.0))
                    .inner_margin(egui::Margin::same(GUIDE_PAD))
                    .show(ui, |ui| {
                        ui.set_min_width(inner_w);
                        ui.set_max_width(inner_w);

                        // ── 見出し
                        ui.allocate_ui(Vec2::new(inner_w, lay.header.height()), |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(tr("📖 全機能ガイド"))
                                        .strong()
                                        .color(theme.text),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .button(tr("閉じる"))
                                            .on_hover_text(tr("Esc でも閉じられます"))
                                            .clicked()
                                        {
                                            close = true;
                                        }
                                    },
                                );
                            });
                        });

                        // ── 絞り込み
                        ui.add_sized(
                            [inner_w, lay.search.height()],
                            egui::TextEdit::singleline(&mut filter)
                                .id_salt("zv-guide-filter")
                                .hint_text(tr("機能名・説明・打鍵で絞り込む")),
                        );
                        ui.add_space(4.0);

                        egui::ScrollArea::vertical()
                            .id_salt("zv-guide-body")
                            .max_height(body_h)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                let chapters: Vec<&'static Walkthrough> = WALKTHROUGHS
                                    .iter()
                                    .filter(|w| chapter_matches(w, &filter))
                                    .collect();
                                let feats: Vec<&IndexRow> = rows
                                    .iter()
                                    .filter(|r| {
                                        r.kind == RowKind::Feature && row_matches(r, &filter)
                                    })
                                    .collect();
                                let acts: Vec<&IndexRow> = rows
                                    .iter()
                                    .filter(|r| {
                                        r.kind == RowKind::Action && row_matches(r, &filter)
                                    })
                                    .collect();

                                // 空状態は**利用可能領域の中央に 1 枚**だけ。
                                if chapters.is_empty() && feats.is_empty() && acts.is_empty() {
                                    ui.allocate_ui_with_layout(
                                        Vec2::new(ui.available_width(), body_h),
                                        egui::Layout::centered_and_justified(
                                            egui::Direction::TopDown,
                                        ),
                                        |ui| {
                                            ui.label(
                                                egui::RichText::new(tr("一致する機能がありません"))
                                                    .color(theme.text_dim),
                                            );
                                        },
                                    );
                                    return;
                                }

                                // 中身が 0 件の節は**見出しごと出さない** (空白を作らない)。
                                if !chapters.is_empty() {
                                    guide_section(ui, theme, &tr("章立てガイド"));
                                    for w in chapters {
                                        if chapter_row(ui, theme, w, &seen) {
                                            start = Some(w);
                                        }
                                    }
                                    ui.add_space(6.0);
                                }
                                if !feats.is_empty() {
                                    guide_section(
                                        ui,
                                        theme,
                                        &format!("{} ({})", tr("機能"), feats.len()),
                                    );
                                    for r in feats {
                                        index_row_ui(ui, theme, r);
                                    }
                                    ui.add_space(6.0);
                                }
                                if !acts.is_empty() {
                                    guide_section(
                                        ui,
                                        theme,
                                        &format!("{} ({})", tr("組み込み操作"), acts.len()),
                                    );
                                    for r in acts {
                                        index_row_ui(ui, theme, r);
                                    }
                                }
                            });
                    });
            });

        self.filter = filter;

        // Esc で閉じる。**入力欄にフォーカスがあるときは奪わない**
        // (絞り込みを打っている人から Esc を取り上げない)。
        if !ctx.wants_keyboard_input()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            close = true;
        }
        if let Some(w) = start {
            self.start_chapter(w);
        } else if close {
            self.guide = false;
        }
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

// ── ガイドの部品 (描画) ──────────────────────────────────────

/// 節の見出し。**中身が 0 件のときは呼ばないこと** (空白を作らないため)。
fn guide_section(ui: &mut egui::Ui, theme: &crate::theme::Theme, title: &str) {
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(title)
            .small()
            .strong()
            .color(theme.accent),
    );
    ui.separator();
}

/// 章 1 行。押されたら `true` (その章の再生を始める)。
fn chapter_row(
    ui: &mut egui::Ui,
    theme: &crate::theme::Theme,
    w: &Walkthrough,
    seen: &BTreeMap<String, u32>,
) -> bool {
    let (done, total) = chapter_progress(seen, w);
    let cols = row_cols(ui.available_width(), 44.0);
    let full = format!("{} {} — {}", w.icon, tr(w.title), tr(w.summary));
    let shown = ellipsize(&full, fit_chars(cols.label, 12.0));
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.add_sized(
            [cols.icon, 18.0],
            egui::Label::new(egui::RichText::new(progress_mark(done, total)).color(
                if done >= total {
                    theme.ok
                } else {
                    theme.text_dim
                },
            )),
        );
        if ui
            .add_sized(
                [cols.label, 20.0],
                egui::Button::new(egui::RichText::new(shown).color(theme.text)).frame(false),
            )
            .on_hover_text(full.as_str())
            .clicked()
        {
            clicked = true;
        }
        if cols.keys > 0.0 {
            ui.add_sized(
                [cols.keys, 18.0],
                egui::Label::new(
                    egui::RichText::new(format!("{done}/{total}"))
                        .small()
                        .color(theme.text_dim),
                )
                .truncate(),
            );
        }
    });
    clicked
}

/// 索引 1 行。行は必ず可用幅に収め、詰めた分はホバーで全文を出す。
fn index_row_ui(ui: &mut egui::Ui, theme: &crate::theme::Theme, r: &IndexRow) {
    let want_keys = if r.keys.is_empty() { 0.0 } else { 96.0 };
    let cols = row_cols(ui.available_width(), want_keys);
    let full = if r.note.is_empty() {
        format!("{} ({})", r.label, r.id)
    } else {
        format!("{} — {} ({})", r.label, r.note, r.id)
    };
    let shown = ellipsize(&full, fit_chars(cols.label, 12.0));
    ui.horizontal(|ui| {
        ui.add_sized(
            [cols.icon, 16.0],
            egui::Label::new(egui::RichText::new(r.icon.as_str()).small()),
        );
        ui.add_sized(
            [cols.label, 16.0],
            egui::Label::new(egui::RichText::new(shown).small().color(
                // 機能は本文色、組み込み操作は控えめに (行数が多いので沈める)。
                if r.kind == RowKind::Feature {
                    theme.text
                } else {
                    theme.text_dim
                },
            ))
            .truncate(),
        )
        .on_hover_text(full.as_str());
        if cols.keys > 0.0 && !r.keys.is_empty() {
            ui.add_sized(
                [cols.keys, 16.0],
                egui::Label::new(
                    egui::RichText::new(r.keys.as_str())
                        .small()
                        .monospace()
                        .color(theme.text_dim),
                )
                .truncate(),
            );
        }
    });
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
            // 欄が増えても壊れないよう、テストは既定で締める。
            ..Persisted::default()
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
                ..Persisted::default()
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
                ..Persisted::default()
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

    // ── 追加手順と章立てガイド ──

    #[test]
    fn 追加手順のidは一意で既存の手順と重ならない() {
        let mut seen = std::collections::HashSet::new();
        for s in STEPS {
            seen.insert(s.id);
        }
        for s in EXTRA_STEPS {
            assert!(!s.id.is_empty(), "空の id がある");
            assert!(
                seen.insert(s.id),
                "id が STEPS か EXTRA_STEPS と重複: {}",
                s.id
            );
        }
        assert!(
            EXTRA_STEPS.len() >= 20,
            "追加手順が少なすぎる: {}",
            EXTRA_STEPS.len()
        );
    }

    #[test]
    fn 追加手順にも見出しと本文がある() {
        for s in EXTRA_STEPS {
            assert!(!s.title.trim().is_empty(), "{}: title が空", s.id);
            assert!(!s.body.trim().is_empty(), "{}: body が空", s.id);
            let sentences = s.body.matches('。').count();
            assert!(sentences <= 3, "{}: 文が多すぎる ({})", s.id, sentences);
        }
    }

    /// 追加手順が使うアンカーも `ALL_ANCHORS` にあるものだけ。
    #[test]
    fn 追加手順のアンカーは登録済みのものだけ() {
        for s in EXTRA_STEPS {
            if let Some(a) = s.anchor {
                assert!(ALL_ANCHORS.contains(&a), "{}: 未登録のアンカー {a:?}", s.id);
            }
        }
    }

    /// **章が参照する id はすべて実在する。** 落ちると章が黙って短くなる。
    #[test]
    fn 章の手順idはすべて実在する() {
        for w in WALKTHROUGHS {
            let got = w.steps();
            assert_eq!(
                got.len(),
                w.step_ids.len(),
                "章 {} に解決できない手順 id がある: {:?}",
                w.id,
                w.step_ids
                    .iter()
                    .filter(|id| !got.iter().any(|s| s.id == **id))
                    .collect::<Vec<_>>()
            );
            assert!(!got.is_empty(), "章 {} が空", w.id);
        }
    }

    /// **追加した手順が、どの章からも届かない状態を禁じる。**
    ///
    /// 手順だけ足して章へ載せ忘れると、UI から永久に到達できない
    /// (= 作ったのに繋いでいない) 。
    #[test]
    fn 追加手順はどれかの章から届く() {
        for s in EXTRA_STEPS {
            assert!(
                WALKTHROUGHS.iter().any(|w| w.step_ids.contains(&s.id)),
                "どの章からも届かない手順: {}",
                s.id
            );
        }
    }

    #[test]
    fn 章のidと見出しは重複せず空でない() {
        let mut ids = std::collections::HashSet::new();
        for w in WALKTHROUGHS {
            assert!(ids.insert(w.id), "章 id が重複: {}", w.id);
            assert!(!w.icon.trim().is_empty(), "{}: アイコンが空", w.id);
            assert!(!w.title.trim().is_empty(), "{}: 見出しが空", w.id);
            assert!(!w.summary.trim().is_empty(), "{}: 要約が空", w.id);
        }
        assert!(WALKTHROUGHS.len() >= 6, "章が少なすぎる");
    }

    /// 章のアンカーも app.rs 側で申告されている (無いと 4 秒で勝手に飛ぶ)。
    #[test]
    fn 章の手順もapp_rsでアンカーを申告している() {
        let src = include_str!("app.rs").replace("\r\n", "\n");
        let mut missing: Vec<String> = Vec::new();
        for s in EXTRA_STEPS {
            let Some(id) = s.anchor else { continue };
            let needle = format!("AnchorId::{id:?}");
            if !src.contains(&needle) {
                missing.push(format!("{} ({needle})", s.id));
            }
        }
        assert!(missing.is_empty(), "申告の無いアンカー: {missing:?}");
    }

    /// 章の手順が出す依頼も app.rs のルーティングに届く。
    #[test]
    fn 章の手順の依頼もapp_rsに届く() {
        let src = include_str!("app.rs").replace("\r\n", "\n");
        let body = src
            .split("fn apply_tutorial_action(")
            .nth(1)
            .expect("ルーティング関数がある");
        for s in EXTRA_STEPS {
            let Some(act) = s.pre_action else { continue };
            let name = format!("{act:?}");
            let variant = name.split('(').next().unwrap_or(&name).to_string();
            assert!(
                body.contains(&format!("TA::{variant}")),
                "{} の依頼 {variant} を実行していない",
                s.id
            );
        }
    }

    // ── 全機能索引 (自動生成) ──

    fn plain_keys() -> Keybinds {
        Keybinds::from_overrides(&HashMap::new())
    }

    /// **番人: レジストリにあるのに索引へ出ない機能がゼロ。**
    ///
    /// `src/features/<名前>.rs` が 1 つ増えたら、tutorial.rs を 1 バイトも
    /// 触らずに索引へ載る — これが崩れると「新機能がガイドに無い」が再発する。
    #[test]
    fn 索引はレジストリの全機能を必ず載せる() {
        let rows = index_rows(&plain_keys());
        let mut missing: Vec<&str> = Vec::new();
        let mut total = 0usize;
        for f in crate::feature::REGISTRY {
            for e in f.entries {
                total += 1;
                if !rows.iter().any(|r| r.id == e.id) {
                    missing.push(e.id);
                }
            }
        }
        assert!(missing.is_empty(), "索引に出ない機能: {missing:?}");
        // 走査が空振りしていないこと (0 件でも緑、が最悪の壊れ方)。
        assert!(total >= 5, "レジストリが {total} 件しか見えていない");
    }

    /// 組み込み操作も全部載る (`ALL_ACTIONS` に足したら索引にも出る)。
    #[test]
    fn 索引は組み込み操作も全部載せる() {
        let keys = plain_keys();
        let rows = index_rows(&keys);
        for a in crate::keybinds::ALL_ACTIONS {
            let id = crate::keybinds::config_name(a);
            assert!(rows.iter().any(|r| r.id == id), "索引に出ない操作: {id}");
        }
        assert_eq!(
            rows.len(),
            feature_rows().len() + crate::keybinds::ALL_ACTIONS.len(),
            "索引の件数が合わない"
        );
    }

    /// **補足説明が無い機能も、説明が空になるだけで必ず一覧に出る。**
    ///
    /// ここを「説明表に載っているものだけ出す」にすると、今回の要望
    /// (新機能が案内に出てこない) がそのまま再発する。
    #[test]
    fn 説明の無い機能も索引に出る() {
        // **実在の登録に依存せず**、説明表に無い id でも行が作れることを見る。
        // 件数比較だけだと、レジストリ全件にたまたま説明が付いている間は
        // 何も検査しない (実際に空回りしていたのを反証で見つけた)。
        let newbie = "まだ説明の無い新機能.open";
        assert_eq!(note_of(newbie), "");
        let row = feature_row("🆕", "まだ説明の無い新機能", newbie);
        assert_eq!(row.note, "", "説明が無いのに何かが入っている");
        assert_eq!(row.id, newbie);
        assert_eq!(row.icon, "🆕");
        assert!(!row.label.is_empty(), "説明が無いと見出しまで消えている");
        assert_eq!(row.kind, RowKind::Feature);
        // 説明が無いだけで一覧や絞り込みから消えたりしない。
        assert!(row_matches(&row, ""));
        assert!(row_matches(&row, "まだ説明の無い新機能"));

        // そのうえで、レジストリ全件が 1 件も落ちずに行になっている。
        let entries: usize = crate::feature::REGISTRY
            .iter()
            .map(|f| f.entries.len())
            .sum();
        assert_eq!(
            feature_rows().len(),
            entries,
            "説明表の有無で行が落ちている"
        );
    }

    /// 補足説明の宛先が実在すること (機能を消したのに説明だけ残るのを防ぐ)。
    #[test]
    fn 補足説明の宛先は実在する機能だけ() {
        let mut stale: Vec<&str> = Vec::new();
        for (id, note) in FEATURE_NOTES {
            assert!(!note.trim().is_empty(), "{id} の説明が空");
            let live = crate::feature::REGISTRY
                .iter()
                .any(|f| f.entries.iter().any(|e| e.id == *id));
            if !live {
                stale.push(id);
            }
        }
        assert!(stale.is_empty(), "宛先の無い補足説明: {stale:?}");
    }

    /// **打鍵はキーバインド表から作る** (ベタ書きなら再割り当てで嘘になる)。
    #[test]
    fn 索引の打鍵は再割り当てに追随する() {
        use crate::keybinds::Binding;
        let plain = plain_keys();
        let before = plain.label(BindAction::Save);
        let mut keys = plain_keys();
        keys.set(
            BindAction::Save,
            Binding::Single(egui::KeyboardShortcut::new(
                egui::Modifiers::ALT,
                egui::Key::F9,
            )),
        );
        let id = crate::keybinds::config_name(BindAction::Save);
        let row = action_rows(&keys)
            .into_iter()
            .find(|r| r.id == id)
            .expect("保存の行がある");
        assert_eq!(row.keys, keys.label(BindAction::Save));
        assert_ne!(row.keys, before, "再割り当てが索引に反映されていない");
    }

    #[test]
    fn 絞り込みは名前とidと説明に当たる() {
        let rows = index_rows(&plain_keys());
        let row = rows
            .iter()
            .find(|r| r.id == "lease.list")
            .expect("lease.list がある");
        assert!(row_matches(row, ""), "空の絞り込みは全部通す");
        assert!(row_matches(row, "   "), "空白だけも全部通す");
        assert!(row_matches(row, "lease"), "id に当たらない");
        assert!(row_matches(row, "LEASE"), "大文字小文字を無視していない");
        assert!(row_matches(row, "台帳"), "補足説明に当たらない");
        assert!(!row_matches(row, "存在しない語"), "誤って通している");
        // 複数語は and 条件
        assert!(row_matches(row, "lease 台帳"));
        assert!(!row_matches(row, "lease 存在しない語"));
    }

    #[test]
    fn 章の絞り込みは手順の見出しにも当たる() {
        let git = WALKTHROUGHS
            .iter()
            .find(|w| w.id == "git")
            .expect("git の章がある");
        assert!(chapter_matches(git, ""));
        assert!(chapter_matches(git, "git"));
        assert!(chapter_matches(git, "差分"), "手順の見出しに当たらない");
        assert!(!chapter_matches(git, "存在しない語"));
    }

    /// 一致ゼロなら章も索引も 0 件 = 空状態のカード 1 枚だけになる。
    #[test]
    fn 一致ゼロなら章も索引も空になる() {
        let needle = "zzz該当なしzzz";
        let rows = index_rows(&plain_keys());
        assert!(rows.iter().all(|r| !row_matches(r, needle)));
        assert!(WALKTHROUGHS.iter().all(|w| !chapter_matches(w, needle)));
    }

    // ── ガイドの配置 (純粋関数) ──

    #[test]
    fn ガイドの区画はどの窓でも画面内に収まり重ならない() {
        let screens = [
            ("既定", rect(0.0, 0.0, 1200.0, 800.0)),
            ("小さめ", rect(0.0, 0.0, 900.0, 700.0)),
            ("横長で低い", rect(0.0, 0.0, 1200.0, 300.0)),
            ("小窓", rect(0.0, 0.0, 600.0, 400.0)),
            ("極小", rect(0.0, 0.0, 320.0, 200.0)),
            ("極端に低い", rect(0.0, 0.0, 1000.0, 60.0)),
            ("原点がゼロでない", rect(120.0, 80.0, 1000.0, 700.0)),
            ("大画面", rect(0.0, 0.0, 2560.0, 1440.0)),
        ];
        for (name, screen) in screens {
            let l = guide_layout(screen);
            assert!(inside(l.window, screen), "{name}: 枠が画面外");
            for (part, r) in [("header", l.header), ("search", l.search), ("body", l.body)] {
                assert!(inside(r, screen), "{name}: {part} が画面外");
                assert!(inside(r, l.window.expand(0.01)), "{name}: {part} が枠外");
            }
            // 縦に積んで重ならない
            assert!(
                l.header.max.y <= l.search.min.y + 0.01,
                "{name}: header と search が重なる"
            );
            assert!(
                l.search.max.y <= l.body.min.y + 0.01,
                "{name}: search と body が重なる"
            );
            assert!(l.body.height() > 0.0, "{name}: 本文の高さがゼロ");
            assert!(
                l.header.height() + l.search.height() + l.body.height() <= l.window.height() + 0.01,
                "{name}: 区画の合計が枠を超えている"
            );
        }
    }

    #[test]
    fn 索引の列幅は可用幅を超えない() {
        for avail in [0.0_f32, 1.0, 40.0, 120.0, 300.0, 680.0, 2000.0] {
            for want in [0.0_f32, 44.0, 96.0, 400.0] {
                let c = row_cols(avail, want);
                assert!(c.icon >= 0.0 && c.label >= 0.0 && c.keys >= 0.0);
                assert!(
                    c.icon + c.label + c.keys <= avail + 0.01,
                    "avail={avail} want={want} → {c:?} が可用幅を超えた"
                );
            }
        }
    }

    #[test]
    fn 字数の切り詰めは日本語を割らない() {
        assert_eq!(ellipsize("あいうえお", 10), "あいうえお");
        assert_eq!(ellipsize("あいうえお", 5), "あいうえお");
        assert_eq!(ellipsize("あいうえお", 3), "あい…");
        assert_eq!(ellipsize("あいうえお", 1), "…");
        assert_eq!(ellipsize("あいうえお", 0), "");
        assert_eq!(ellipsize("", 4), "");
        // 詰めた結果も必ず上限以内 (キリル文字・漢字・かなが混ざっても)
        for n in 0..8 {
            assert!(ellipsize("абвгд漢字かな", n).chars().count() <= n);
        }
    }

    #[test]
    fn 幅から出す字数は壊れた入力でもゼロ() {
        assert_eq!(fit_chars(120.0, 12.0), 10);
        assert_eq!(fit_chars(0.0, 12.0), 0);
        assert_eq!(fit_chars(-5.0, 12.0), 0);
        assert_eq!(fit_chars(f32::NAN, 12.0), 0);
        assert_eq!(fit_chars(f32::INFINITY, 12.0), 0);
        assert_eq!(fit_chars(120.0, 0.0), 0);
    }

    // ── 章の進捗 ──

    #[test]
    fn 進捗の印は三状態() {
        assert_eq!(progress_mark(0, 5), "·");
        assert_eq!(progress_mark(2, 5), "◔");
        assert_eq!(progress_mark(5, 5), "✓");
        assert_eq!(progress_mark(9, 5), "✓");
        // 空の章で ✓ を出さない (0/0 は未着手扱い)
        assert_eq!(progress_mark(0, 0), "·");
    }

    #[test]
    fn 章の進捗は手順数を超えない() {
        let w = &WALKTHROUGHS[0];
        let mut m = BTreeMap::new();
        assert_eq!(chapter_progress(&m, w), (0, w.step_ids.len()));
        m.insert(w.id.to_string(), 2);
        assert_eq!(chapter_progress(&m, w), (2, w.step_ids.len()));
        m.insert(w.id.to_string(), 9999);
        assert_eq!(
            chapter_progress(&m, w),
            (w.step_ids.len(), w.step_ids.len())
        );
    }

    /// 章は**途中から再開**し、進捗は減らない。
    #[test]
    fn 章は途中から再開して進捗は減らない() {
        let w = &WALKTHROUGHS[0];
        let dir = unique_temp_dir("zaivern-tutorial-test", "chapter-resume");
        let mut t = Tutorial::in_dir(dir.clone());
        t.start_chapter(w);
        assert!(t.active());
        assert_eq!(t.index(), 0, "初回は先頭から");
        assert_eq!(t.progress(), format!("1 / {}", w.step_ids.len()));
        t.next();
        t.next();
        t.skip();

        // 別インスタンス = 次の起動。続きから始まる。
        let mut t2 = Tutorial::in_dir(dir.clone());
        t2.start_chapter(w);
        assert_eq!(t2.index(), 2, "続きから始まっていない");
        // 手前まで戻って抜けても進捗は減らない
        t2.back();
        t2.skip();
        let mut t3 = Tutorial::in_dir(dir.clone());
        t3.start_chapter(w);
        assert_eq!(t3.index(), 2, "進捗が巻き戻った");

        // 最後まで見たら「見終わり」。もう一度選ぶと先頭から読み返せる。
        let mut t4 = Tutorial::in_dir(dir.clone());
        t4.start_chapter(w);
        while t4.next() == Nav::Moved {}
        let seen = load_from(&dir).chapters;
        assert_eq!(chapter_progress(&seen, w).0, w.step_ids.len());
        let mut t5 = Tutorial::in_dir(dir);
        t5.start_chapter(w);
        assert_eq!(t5.index(), 0, "見終わった章は先頭から読み返せる");
    }

    /// 章を再生しても**初回ツアーの既読フラグは触らない**。
    #[test]
    fn 章の再生は初回ツアーの既読を触らない() {
        let dir = unique_temp_dir("zaivern-tutorial-test", "chapter-isolated");
        let mut t = Tutorial::in_dir(dir.clone());
        assert!(should_autostart_in(&dir), "前提: まだ見ていない");
        t.start_chapter(&WALKTHROUGHS[1]);
        t.next();
        assert!(
            should_autostart_in(&dir),
            "章を見ただけで初回ツアーが既読になっている"
        );
        // 逆向き: 初回ツアーを終えても章の進捗は消えない
        let saved = load_from(&dir).chapters.clone();
        assert!(!saved.is_empty(), "章の進捗が保存されていない");
        t.start();
        t.skip();
        assert_eq!(
            load_from(&dir).chapters,
            saved,
            "既読フラグの保存で章の進捗が消えた"
        );
    }

    /// 章の再生でも初回ツアーと同じ状態機械が動く (進捗表示・末尾で完了)。
    #[test]
    fn 章の再生も同じ状態機械で動く() {
        let w = WALKTHROUGHS
            .iter()
            .find(|w| w.id == "czero")
            .expect("競合ゼロの章がある");
        let mut t = tut("chapter-machine");
        t.start_chapter(w);
        for i in 0..w.step_ids.len() - 1 {
            assert_eq!(t.index(), i);
            assert_eq!(t.progress(), format!("{} / {}", i + 1, w.step_ids.len()));
            assert_eq!(t.next(), Nav::Moved);
        }
        assert_eq!(t.next(), Nav::Completed);
        assert!(!t.active());
        // 初回ツアーへ戻ると再生列も戻る
        t.start();
        assert_eq!(t.progress(), format!("1 / {}", STEPS.len()));
    }

    /// ガイドを開いてもツアーは終わらない (閉じれば同じ手順へ戻る)。
    #[test]
    fn ガイドは開いてもツアーを終わらせない() {
        let mut t = tut("guide-pause");
        t.start();
        t.next();
        let at = t.index();
        t.open_guide();
        assert!(t.active(), "ガイドを開いただけでツアーが終わっている");
        assert_eq!(t.index(), at, "位置が動いた");
    }

    /// 初回ツアーは**この版でも 1 件も減っていない** (章立ては別立てで足す)。
    #[test]
    fn 初回ツアーの手順は減っていない() {
        assert!(
            STEPS.len() >= 27,
            "初回ツアーが削られている: {}",
            STEPS.len()
        );
        for id in ["welcome", "layout", "palette", "cockpit", "finish"] {
            assert!(STEPS.iter().any(|s| s.id == id), "{id} が消えている");
        }
    }
}
