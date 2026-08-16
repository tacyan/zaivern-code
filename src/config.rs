use crate::agents;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// ファイル索引 (⌘P) の既定の上限。`.gitignore` を尊重すれば
/// 数万ファイル規模のモノレポでも届かない値にしてある
/// (以前は 8000 件 / 深さ 12 で**無音**に打ち切っていた)。
pub const DEFAULT_INDEX_MAX_FILES: usize = 50_000;
/// ファイル索引が潜る既定の深さ。node_modules を除いた実在のツリーは
/// 20 段も無いが、生成物混じりのリポジトリでも足りるよう余裕を持たせる。
pub const DEFAULT_INDEX_MAX_DEPTH: usize = 32;
/// ツリーの 1 ディレクトリで一度に描く既定の行数。
/// 画面に入るのは数十行なので、これを超えたぶんは「さらに N 件」に畳む。
pub const DEFAULT_TREE_DIR_PAGE: usize = 300;
/// Hot Exit が 1 バッファあたりに退避する既定の上限 (KiB)。
/// 普通のソースは数十 KiB なので余裕があり、生成物やログを開いたまま
/// 落ちても数百 MiB を `~/.zaivern` に抱え込まない値にしてある。
pub const DEFAULT_HOT_EXIT_MAX_KB: usize = 4096;
/// Hot Exit の退避を書き出す既定の最短間隔 (ミリ秒)。
/// 打鍵のたびに書くと大きなファイルで I/O が飽和するので必ず間引く。
pub const DEFAULT_HOT_EXIT_INTERVAL_MS: u64 = 1500;
/// ローカルヒストリの既定の保持日数。IntelliJ の lvcs と同じ 5 日。
/// **壁時計ではなく活動時間**で数えるので、離席した日数は消費しない。
pub const DEFAULT_LOCAL_HISTORY_DAYS: u32 = 5;
/// 「活動していない」と見なす空白の既定のしきい値 (時間)。
/// IntelliJ の `INTERVAL_BETWEEN_ACTIVITIES` (12 時間) に合わせてある。
pub const DEFAULT_LOCAL_HISTORY_GAP_HOURS: u32 = 12;

/// コスト上限の既定の警告割合 (8 割)。
///
/// **なぜ 8 割か** — 残り 2 割は「いま走っているターンを最後まで走らせて、
/// それから並列度を落とすか上限を上げるかを決める」ぶんの猶予として要る。
/// 9 割では 1 ターンぶんに満たないことがあり (エージェント 1 本の 1 ターンは
/// 数十万トークン = 上限が $50 なら数ドル規模)、警告と同時に超過する。
/// 5 割では並列で走らせている限りほぼ常時鳴り、狼少年になる。
/// 使用率の助言側 ([`crate::coordinator::quota::Policy::slow_fraction`]) も
/// 同じ 0.80 で「絞れ」を出すので、しきい値の意味を 2 か所で食い違わせない。
pub const DEFAULT_COST_WARN_RATIO: f32 = 0.80;

/// ガターの git blame をどこまで出すか。**3 段**ある。
///
/// 競合で最も評価が高いのは GitLens の既定である「カーソル行だけ」で、
/// 全行ガターは横幅を食って邪魔になるという評価が多い。そこで既定は
/// [`BlameMode::Off`] のまま、`current` を**中間の段**として持つ。
///
/// ## 旧設定との互換
///
/// 0.15.0 までは `git_blame = true / false` の**真偽値**だった。
/// [`BlameMode`] の `Deserialize` は bool も文字列も読むので、既存の
/// config.toml / state.toml はそのまま動く (`true` → [`BlameMode::All`])。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BlameMode {
    /// 出さない (既定)。git は 1 度も起きない。
    #[default]
    Off,
    /// **カーソル行だけ**。取りに行くのもその 1 行を含むブロックだけ。
    Current,
    /// 可視域の全行 (0.15.0 までの `git_blame = true` と同じ)。
    All,
}

impl BlameMode {
    /// config の文字列から。未知の値は既定 (Off)。
    pub fn from_config_str(s: &str) -> BlameMode {
        match s.trim().to_ascii_lowercase().as_str() {
            "current" | "line" | "cursor" => BlameMode::Current,
            "all" | "true" | "gutter" | "1" => BlameMode::All,
            _ => BlameMode::Off,
        }
    }

    /// 旧形式の真偽値から (`true` = 全行)。
    pub fn from_flag(on: bool) -> BlameMode {
        if on {
            BlameMode::All
        } else {
            BlameMode::Off
        }
    }

    /// config へ書く文字列。
    pub const fn config_str(self) -> &'static str {
        match self {
            BlameMode::Off => "off",
            BlameMode::Current => "current",
            BlameMode::All => "all",
        }
    }

    /// UI に出す名前 (**日本語の原文**。表示側で `tr` を通す)。
    pub const fn label(self) -> &'static str {
        match self {
            BlameMode::Off => "出さない",
            BlameMode::Current => "カーソル行だけ",
            BlameMode::All => "全行",
        }
    }

    /// 何か出すか (ガターの列を確保するかの判定)。
    pub fn is_on(self) -> bool {
        !matches!(self, BlameMode::Off)
    }

    /// 次の段 (メニューの 1 項目から 3 段を回すため)。
    pub fn next(self) -> BlameMode {
        match self {
            BlameMode::Off => BlameMode::Current,
            BlameMode::Current => BlameMode::All,
            BlameMode::All => BlameMode::Off,
        }
    }
}

impl Serialize for BlameMode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.config_str())
    }
}

impl<'de> Deserialize<'de> for BlameMode {
    /// **旧形式の真偽値も読む。** 既存ユーザーの `git_blame = true` を
    /// 「壊れた設定」にしないための入口で、ここが唯一の変換点。
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<BlameMode, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = BlameMode;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(r#""off" / "current" / "all" (旧形式の true / false も可)"#)
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<BlameMode, E> {
                Ok(BlameMode::from_flag(v))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<BlameMode, E> {
                Ok(BlameMode::from_config_str(v))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<BlameMode, E> {
                Ok(BlameMode::from_config_str(&v))
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: String,
    /// UI の表示言語。`"auto"` (既定) は OS の言語に従う。
    ///
    /// 値は `locales/<id>.json` の `<id>`。同梱は
    /// `en` / `ja` / `zh-CN` / `ko` / `pt-BR` / `es` の 6 つで、
    /// `~/.zaivern/locales/fr.json` を置けば `"fr"` も選べる。
    /// 解決の規則は [`crate::locale::resolve`]。
    pub ui_language: String,
    pub editor_font_size: f32,
    pub terminal_font_size: f32,
    /// 画面全体のズーム倍率 (VS Code の `window.zoomLevel` 相当)。
    ///
    /// egui の `zoom_factor` へそのまま渡す = **UI の全部** (サイドバー・タブ・
    /// メニュー・端末・エディタ) が一緒に拡大縮小する。段は `crate::zoom::STEPS`。
    /// ファイル単位のズームは倍率をバッファ側が持つので、ここには入らない。
    pub ui_zoom: f32,
    /// **文字サイズだけ**の倍率 (既定 1.0)。段は `crate::zoom::STEPS`。
    ///
    /// `ui_zoom` との違いが要点: `ui_zoom` は egui の `zoom_factor` を動かすので
    /// 余白・ボタン・パネル幅まで一緒に大きくなる = **画面の情報量が減る**。
    /// こちらは本文・ボタン文字・エディタ・ターミナルの**文字サイズだけ**を
    /// 掛け直すので、レイアウトはそのままで字だけ読みやすくできる。
    /// (「画面は大きく出来るのに文字サイズだけ変えられない」への答え)
    ///
    /// エディタ / ターミナルの実サイズは `editor_font_size` /
    /// `terminal_font_size` にこの倍率を掛けたもの。
    pub text_scale: f32,
    /// 最後に「What's New」を見た版 (`0.9.0` 形式)。空 = 一度も見ていない。
    ///
    /// 初回起動では**何も出さない** — 入れた直後に変更履歴を突き付けても
    /// 意味が無く、「画面が突然変わらない」に反する。判定は
    /// [`crate::whats_new::unseen`]。
    #[serde(skip)]
    pub last_seen_version: String,
    pub show_hidden_files: bool,

    /// `.gitignore` (+ `.git/info/exclude` + `core.excludesFile`) を尊重して
    /// ファイルツリーとファイル索引 (⌘P) から除外するか。**既定はオン**。
    /// これが無いと `node_modules` / `target` がツリーと索引を埋め尽くす。
    pub respect_gitignore: bool,

    /// 無視されたファイルを隠さず**薄く**表示するか (VS Code と同じ見せ方)。
    /// **既定はオン** — VS Code の既定 (`explorer.excludeGitIgnore = false` +
    /// `git.decorations.enabled = true`) が「消さずに薄く出す」だからで、
    /// 「置いたはずのファイルがツリーに無い」を既定で起こさないため。
    /// `respect_gitignore = false` のときは意味を持たない (無視の判定自体が無い)。
    pub dim_ignored_files: bool,

    /// ファイル索引 (⌘P) に載せる最大件数。上限に達したらパレットに
    /// 「N 件で打ち切りました」と出す (無音で切らない)。
    /// `.gitignore` を尊重していれば数万件のリポジトリでも届かない。
    pub index_max_files: usize,

    /// ファイル索引が潜る最大の深さ (ルート直下 = 1)。
    pub index_max_depth: usize,

    /// ツリーの 1 ディレクトリで一度に描く行数の上限。
    /// 超えたぶんは「さらに N 件」を押すと同じ数だけ伸びる
    /// (巨大ディレクトリで数万行を描いてフレームを落とさないため)。
    pub tree_dir_page: usize,

    /// エディタ本文の折り返し (VS Code の Word Wrap 相当)。既定はオフ。
    pub word_wrap: bool,

    /// 空白文字の可視化 (スペース「·」/ タブ「→」)。既定はオフ。
    pub show_whitespace: bool,

    /// カーソル下のシンボルと同じものを本文で薄くハイライトするか
    /// (LSP の textDocument/documentHighlight)。既定はオン。
    /// オフにすると要求自体を送らないので、サーバーへの往復もゼロになる。
    pub lsp_highlight_occurrences: bool,

    /// 診断メッセージを本文の**行末**に淡色で出すか (VS Code の Error Lens 相当)。
    /// **既定はオン。ただし出すのはキャレット行だけ** — 全行に出すと本文の
    /// 右側が文章で埋まり、コードより診断の方が目立ってしまう。
    /// オフにしても波線とホバーは残る (消えるのは行末の文字だけ)。
    pub inline_diagnostics: bool,

    /// LSP のインレイヒント (推論された型・引数名) を本文の行末に淡色で出すか。
    ///
    /// **既定はオフ**。理由は 2 つ:
    /// 1. 出るのは行末なので、ONにすると本文の右側が型名で埋まる。ミニマップと
    ///    同じ判断で「欲しい人だけが払う」。
    /// 2. 本文を打つたびにサーバーへ往復が 1 つ増える (キャッシュは版ごと)。
    ///    使わない人にその代金を払わせない (設計原則 3)。
    pub inlay_hints: bool,

    /// エディタ右端のミニマップ (VS Code の遠景ビュー相当)。
    /// **既定はオフ** — 本文の横幅を 64px 奪うので、欲しい人だけが払う。
    /// 幅が足りない画面では設定が ON でも自動的に隠れる。
    pub minimap: bool,

    /// 取り消し履歴: 連続した文字入力を 1 段へまとめる時間しきい値 (ミリ秒)。
    /// これを越えて間が空いたら別の段になる (VS Code / Zed と同じ考え方)。
    pub undo_merge_ms: u64,

    /// 取り消し履歴の最大段数 (タブ 1 枚あたり)。古いものから捨てる。
    pub undo_max_steps: usize,

    /// 取り消し履歴が抱える差分の合計バイト上限 (タブ 1 枚あたり)。
    /// 巨大な一括置換を何度もやっても、ここで頭打ちになる。
    pub undo_max_bytes: usize,

    /// 未保存の本文を退避して再起動後に復元する (VS Code の `files.hotExit`)。
    /// **既定はオン** — 落ちたら本文が消えるのはデータ損失なので、既定で守る。
    /// 退避先は `~/.zaivern/hotexit/<ワークスペース>/`。保存して閉じれば消える。
    pub hot_exit: bool,

    /// Hot Exit が 1 バッファあたりに退避する上限 (KiB)。
    /// 超えたバッファは退避せず、その旨をトーストで伝える (無音で落とさない)。
    pub hot_exit_max_kb: usize,

    /// Hot Exit の退避を書き出す最短間隔 (ミリ秒)。
    /// 打鍵のたびに書かないためのスロットリング。0 にすると変更のたびに書く。
    pub hot_exit_interval_ms: u64,

    /// ローカルヒストリ (VCS に依らない取り消し履歴) を記録するか。
    /// **既定はオン** — 「コミットしていない変更を壊した」は git では戻せない。
    pub local_history: bool,

    /// ローカルヒストリの保持日数。**壁時計ではなく「活動時間」**で数えるので、
    /// 1 週間マシンを離れても予算を食わない
    /// ([`crate::local_history::purge_from`])。
    pub local_history_days: u32,

    /// これを超える空白は「活動していない」と見なして 1ms と数える (時間)。
    /// IntelliJ の既定 (12 時間) に合わせてある。
    pub local_history_gap_hours: u32,
    /// which-key ポップアップを出すまでの待ち (ミリ秒)。
    ///
    /// chord (2 打鍵) の 1 打鍵目を握ってからこの時間が経つと、そこから続く
    /// 打鍵の一覧が右下に出る。**待ちがあるのは、chord を淀みなく打つ人に
    /// ポップアップを 1 度も見せないため**。0 にすると即座に出る。
    /// 2 打鍵目以降は待たない ([`crate::whichkey::SECOND_DELAY`] = 0)。
    pub whichkey_delay_ms: u64,

    /// エディタ上部のブレッドクラム (`ワークスペース › フォルダ › ファイル › シンボル`)。
    /// **既定はオン** — 高さ 1 行ぶんで、どの言語でも (LSP 無しでも) 必ず出せる。
    pub breadcrumbs: bool,
    /// 差分ビューの既定の表示: `"side_by_side"` (左右 2 列) | `"inline"` (1 列)。
    ///
    /// **既定は並列**。ただし幅が足りないときは `diff::diff_layout` が
    /// 自動で 1 列へ縮退させるので、狭いウィンドウでも見切れない。
    /// 値の解釈は [`crate::diff::DiffMode::from_config_str`] に集約してある。
    pub diff_view: String,
    /// ドラッグ&ドロップで移動するとき「"X" を "Y" へ移動しますか?」を出すか
    /// (VS Code の `explorer.confirmDragAndDrop`)。**既定はオン**。
    ///
    /// 確認ダイアログの「今後確認しない」を押すとここが false になり
    /// state.toml へ残る。同名衝突の確認は**この設定では消せない**
    /// (既存を壊す操作なので、必ず聞く)。
    pub confirm_drag_and_drop: bool,

    /// 削除をゴミ箱へ送るか (VS Code の `files.enableTrash`)。**既定はオン**。
    ///
    /// オフにすると削除は常に完全削除 (取り消せない) になる。
    /// オンでも Shift 併用なら完全削除を選べる。
    pub enable_trash: bool,

    /// エディタ左端のガターに git blame (著者 · 相対日時) を出すか。
    /// `"off"` (既定) / `"current"` (カーソル行だけ) / `"all"` (全行)。
    /// 出している間だけ**必要な行ぶん**の `git blame` を非同期で取る。
    /// 旧形式の `git_blame = true / false` もそのまま読める。
    pub git_blame: BlameMode,

    // ── 保存時の整形 (VS Code の files.* / editor.formatOnSave 相当) ──
    //
    // **どれも既定はオフ** — VS Code の既定と同じ。保存しただけで差分が
    // 増えるのは事故なので、明示的に選んだ人だけが払う。
    /// 保存時に各行の末尾空白を落とす (`files.trimTrailingWhitespace`)。
    /// 全角スペース U+3000 と NBSP は落とさない
    /// ([`crate::editor_ops::trim_trailing_whitespace`] を参照)。
    pub trim_trailing_whitespace: bool,
    /// 保存時に末尾の余分な空行を落とす (`files.trimFinalNewlines`)。
    pub trim_final_newlines: bool,
    /// 保存時に最終行へ改行を入れる (`files.insertFinalNewline`)。
    pub insert_final_newline: bool,
    /// 保存時に LSP の整形をかける (`editor.formatOnSave`)。
    /// 整形の経路は「ドキュメントを整形」と同じ 1 本。
    pub format_on_save: bool,

    // ── エディタの見た目・インデント ──────────────────────────────
    /// 括弧を入れ子の深さごとに色分けする
    /// (`editor.bracketPairColorization.enabled`)。**既定はオン**。
    /// 色はテーマの ANSI 表から採る ([`crate::theme::Theme::bracket_colors`])。
    pub bracket_colorization: bool,
    /// 縦のルーラーを引く桁 (`editor.rulers`)。既定は空 = 1 本も引かない。
    /// 例: `rulers = [80, 120]`。桁は等幅の**桁数**で数える (東アジア文字幅ではない)。
    pub rulers: Vec<usize>,
    /// 開いたファイルの中身からインデントを推定する
    /// (`editor.detectIndentation`)。**既定はオン**。
    /// オフにすると `tab_size` / `insert_spaces` をそのまま使う。
    pub detect_indentation: bool,
    /// インデント 1 段の桁数 (`editor.tabSize`)。既定 4。
    pub tab_size: usize,
    /// インデントにスペースを使う (`editor.insertSpaces`)。既定オン。
    pub insert_spaces: bool,
    /// タブ切替 (Ctrl+Tab) を **MRU (最近使った順)** で回すか。
    ///
    /// **既定はオン** — VS Code / Zed と同じで、押しっぱなしの間に候補一覧を
    /// 出し、離したところで確定する。2 回押せば直前のファイルへ戻れる。
    /// オフにすると従来どおりの**位置巡回** (タブの並び順) になる。
    /// どちらでも `[keybindings]` の `switch_tab` / `switch_tab_back` で
    /// 打鍵を付け替えられる。
    pub tab_switch_mru: bool,

    /// **プレビュータブ** (VS Code の斜体タブ) を使うか。
    ///
    /// **既定はオン** — ツリーやパレットからシングルクリックで開いたタブは
    /// 使い捨てになり、次のプレビューで置き換わるのでタブが無限に増えない。
    /// もう一度クリック / 編集 / ピン留め / ドラッグで確定タブへ昇格する。
    /// オフにすると開いたタブは常に確定タブになる。
    pub preview_tabs: bool,

    /// 既定の権限モード: "ask"(毎回ユーザー承認) | "auto"(全て自動YES) |
    /// "agent"(Agent欄優先: プリセットのコマンドに書かれたフラグをそのまま使う)
    pub approval_mode: String,
    /// フォルダを開き直したとき、前回のエージェントタブを復元して会話を再開するか。
    /// **既定は false** — 起動しただけで前回の会話が勝手に走り出さない。
    /// 過去の会話へ戻る口は「💬 セッション」タブ (明示的に選んで再開) の 1 本に絞る。
    /// true にすると前回スクロールバックを再生し、対応 CLI (claude / codex) は
    /// 再開指定付きで起動する。
    pub restore_agents: bool,
    /// ターンが終わった時点で、**そのエージェント自身の CLI** に 2〜5 語の
    /// 題名を作らせてタブ名にするか (cmux 由来)。
    ///
    /// **既定は false。** 有効にしても、手で付けた名前は上書きしない /
    /// 生成に失敗したら黙って従来名のまま / アイドル時は 1 プロセスも起こさない。
    /// 対応 CLI は [`crate::agents::TITLE_GENERATORS`] (実機で確認できたものだけ)。
    pub auto_name_sessions: bool,
    pub show_pet: bool,
    /// state.toml へ書き戻すグローバル値の控え (プロジェクト overlay 適用前)。
    /// save_state はこちらを書くので、.zaivern.toml のプロジェクト値が
    /// グローバル state.toml へ漏れて永続化されることはない。
    /// 設定ファイルには読み書きしない (メモリ上だけ)。
    #[serde(skip)]
    pub global_theme: String,
    #[serde(skip)]
    pub global_approval_mode: String,
    #[serde(skip)]
    pub global_show_pet: bool,
    #[serde(skip)]
    pub global_word_wrap: bool,
    #[serde(skip)]
    pub global_show_whitespace: bool,
    #[serde(skip)]
    pub global_ui_zoom: f32,
    #[serde(skip)]
    pub global_text_scale: f32,
    #[serde(skip)]
    pub global_minimap: bool,
    #[serde(skip)]
    pub global_breadcrumbs: bool,
    pub global_git_blame: BlameMode,
    /// overlay を重ねる前のグローバルなプラグイン設定の控え。
    /// save_plugins_section はこちらを書く — セッション中の値を書くと
    /// プロジェクトの .zaivern.toml 由来の無効化・設定値がグローバル
    /// config.toml へ漏れて永続化されてしまう。
    #[serde(skip)]
    pub global_plugins: PluginsConfig,
    /// ペット画像のフルパス(None なら内蔵ドット絵)
    pub pet_image: Option<String>,
    /// ペットの固定位置(None なら右下うろうろ)
    pub pet_x: Option<f32>,
    pub pet_y: Option<f32>,
    /// ペットの見た目: "blocky" | "crab" | "cat" | "cloud"
    pub pet_variant: String,
    /// ペットの大きさ (0.75=小 / 1.0=中 / 1.4=大)
    pub pet_scale: f32,
    /// うろうろ散歩するか
    pub pet_free_roam: bool,
    /// 無操作で睡眠するか
    pub pet_sleep: bool,
    /// 効果音を鳴らすか
    pub pet_sounds: bool,
    /// 承認バブルを表示するか
    pub pet_bubbles: bool,
    /// 承認プロンプトへ自動で YES を送るか (オフ=ユーザー承認必須)
    pub pet_auto_yes: bool,
    /// 承認時に PTY へ送るキー (既定は Enter)
    pub pet_approve_keys: String,
    /// 拒否時に PTY へ送るキー (既定は ESC)
    pub pet_deny_keys: String,
    /// 自動YESの追加ルール (ユーザー定義)。
    ///
    /// CLI 側が承認プロンプトの文言を変えると、同梱の応答表 (src/agents.rs) が
    /// 一致しなくなり自動YESが素通りする。そのとき**再ビルドせずに**
    /// config.toml だけで直せるようにするための逃げ道。
    /// 組み込みの表より先に評価されるので、既定の判断を上書きもできる。
    pub auto_yes_rules: Vec<AutoYesRule>,
    /// 統合承認キューのポリシー (`[[approval_policies]]`)。
    ///
    /// 「この種別の承認は、この範囲では常に許可/拒否」を宣言する表。
    /// 空 (既定) なら従来どおり全部ユーザーに聞く。**このキーが無い
    /// 既存の config.toml でも `Config` の `#[serde(default)]` により
    /// 空として読み込まれる**ので、古い設定を壊さない。
    pub approval_policies: Vec<ApprovalPolicy>,
    /// 音声認識エンジン: "auto" | "mac" | "powershell" | "browser" | "command" | "off"
    /// auto = macOS は内蔵、voice_command 設定済みならそれ、Windows は標準の
    /// 音声認識、残りはブラウザの /voice ページ (src/voice.rs の resolve_engine)
    pub voice_engine: String,
    /// 音声入力の既定の届け先: "active"(アクティブなエージェント) | "broadcast"(全員)
    pub voice_target: String,
    /// 認識言語 (BCP-47)
    pub voice_lang: String,
    /// 外部音声認識コマンド (mac 以外 / 独自エンジン用)。
    /// 標準出力に 1 行ずつテキストを吐き、stdin の "q" で停止する実装を想定。
    /// {lang} は voice_lang に置換される。
    pub voice_command: String,
    /// 話すと自動で Enter まで送るキーワード (空文字 = 常に手動 Enter)
    pub voice_keyword: String,
    /// SSH リモート接続の踏み台 (`user@host` / `user@host:port`。空 = 未設定)。
    ///
    /// **鍵やパスフレーズは絶対に保存しない** — 認証は OS の `ssh` と
    /// `~/.ssh/config` / ssh-agent に丸投げする。ここに置くのは接続先だけ。
    pub ssh_tunnel_host: String,
    /// 外部通知の Webhook URL (空 = 無効)。承認待ち・終了・レート制限を
    /// curl で POST する。ntfy トピック URL / Slack / Discord の Incoming Webhook に対応。
    pub webhook_url: String,
    pub agents: Vec<AgentPreset>,
    /// キーバインドの上書き: action名 → "cmd+shift+p" 形式 (src/keybinds.rs 参照)
    pub keybindings: HashMap<String, String>,
    /// プラグインの有効/無効と設定値。
    pub plugins: PluginsConfig,
    /// **シェル統合 (OSC 633 / 133) の注入**。既定は `false` = オプトイン。
    ///
    /// off のとき、端末の起動経路は導入前と 1 バイトも変わらない。
    /// on にすると素のシェル (コマンド指定なしのターミナル) だけが
    /// `~/.zaivern/shellint/` のシムを読んで起動し、コマンドの境界・
    /// 終了コード・コマンド行を OSC で報告するようになる。
    /// 受け取り側 (パース) は常時 on — iTerm2 / kitty / starship を
    /// 使っている人のシェルは注入しなくても既に喋っているため。
    pub shell_integration: bool,
    /// エージェント監視 (スーパーバイザー) の設定。
    /// `[supervisor]` セクションが無い既存の config.toml でも、
    /// `SupervisorConfig` 側の `#[serde(default)]` により既定値で読み込まれる。
    pub supervisor: crate::supervisor::SupervisorConfig,
    /// 監視役 LLM (スーパーエージェント) の設定。
    /// `[super_agent]` セクションが無い既存の config.toml でも、
    /// `SuperAgentConfig` 側の `#[serde(default)]` により既定値 (= なし) で読み込まれる。
    pub super_agent: SuperAgentConfig,
    /// レート制限時のアカウント自動フェイルオーバー (`[failover]`)。
    /// **既定は無効** — ユーザーが明示的に有効化したときだけ働く。
    pub failover: crate::failover::FailoverConfig,

    /// 🏁 プロンプトレースの勝者評価 (`[race_eval]`)。
    /// 除外パターンと上限は**すべてここから来る** — race.rs にリテラルは置かない。
    pub race_eval: RaceEvalConfig,
    /// トークン単価の表 (`[pricing]`)。推定コストの計算に使う。
    /// **価格は変わるのでユーザーが上書きできる** — 詳細は [`PricingConfig`]。
    pub pricing: PricingConfig,

    /// このアプリを起動してからの推定コストの上限
    /// (通貨は [`PricingConfig::currency`])。**0 = 無制限 (既定)**。
    pub cost_limit_session: f32,
    /// 1 日 (UTC) の推定コストの上限。**0 = 無制限 (既定)**。
    ///
    /// 日の境界を UTC で切る理由は
    /// [`crate::coordinator::quota::utc_day_index`] のコメントに書いてある。
    pub cost_limit_daily: f32,
    /// 上限の何割を使ったら警告するか (0.0..=1.0)。
    /// 既定は [`DEFAULT_COST_WARN_RATIO`]。
    pub cost_warn_ratio: f32,
    /// 上限に達したときの動作: `"notify"` (知らせるだけ) / `"stop"`
    /// (新規の送信を止める)。**既定は `"notify"`** — 勝手に止めない。
    pub cost_limit_action: String,

    /// コマンドパレットの MRU (最近実行したコマンド。先頭が直近)。
    ///
    /// UI の操作から溜まる値なので手書きの config.toml には**書かない** —
    /// state.toml 側 (`[[palette_recent]]`) に置く。`save_state` が
    /// この控えをそのまま書き戻すので、テーマ変更などで消えることはない。
    #[serde(skip)]
    pub palette_recent: Vec<PaletteRecent>,

    /// ⌃1〜⌃9 の起動バーに並べるプリセット名 (**ユーザーが決めた順**)。
    ///
    /// * `None` = まだ何も決めていない → [`quick_launch_slots`] が
    ///   `agents` の**先頭から**既定の並びを作る (プリセットの並び自体が固定順)。
    /// * `Some(空)` = ユーザーが全部外した → 起動バーは 1px も描かない。
    ///
    /// **使用頻度・通知・未読で並べ替えない。** cmux が「通知順でタイルを
    /// 並べ替えたら ⌘1-9 の割当が動き続ける」と批判された轍を踏まないため、
    /// 番号 → プリセットの対応は `quick_launch_slots` という純粋関数だけが決め、
    /// その入力は「プリセット一覧」と「この保存済みの並び」しか無い。
    ///
    /// UI の操作から溜まる値なので手書きの config.toml には書かない
    /// (state.toml 側に `quick_launch = [...]` として置く)。
    #[serde(skip)]
    pub quick_launch: Option<Vec<String>>,

    /// 機能 ([`crate::feature::REGISTRY`]) が自分で宣言した設定の値。
    /// config.toml では `[features]` 区画。
    ///
    /// **この 1 フィールドを足したのは 1 度きり。** 以後どれだけ機能が増えても
    /// `Config` へフィールドを足す必要が無いので、2 つのブランチが同時に
    /// 設定を足しても**同じ行を奪い合わない** — which-key と local_history が
    /// `config.rs` へ追記して 3 ハンク衝突したのが、まさに避けたかった形。
    /// 機能側は `src/features/<名前>.rs` に
    /// [`crate::feature::Setting`] を宣言するだけで済む。
    ///
    /// キーは `"<module>.<name>"`。点を含むので TOML では必ず引用符が付く:
    ///
    /// ```toml
    /// [features]
    /// "whichkey.delay_ms" = 300
    /// ```
    ///
    /// **この版が知らないキーも捨てずに持ち続ける。** 新しい版が足した設定を、
    /// 古い版で 1 度起動しただけで消してしまう事故を防ぐため
    /// (読み書きの往復で残ることを `機能設定の未知のキーは読み書きしても消えない`
    /// が番人として押さえている)。
    ///
    /// 読み書きは型付きのアクセサ ([`Config::feature_bool`] /
    /// [`Config::feature_i64`] / [`Config::feature_f64`] /
    /// [`Config::feature_str`] / [`Config::set_feature`]) から行う。
    /// **直接触ると型違いで panic する経路を作れてしまう。**
    #[serde(default, rename = "features")]
    pub extra: std::collections::BTreeMap<String, toml::Value>,
}

/// ⌃1〜⌃9 に割り当たるプリセットの添字を**スロット順**で返す。
///
/// * 入力は「プリセット一覧」と「保存済みの並び」だけ — 使用頻度も未読も
///   通知も受け取らないので、**番号が勝手に動く余地が構造的に無い**。
/// * 保存済みの名前が今のプリセットに無ければ黙って飛ばす
///   (壊れた設定でも panic しない)。
/// * 同じ名前が 2 度並んでいたら 1 度だけ採る。
/// * 返す件数は最大 [`QUICK_LAUNCH_SLOTS`] 件。添字 0 が ⌃1。
pub fn quick_launch_slots(agents: &[AgentPreset], stored: Option<&[String]>) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    match stored {
        // 既定: プリセットの並びの先頭から。プリセットの並び自体が固定なので、
        // ここでも並べ替えは起こらない。
        None => {
            for (i, _) in agents.iter().enumerate() {
                if out.len() >= QUICK_LAUNCH_SLOTS {
                    break;
                }
                out.push(i);
            }
        }
        Some(names) => {
            for name in names {
                if out.len() >= QUICK_LAUNCH_SLOTS {
                    break;
                }
                let Some(i) = agents.iter().position(|p| p.name == *name) else {
                    continue; // 消えたプリセットは黙って飛ばす
                };
                if !out.contains(&i) {
                    out.push(i);
                }
            }
        }
    }
    out
}

/// 起動バーの枠数。⌃1〜⌃9 の 9 個で固定。
pub const QUICK_LAUNCH_SLOTS: usize = 9;

/// 現在のスロット割り当てを**保存する形** (プリセット名の並び) にする。
/// 保存 → 読み込みで順序が変わらないことは往復テストで固定してある。
pub fn quick_launch_names(agents: &[AgentPreset], slots: &[usize]) -> Vec<String> {
    slots
        .iter()
        .filter_map(|i| agents.get(*i).map(|p| p.name.clone()))
        .collect()
}

/// パレット MRU の永続化 1 件ぶん。**アクションは保存しない** —
/// `Cmd` にはパスや添字が入り、次の起動では別物を指しうるため。
/// ラベルから引き直せなければ順位付けにだけ使う (palette.rs)。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaletteRecent {
    pub label: String,
    pub icon: String,
    pub uses: u32,
}

/// `[super_agent]` セクション。**どのエージェントに他のエージェントを見張らせるか**。
///
/// 既定は「なし」。決定論的な監視 (supervisor) は この設定に関わらず常に働くので、
/// ここが空でも見張り自体は成立する。LLM はあくまで助言役。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SuperAgentConfig {
    /// 監視役に使うプリセットのコマンド。空文字 = なし (LLM には相談しない)。
    pub command: String,
    /// 指揮官に指名したセッションのタイトル (例: `Claude Code (全自動) #3`)。
    /// 空文字 = 指名なし (旧来どおり `command` に一致する最初のセッションを使う)。
    /// セッション ID は再起動で変わるため、再起動をまたいでも追従できる
    /// タイトルで持つ。
    pub session_title: String,
    /// 指揮を有効にするか。指名 (`session_title` / `command`) が空なら
    /// この値によらず指揮しない。
    pub enabled: bool,
    /// (廃止) かつての状況フィードの最短間隔 (秒)。状況フィードは廃止された
    /// (指揮官の端末へも他エージェントへも自動注入しない) ため、いまは
    /// どこからも参照されない。既存の state.toml を壊さないよう残している。
    pub timeout_secs: u64,
}

impl Default for SuperAgentConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            session_title: String::new(),
            enabled: false,
            timeout_secs: 60,
        }
    }
}

impl SuperAgentConfig {
    /// 監視役として実際に動かす対象のコマンド。無効・未選択なら `None`。
    ///
    /// 「有効フラグが立っている」だけでは足りない。コマンドが空なら誰も選ばれて
    /// いないので、ここで必ず弾く。
    pub fn active_command(&self) -> Option<&str> {
        if !self.enabled {
            return None;
        }
        let c = self.command.trim();
        if c.is_empty() {
            None
        } else {
            Some(c)
        }
    }
}

/// `[race_eval]` セクション — 🏁 プロンプトレースの**勝者評価**が使う除外と上限。
///
/// 評価は「どの racer の差分が良いか」を見るものなので、**人が書いていない差分**を
/// 読ませても判断の役に立たない。ロックファイル・ビルド成果物・巨大な生成物は
/// ここに書いたパターンで落とす。**パターンはコードに直書きせず必ずここから配る**
/// (race.rs は `&RaceEvalConfig` を受け取るだけで、既定値も知らない)。
///
/// 書式は `.gitignore` と同じ (`crate::ignore` がそのまま解釈する) ので、
/// `target/` のようなディレクトリ指定も `*.png` のようなグロブも書ける。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RaceEvalConfig {
    /// 評価の入力から落とすパス (`.gitignore` 記法)。
    pub exclude: Vec<String>,
    /// 候補 1 本ぶんの差分に許す最大バイト数。超えたら**切り詰めた旨を明示**する。
    pub max_diff_bytes: usize,
    /// 全候補を合わせた差分の最大バイト数 (1 本ぶんの上限とは別に効く)。
    pub max_total_bytes: usize,
    /// 1 行がこれを超えるファイルは「生成物」とみなして丸ごと落とす。
    /// minified な生成物は行数が少なくても 1 行が数十万バイトになる。
    pub max_line_bytes: usize,
}

/// 既定の除外パターン。**ここが唯一の出どころ**で、race.rs 側には持たせない。
const DEFAULT_RACE_EXCLUDE: &[&str] = &[
    // ── ロックファイル (どれも自動生成で、差分が長い割に読む価値がない) ──
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "npm-shrinkwrap.json",
    "bun.lockb",
    "poetry.lock",
    "Pipfile.lock",
    "composer.lock",
    "Gemfile.lock",
    "go.sum",
    "flake.lock",
    "uv.lock",
    // ── ビルド成果物・依存の展開先 ──
    "target/",
    "node_modules/",
    "dist/",
    "build/",
    "out/",
    ".next/",
    ".venv/",
    "vendor/",
    "__pycache__/",
    "*.min.js",
    "*.min.css",
    "*.map",
    // ── 生成された固定物 ──
    "*.snap",
    "*.pb.go",
];

impl Default for RaceEvalConfig {
    fn default() -> Self {
        Self {
            exclude: DEFAULT_RACE_EXCLUDE.iter().map(|s| s.to_string()).collect(),
            max_diff_bytes: 24 * 1024,
            max_total_bytes: 96 * 1024,
            max_line_bytes: 4 * 1024,
        }
    }
}

/// `[plugins]` セクション。
///
/// - `disabled`: 無効にするプラグイン名。未記載のものは有効。
/// - `settings`: プラグインごとの設定値 (`[plugins.settings.<名前>]`)。
///   キーはマニフェストの `[[setting]] key`、値は文字列として保持する。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    pub disabled: Vec<String>,
    pub settings: HashMap<String, HashMap<String, String>>,
}

impl PluginsConfig {
    /// 指定プラグインが有効か。
    pub fn is_enabled(&self, name: &str) -> bool {
        !self.disabled.iter().any(|d| d == name)
    }

    /// 有効/無効を切り替える。
    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        if enabled {
            self.disabled.retain(|d| d != name);
        } else if !self.disabled.iter().any(|d| d == name) {
            self.disabled.push(name.to_string());
        }
    }

    /// プラグインの設定値を取り出す (未設定なら None)。
    /// `set_setting` と対になる読み出し口として公開しておく。
    #[allow(dead_code)]
    pub fn setting(&self, plugin: &str, key: &str) -> Option<&str> {
        self.settings.get(plugin)?.get(key).map(|s| s.as_str())
    }

    /// プラグインの設定値を書き込む。
    pub fn set_setting(&mut self, plugin: &str, key: &str, value: &str) {
        self.settings
            .entry(plugin.to_string())
            .or_default()
            .insert(key.to_string(), value.to_string());
    }
}

/// モデル 1 種類の単価 (100 万トークンあたり、[`PricingConfig::currency`] 単位)。
///
/// ```toml
/// [pricing.models."claude-opus-5"]
/// input = 5.0
/// output = 25.0
/// cache_write = 6.25
/// cache_read = 0.5
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPrice {
    /// キャッシュに当たらなかった入力。
    pub input: f64,
    /// 出力。
    pub output: f64,
    /// キャッシュ書き込み。
    pub cache_write: f64,
    /// キャッシュ読み出し。
    pub cache_read: f64,
}

impl Default for ModelPrice {
    fn default() -> Self {
        Self {
            input: 0.0,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
        }
    }
}

/// `[pricing]` セクション — **推定コストの単価表**。
///
/// ## なぜ設定に持つのか
///
/// 価格はベンダーの都合でいつでも変わる。ロジック側にモデル名と金額を
/// 焼き込むと、値上げ・値下げ・新モデルのたびにビルドし直しになる。
/// ここへ表として置き、`state.toml` / `config.toml` から上書きできるようにする。
/// `quota` module 側にはモデル名も金額も**一切書かない**
/// ([`crate::coordinator::quota::PriceLookup`] 越しに引くだけ)。
///
/// ## 引き当ての規則
///
/// 1. 完全一致
/// 2. 無ければ**最長の前方一致** (`claude-opus-5[1m]` → `claude-opus-5`)
/// 3. それでも無ければ「不明」。**0 円にはしない** (推定を過小に見せないため)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PricingConfig {
    /// 推定コストを出すか。**既定はオン** (消費ゼロなら結局何も出ない)。
    pub enabled: bool,
    /// 表示に使う通貨記号。既定値の表は米ドル建てなので `"$"`。
    /// 別通貨で持つならここと `models` を一緒に書き換える。
    pub currency: String,
    /// モデル名 → 単価。キーは前方一致で引かれる。
    pub models: HashMap<String, ModelPrice>,
}

impl Default for PricingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            currency: "$".into(),
            models: default_model_prices(),
        }
    }
}

/// 既定の単価表。
///
/// **2026-08-09 時点**の公開価格 (100 万トークンあたり・米ドル)。
/// キャッシュ単価は入力単価から導出している (書き込み = 入力の 1.25 倍 /
/// 5 分 TTL、読み出し = 入力の 0.1 倍)。**古くなったら設定で上書きすること** —
/// このアプリは価格を問い合わせに行かない (通信ゼロの方針)。
///
/// キーは前方一致で引かれるので、日付サフィックス付きの ID
/// (`claude-haiku-4-5-20251001` 等) もここの短い名前に当たる。
fn default_model_prices() -> HashMap<String, ModelPrice> {
    /// 入力単価から 1 行ぶんを組み立てる (キャッシュ倍率は共通)。
    fn row(input: f64, output: f64) -> ModelPrice {
        ModelPrice {
            input,
            output,
            cache_write: input * 1.25,
            cache_read: input * 0.1,
        }
    }
    // (モデル名の前方一致キー, 入力, 出力)
    const TABLE: &[(&str, f64, f64)] = &[
        // Anthropic
        ("claude-fable-5", 10.0, 50.0),
        ("claude-mythos-5", 10.0, 50.0),
        ("claude-opus-5", 5.0, 25.0),
        ("claude-opus-4", 5.0, 25.0),
        ("claude-sonnet-5", 3.0, 15.0),
        ("claude-sonnet-4", 3.0, 15.0),
        ("claude-haiku-4", 1.0, 5.0),
    ];
    TABLE
        .iter()
        .map(|(name, i, o)| ((*name).to_string(), row(*i, *o)))
        .collect()
}

impl PricingConfig {
    /// モデル名から単価を引く (完全一致 → 最長の前方一致)。
    pub fn lookup(&self, model: &str) -> Option<ModelPrice> {
        if !self.enabled || model.is_empty() {
            return None;
        }
        if let Some(p) = self.models.get(model) {
            return Some(*p);
        }
        self.models
            .iter()
            .filter(|(k, _)| !k.is_empty() && model.starts_with(k.as_str()))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, p)| *p)
    }
}

impl Config {
    /// 設定を [`crate::coordinator::quota::CostLimits`] へ畳む。
    ///
    /// **これが上限の唯一の作り方**。判定側 (`quota`) は金額も通貨も知らず、
    /// ここから渡された数値を比べるだけ。
    pub fn cost_limits(&self) -> crate::coordinator::quota::CostLimits {
        use crate::coordinator::quota::{CostLimits, LimitAction};
        CostLimits {
            session: f64::from(self.cost_limit_session.max(0.0)),
            daily: f64::from(self.cost_limit_daily.max(0.0)),
            warn_ratio: self.cost_warn_ratio,
            action: LimitAction::from_key(&self.cost_limit_action),
        }
    }
}

impl crate::coordinator::quota::PriceLookup for PricingConfig {
    fn rate(&self, model: &str) -> Option<crate::coordinator::quota::ModelRate> {
        self.lookup(model)
            .map(|p| crate::coordinator::quota::ModelRate {
                input: p.input,
                output: p.output,
                cache_write: p.cache_write,
                cache_read: p.cache_read,
            })
    }

    fn currency(&self) -> &str {
        &self.currency
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "zaivern-dark".into(),
            ui_language: crate::locale::AUTO.into(),
            editor_font_size: 15.0,
            terminal_font_size: 13.0,
            ui_zoom: crate::zoom::DEFAULT,
            text_scale: crate::zoom::DEFAULT,
            last_seen_version: String::new(),
            show_hidden_files: true,
            respect_gitignore: true,
            dim_ignored_files: true,
            index_max_files: DEFAULT_INDEX_MAX_FILES,
            index_max_depth: DEFAULT_INDEX_MAX_DEPTH,
            tree_dir_page: DEFAULT_TREE_DIR_PAGE,
            word_wrap: false,
            show_whitespace: false,
            lsp_highlight_occurrences: true,
            inline_diagnostics: true,
            inlay_hints: false,
            minimap: false,
            undo_merge_ms: crate::editor::UNDO_MERGE_MS,
            undo_max_steps: crate::editor::UNDO_MAX_STEPS,
            undo_max_bytes: crate::editor::UNDO_MAX_BYTES,
            hot_exit: true,
            hot_exit_max_kb: DEFAULT_HOT_EXIT_MAX_KB,
            hot_exit_interval_ms: DEFAULT_HOT_EXIT_INTERVAL_MS,
            local_history: true,
            local_history_days: DEFAULT_LOCAL_HISTORY_DAYS,
            local_history_gap_hours: DEFAULT_LOCAL_HISTORY_GAP_HOURS,
            whichkey_delay_ms: crate::whichkey::DEFAULT_FIRST_DELAY_MS,
            breadcrumbs: true,
            diff_view: crate::diff::DiffMode::default().config_str().into(),
            git_blame: BlameMode::Off,
            confirm_drag_and_drop: true,
            enable_trash: true,
            // 保存時の整形は VS Code と同じく全部オフから始める
            trim_trailing_whitespace: false,
            trim_final_newlines: false,
            insert_final_newline: false,
            format_on_save: false,
            bracket_colorization: true,
            rulers: Vec::new(),
            detect_indentation: true,
            tab_size: crate::editor_ops::IndentStyle::DEFAULT_WIDTH,
            insert_spaces: true,
            tab_switch_mru: true,
            preview_tabs: true,
            approval_mode: "ask".into(),
            restore_agents: false,
            auto_name_sessions: false,
            show_pet: true,
            global_theme: "zaivern-dark".into(),
            global_approval_mode: "ask".into(),
            global_show_pet: true,
            global_word_wrap: false,
            global_show_whitespace: false,
            global_ui_zoom: 1.0,
            global_text_scale: 1.0,
            global_minimap: false,
            global_breadcrumbs: true,
            global_git_blame: BlameMode::Off,
            global_plugins: PluginsConfig::default(),
            pet_image: None,
            pet_x: None,
            pet_y: None,
            pet_variant: "blocky".into(),
            pet_scale: 1.0,
            pet_free_roam: true,
            pet_sleep: true,
            pet_sounds: true,
            pet_bubbles: true,
            pet_auto_yes: false,
            pet_approve_keys: "\r".into(),
            pet_deny_keys: "\u{1b}".into(),
            auto_yes_rules: Vec::new(),
            approval_policies: Vec::new(),
            voice_engine: "auto".into(),
            voice_target: "active".into(),
            voice_lang: "ja-JP".into(),
            voice_command: String::new(),
            voice_keyword: String::new(),
            ssh_tunnel_host: String::new(),
            webhook_url: String::new(),
            agents: default_agents(),
            keybindings: HashMap::new(),
            shell_integration: false,
            supervisor: crate::supervisor::SupervisorConfig::default(),
            super_agent: SuperAgentConfig::default(),
            plugins: PluginsConfig::default(),
            failover: crate::failover::FailoverConfig::default(),
            race_eval: RaceEvalConfig::default(),
            pricing: PricingConfig::default(),
            // 上限は**出荷時は無し**。設定するまで画面に 1px も出さないし、
            // 何も止めない (金額をコードに埋めないためでもある)。
            cost_limit_session: 0.0,
            cost_limit_daily: 0.0,
            cost_warn_ratio: DEFAULT_COST_WARN_RATIO,
            cost_limit_action: "notify".into(),
            palette_recent: Vec::new(),
            quick_launch: None,
            // 機能の設定は「宣言された既定」が正なので、ここは常に空から始める。
            // 空 = 全部が既定値、という意味 (`feature_*` が宣言へ落ちる)。
            extra: std::collections::BTreeMap::new(),
        }
    }
}

impl Config {
    /// 取り消し履歴のしきい値と上限。**呼び出し側で直書きしないための唯一の口**。
    pub fn history_limits(&self) -> crate::editor::HistoryLimits {
        crate::editor::HistoryLimits {
            merge_ms: self.undo_merge_ms,
            // 0 を書かれても 1 段は残す (`History::trim` 側でも守っている)
            max_steps: self.undo_max_steps.max(1),
            max_bytes: self.undo_max_bytes,
        }
    }
}

/// 自動YESのユーザー定義ルール 1 件 (`[[auto_yes_rules]]`)。
///
/// ```toml
/// [[auto_yes_rules]]
/// pattern = "Allow access to this file?"   # 画面に出る目印
/// reply   = "\r"                            # PTY へ送るキー ("\r"=Enter)
/// agent   = "agy"                           # 省略/空なら全エージェント
/// ```
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct AutoYesRule {
    /// 画面に含まれていたら一致とみなす文字列。空の行は無視される。
    pub pattern: String,
    /// 一致したとき PTY へ送るキー列。TOML のエスケープがそのまま使える
    /// (`"\r"` = Enter / `"y\r"` = y と Enter / `"1"` = 番号キー)。
    pub reply: String,
    /// 対象エージェントの実行ファイル名 (`agy` / `claude` …)。
    /// 空なら全エージェント。別名で書いても正規化されないので正規名で書く。
    pub agent: String,
}

impl Default for AutoYesRule {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            reply: "\r".into(),
            agent: String::new(),
        }
    }
}

/// 統合承認キューのポリシー 1 件 (`[[approval_policies]]`)。
///
/// ```toml
/// [[approval_policies]]
/// kind     = "file_read"     # 承認の種別 (src/approvals.rs の ApprovalKind)
/// scope    = "agent"         # 適用範囲: "global" | "agent" | "session" | "path"
/// target   = "claude"        # scope の対象 (global は空)
/// decision = "allow_always"  # "ask" | "allow_once" | "allow_always" | "deny_always"
/// ```
///
/// 値はすべて**安定 ID (英小文字)** で持つ。列挙型を直接 serde に載せず
/// 文字列にしているのは、将来 kind が増えても古い Zaivern が
/// 「未知の行」として読み飛ばせるようにするため
/// ([`approval_policies_from_config`] が未知の値の行を捨てる)。
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct ApprovalPolicy {
    /// 承認の種別 ID (`file_read` / `file_write` / `file_delete` /
    /// `shell_command` / `network_access` / `git_operation` /
    /// `package_install` / `privilege` / `other`)。
    pub kind: String,
    /// 適用範囲 (`global` / `agent` / `session` / `path`)。省略で `global`。
    pub scope: String,
    /// `scope` の対象値。`agent` なら実行ファイル名、`session` なら数値 ID、
    /// `path` ならパス接頭辞。`global` では空。
    pub target: String,
    /// 判断 (`ask` / `allow_once` / `allow_always` / `deny_always`)。
    pub decision: String,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            kind: String::new(),
            scope: "global".into(),
            target: String::new(),
            decision: "ask".into(),
        }
    }
}

/// `[[approval_policies]]` を承認エンジンの [`crate::agents::approvals::Policy`] へ変換する。
///
/// **未知の種別 / 範囲 / 判断の行は黙って捨てる**。書き間違いや、
/// 新しい Zaivern が書いた行を古いバイナリで読んだときに、意図しない
/// 種別へ丸めて自動承認してしまう事故を防ぐため (「推測しない」の原則)。
pub fn approval_policies_from_config(cfg: &Config) -> Vec<crate::agents::approvals::Policy> {
    use crate::agents::approvals::{ApprovalKind, Decision, Policy, Scope};
    cfg.approval_policies
        .iter()
        .filter_map(|p| {
            Some(Policy {
                kind: ApprovalKind::from_id(p.kind.trim())?,
                scope: Scope::from_toml(p.scope.trim(), p.target.trim())?,
                decision: Decision::from_id(p.decision.trim())?,
            })
        })
        .collect()
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentPreset {
    pub name: String,
    /// Shell command line. Empty string launches a plain login shell.
    pub command: String,
    pub icon: String,
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
}

impl Default for AgentPreset {
    fn default() -> Self {
        Self {
            name: "Shell".into(),
            command: String::new(),
            icon: "🖥".into(),
            cwd: None,
            env: HashMap::new(),
        }
    }
}

/// 既定のプリセット一覧。
///
/// **エージェント固有の値をここへ直書きしない**: 並びは
/// [`agents::DEFAULT_PRESET_BINS`]、中身は [`agents::AGENT_CATALOG`] の
/// エントリから組み立てる(プリセットの作り方はピッカーで「追加」したときと
/// 完全に同じ経路 = `agent_picker::plain_preset` / `auto_preset`)。
/// カタログに CLI を足せば、ここを触らずに既定へ載せられる。
fn default_agents() -> Vec<AgentPreset> {
    let mut out = Vec::new();
    for bin in agents::DEFAULT_PRESET_BINS {
        // 別名でも引けるよう spec_for_bin を通す(bin が変わっても壊れない)。
        let Some(spec) = agents::spec_for_bin(bin) else {
            continue;
        };
        out.push(crate::agent_picker::plain_preset(spec));
        if let Some(auto) = crate::agent_picker::auto_preset(spec) {
            out.push(auto);
        }
    }
    // 素のログインシェルは常に最後(コマンド空 = シェル起動)。
    out.push(AgentPreset {
        name: "Shell".into(),
        command: String::new(),
        icon: "🖥".into(),
        ..Default::default()
    });
    out
}

/// Project-local overlay (<workspace>/.zaivern.toml): every field optional.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Overlay {
    theme: Option<String>,
    editor_font_size: Option<f32>,
    terminal_font_size: Option<f32>,
    ui_zoom: Option<f32>,
    text_scale: Option<f32>,
    show_hidden_files: Option<bool>,
    respect_gitignore: Option<bool>,
    dim_ignored_files: Option<bool>,
    index_max_files: Option<usize>,
    index_max_depth: Option<usize>,
    tree_dir_page: Option<usize>,
    word_wrap: Option<bool>,
    show_whitespace: Option<bool>,
    minimap: Option<bool>,
    breadcrumbs: Option<bool>,
    git_blame: Option<BlameMode>,
    approval_mode: Option<String>,
    show_pet: Option<bool>,
    agents: Vec<AgentPreset>,
    keybindings: HashMap<String, String>,
    /// プロジェクト単位でプラグインを切る / 設定を上書きする。
    plugins: Option<PluginsConfig>,
}

/// UI 上での選択を保持する軽量ステート (~/.zaivern/state.toml)。
/// config.toml はユーザーのコメント付き手書きファイルなので上書きしない。
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct UiState {
    theme: Option<String>,
    approval_mode: Option<String>,
    show_pet: Option<bool>,
    word_wrap: Option<bool>,
    show_whitespace: Option<bool>,
    /// 画面全体のズーム倍率。⌘+ / ⌘- は UI からの操作なので、
    /// 手書きの config.toml ではなく state 側に覚える
    /// (config.toml はアプリが書き換えない方針)。
    ui_zoom: Option<f32>,
    /// 文字サイズだけの倍率。ui_zoom と同じ理由で state 側に覚える。
    text_scale: Option<f32>,
    /// 最後に「What's New」を見た版。空 = 一度も見ていない (初回起動)。
    /// 手書きの config.toml ではなく state 側に置く (アプリが書き換えるため)。
    last_seen_version: Option<String>,
    minimap: Option<bool>,
    breadcrumbs: Option<bool>,
    /// 差分ビューの表示モード ("side_by_side" | "inline")。
    diff_view: Option<String>,
    git_blame: Option<BlameMode>,
    /// D&D の移動確認 / ゴミ箱の使用。確認ダイアログの「今後確認しない」と
    /// ツリーのメニューから切り替えるものなので、手書きの config.toml では
    /// なく state 側に置く (config.toml をアプリが書き換えない方針)。
    confirm_drag_and_drop: Option<bool>,
    enable_trash: Option<bool>,
    /// タブ切替を MRU で回すか (パレットから切り替えるので state 側に置く)。
    tab_switch_mru: Option<bool>,
    /// プレビュータブを使うか (同上)。
    preview_tabs: Option<bool>,
    pet_image: Option<String>,
    pet_x: Option<f32>,
    pet_y: Option<f32>,
    pet_variant: Option<String>,
    pet_scale: Option<f32>,
    pet_free_roam: Option<bool>,
    pet_sleep: Option<bool>,
    pet_sounds: Option<bool>,
    pet_bubbles: Option<bool>,
    pet_auto_yes: Option<bool>,
    pet_approve_keys: Option<String>,
    pet_deny_keys: Option<String>,
    voice_engine: Option<String>,
    voice_target: Option<String>,
    voice_lang: Option<String>,
    voice_command: Option<String>,
    voice_keyword: Option<String>,
    /// SSH リモート接続の踏み台 (接続先だけ。鍵は保存しない)。
    ssh_tunnel_host: Option<String>,
    /// 監視役 LLM の選択。UI から選ぶものなので、手書きの config.toml ではなく
    /// state 側に置く (config.toml をアプリが書き換えない方針に合わせる)。
    super_agent_command: Option<String>,
    super_agent_session_title: Option<String>,
    super_agent_enabled: Option<bool>,
    super_agent_timeout_secs: Option<u64>,
    /// レート制限の自動フェイルオーバーの有効/無効。UI (パレット / Cockpit) から
    /// 切り替えるものなので、手書きの config.toml ではなく state 側に置く。
    /// 上限やクールダウンの数値は `[failover]` (config.toml) のまま。
    failover_enabled: Option<bool>,
    /// ⌃1〜⌃9 の起動バーの割り当て (プリセット名の並び)。
    /// **単純値の配列**なのでテーブル配列 (`palette_recent`) より前に置くこと。
    /// 並びはユーザーが決めたものをそのまま書き、**読み書きで一切並べ替えない**。
    quick_launch: Option<Vec<String>>,
    /// コマンドパレットの MRU。**配列のテーブルなので必ず最後の項目に置く**
    /// (TOML は値をテーブルより先に書く必要があり、途中に置くと
    /// `toml::to_string_pretty` が state.toml 全体を書けなくなる)。
    palette_recent: Option<Vec<PaletteRecent>>,
}

pub fn config_path() -> PathBuf {
    zaivern_dir().join("config.toml")
}

// 実体は save_state_to_dir() が dir から組むため、現在はテストからのみ参照。
#[allow(dead_code)]
pub fn state_path() -> PathBuf {
    zaivern_dir().join("state.toml")
}

/// `~/.zaivern` の場所。home が取れない場合は `./.zaivern` にフォールバック。
/// ディレクトリの作成 (create_dir_all) は行わない — 呼び出し側の責務。
///
/// **`ZAIVERN_HOME` で差し替えられる。** 台帳キーの移行のように「本番の経路を
/// 端から端まで動かさないと確かめられない」処理があり、それを実 `$HOME` で
/// 試すわけにいかない。`$HOME` のすり替えは unix でしか効かない
/// (`dirs` 5 の Windows 実装は `SHGetKnownFolderPath` を叩くので環境変数を
/// 見ない) ため、これが無いと**テスト可能性が OS で非対称**になる。
pub(crate) fn zaivern_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("ZAIVERN_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(d);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".zaivern")
}

/// `~/.zaivern/config.toml` の `ui_language` **だけ**を読む軽い入口。
///
/// CLI (`zai <sub>`) は GUI の設定一式を組み立てない (ワークスペースが無い)
/// ので、表示言語のためだけに `Config` を全部読むのは重い。ここは
/// **1 ファイルを 1 度読んで 1 つのキーを見るだけ**。
/// 読めない・書いていない場合は `"auto"`。
pub fn ui_language_pref() -> String {
    let raw = std::fs::read_to_string(config_path()).unwrap_or_default();
    raw.parse::<toml::Table>()
        .ok()
        .and_then(|t| t.get("ui_language")?.as_str().map(str::to_string))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::locale::AUTO.to_string())
}

pub const DEFAULT_CONFIG: &str = r#"# ══════════════════════════════════════════════════
#  Zaivern Code 設定ファイル
#  場所: ~/.zaivern/config.toml
#  プロジェクトごとの上書き: <workspace>/.zaivern.toml
#  変更後はコマンドパレット (⌘⇧P) の「設定を再読み込み」で反映されます
# ══════════════════════════════════════════════════

# UI の表示言語。"auto" は OS の言語に従います (取れなければ日本語)。
#   同梱: "en" | "ja" | "zh-CN" | "ko" | "pt-BR" | "es"
#   ~/.zaivern/locales/<id>.json を置けば、その <id> も選べます (例: "fr")
#   同じ ID を書けば同梱の訳を上書きできます (再ビルド不要)
ui_language = "auto"

# テーマ (ダーク): "zaivern-dark" | "zaivern-midnight" | "zaivern-nordic"
#                 | "zaivern-ember" | "zaivern-forest" | "zaivern-ocean" | "zaivern-carbon"
# テーマ (ライト): "zaivern-light" | "zaivern-paper" | "zaivern-daylight" | "zaivern-frost"
# カラーテーマJSON (VS Code 互換形式) へのフルパスも指定できます
# (~/.zaivern/themes とプラグイン同梱のテーマは 🎨 メニューに自動で並びます)
theme = "zaivern-dark"
# エディタ本文の文字サイズ (pt)。⌘+ / ⌘- の画面ズームとは別で、本文だけに効きます。
editor_font_size = 15.0
# ターミナル / エージェントタブの文字サイズ (pt)。
terminal_font_size = 13.0
# ドット始まりのファイル・フォルダをツリーと検索に出す。
show_hidden_files = true

# .gitignore (+ .git/info/exclude + core.excludesFile) を尊重して
# ファイルツリーとファイル検索 (⌘P) から除外します。既定は true。
# false にすると node_modules / target なども全部並びます。
# respect_gitignore = true
# 無視されたファイルを隠さず「薄く」表示する。既定は true (VS Code と同じ:
# explorer.excludeGitIgnore = false + git.decorations.enabled = true)。
# false にすると node_modules / target などはツリーから消えます。
# respect_gitignore = false のときは効きません。
# dim_ignored_files = true

# ファイル検索 (⌘P) の索引の上限。上限に達したらパレットにその旨を出します
# (黙って切り捨てません)。索引はバックグラウンドで作るので UI は止まりません。
#   index_max_files = 索引に載せる最大件数。
#   index_max_depth = 索引が潜る最大の深さ (ルート直下 = 1)。
# index_max_files = 50000
# index_max_depth = 32

# ツリーの 1 フォルダで一度に描く行数。超えたぶんは「さらに N 件」に畳みます
# (巨大フォルダで数万行を描いてカクつかせないため)。
# tree_dir_page = 300

# ドラッグ&ドロップで移動するとき「"X" を "Y" へ移動しますか?」を出す。
# オフにしても、同名ファイルを潰す操作の確認だけは必ず出ます。
# confirm_drag_and_drop = true
# 削除をゴミ箱へ送る。オフにすると削除は常に完全削除 (取り消せません)。
# enable_trash = true

# 画面全体のズーム (0.5〜3.0)。UI の全部が一緒に拡大縮小します。
# ⌘+ / ⌘- / ⌘0 で変えた値は ~/.zaivern/state.toml に覚えるので、
# ここに書くのは「起動時の初期値を固定したい」ときだけで構いません。
# ファイル単位のズーム (⌘⌥+ / ⌘⌥- / ⌘⌥0) はタブごとの一時的な値で、保存しません。
# ui_zoom = 1.0
# 文字サイズだけの倍率 (余白やボタンの大きさは変えない)。⌘⇧+ / ⌘⇧- / ⌘⇧0 で変えられる。
# text_scale = 1.0

# エディタ本文の折り返しと空白文字 (·/→) の可視化
# (表示メニュー・コマンドパレットの「折り返し切替」「空白文字表示切替」でも変更できます)
#   word_wrap       = 本文を可用幅で折り返す。
#   show_whitespace = スペースを「·」タブを「→」で見せる。
# word_wrap = false
# show_whitespace = false

# カーソル下のシンボルと同じものを本文で薄くハイライトする (LSP documentHighlight)
# (コマンドパレットの「同一シンボルのハイライト切替」でも変更できます)
# lsp_highlight_occurrences = true

# 診断メッセージを本文の行末に淡色で出す (VS Code の Error Lens 相当)
# 出るのは**キャレット行だけ**です。オフにしても波線とホバーは残ります
# (コマンドパレットの「行末の診断メッセージ切替」でも変更できます)
# inline_diagnostics = true

# LSP のインレイヒント (推論された型・引数名) を本文の行末に淡色で出す
# 既定はオフです (行末が型名で埋まるのと、打鍵ごとにサーバーへの往復が増えるため)
# (コマンドパレットの「インラインヒントの表示切替」でも変更できます)
# inlay_hints = false
# 取り消し (Undo) 履歴の粒度とメモリ上限。タブ 1 枚あたりの値です。
#   undo_merge_ms  = 続けて打った文字を 1 段にまとめる時間しきい値 (ミリ秒)。
#                    これを超えて間が空くと別の段になります。0 にすると 1 打鍵 = 1 段。
#   undo_max_steps = 保持する最大段数。超えたぶんは古い方から捨てます。
#   undo_max_bytes = 履歴が抱える差分の合計バイト上限。巨大な一括置換を
#                    繰り返してもここで頭打ちになります。
# undo_merge_ms = 400
# undo_max_steps = 400
# undo_max_bytes = 4194304

# ── Hot Exit (未保存の本文の復元) ──
# 未保存のまま落ちても、次の起動で本文を戻します (VS Code の files.hotExit 相当)。
# 退避先は ~/.zaivern/hotexit/<ワークスペース>/ で、保存して閉じれば消えます。
# ディスク側が外から書き換わっていた場合は、黙って戻さず選ばせます。
#   hot_exit             = 未保存の本文を退避して復元する。既定はオン。
#   hot_exit_max_kb      = 1 バッファあたりの退避上限 (KiB)。超えたぶんは
#                          退避せず、その旨をトーストで伝えます。
#   hot_exit_interval_ms = 退避を書き出す最短間隔 (ミリ秒)。打鍵のたびに
#                          書かないためのスロットリングです。
# hot_exit = true
# hot_exit_max_kb = 4096
# hot_exit_interval_ms = 1500

# ── ローカルヒストリ (VCS に依らない取り消し履歴) ──
# コミットしていない変更を「20 分前の姿」まで戻せます。エディタの編集だけでなく
# エージェントの shell が書いた変更 (rm -rf を含む) も、ファイルシステム側で
# 見ているので拾えます。取り込みは保存・エージェントのターン境界・履歴を開いた
# 時だけで、何もしていない間は 1 バイトも読み書きしません。
# 置き場は ~/.zaivern/local_history/<ワークスペース>/ で、内容は 1 個だけ持つ
# アドレス指定の倉庫に入るので、同じ内容が何世代あっても容量は増えません。
# コマンドパレットの「ローカルヒストリ: 履歴を開く」から見られます。
#   local_history           = 記録するか。既定はオン。
#   local_history_days      = 保持する日数。**壁時計ではなく活動時間**で数えるので、
#                             1 週間マシンを離れても予算を食いません。
#   local_history_gap_hours = これを超える空白は「活動していない」と見なして
#                             1ms と数えます。
# local_history = true
# local_history_days = 5
# local_history_gap_hours = 12
# chord (2 打鍵) の 1 打鍵目を握ってから、続きの打鍵一覧 (which-key) を
# 出すまでの待ち時間 (ミリ秒)。0 にすると即座に出ます。
# 待ちがあるのは、chord を淀みなく打ち切る人にポップアップを見せないためです
# (2 打鍵目以降は待ちません)。
# whichkey_delay_ms = 200

# ミニマップ (エディタ右端の遠景) とブレッドクラム (上部のパンくず)
# (表示メニュー・コマンドパレットの「ミニマップの表示切替」「ブレッドクラムの表示切替」でも変更できます)
#   minimap     = エディタ右端の遠景。本文の幅を 64px 使うため既定はオフ。
#                 狭い画面では設定がオンでも自動的に隠れます。
#   breadcrumbs = 上部のパンくず (ワークスペース › フォルダ › ファイル › シンボル)。
# minimap = false
# breadcrumbs = true
# 差分ビューの既定の表示: "side_by_side" (左右 2 列) | "inline" (1 列)。
# 幅が足りないときは設定に関わらず 1 列へ自動で縮退します。
# diff_view = "side_by_side"
# ガターに git blame (著者 · 相対日時) を出す。既定はオフ
# (表示メニュー・コマンドパレットの「Git blame の表示切替」でも変更できます)
# git_blame = "off"        # "off" | "current" (カーソル行だけ) | "all" (全行)

# ── 保存時の整形 (VS Code の files.* / editor.formatOnSave 相当) ──
# どれも既定はオフ。保存しただけで差分が増えないようにするためで、
# コマンドパレットの「保存時に…」からも個別に切り替えられます。
# trim_trailing_whitespace = false   # 各行の末尾空白を落とす (全角スペースは残す)
# trim_final_newlines      = false   # 末尾の余分な空行を落とす
# insert_final_newline     = false   # 最終行に改行が無ければ足す
# format_on_save           = false   # LSP の整形をかけてから保存する

# ── 括弧の色分け / 縦のルーラー / インデント ──
# 括弧を入れ子の深さごとに色分けする (色はテーマの ANSI 表から採ります)
# bracket_colorization = true
# 縦のルーラーを引く桁 (等幅の桁数)。既定は空 = 1 本も引きません
# rulers = [80, 120]
# 開いたファイルの中身からインデントを推定する (オフなら下の 2 つをそのまま使う)
#   tab_size      = インデント 1 段の桁数。
#   insert_spaces = インデントにタブではなくスペースを使う。
# detect_indentation = true
# tab_size = 4
# insert_spaces = true
# タブ切替 (Ctrl+Tab / Ctrl+Shift+Tab) を最近使った順 (MRU) で回すか。
# 既定はオン = 押している間に候補一覧が出て、離したところで確定します
# (2 回押せば直前のファイルへ戻る)。false にすると並び順の巡回になります。
# (コマンドパレットの「タブ切替を最近使った順/並び順にする」でも変更できます)
# tab_switch_mru = true
# プレビュータブ (斜体の使い捨てタブ)。既定はオン = ツリーやパレットから
# 1 回クリックして開いたタブは次のプレビューで置き換わるので増え続けません。
# もう一度クリック / 編集 / ピン留め / ドラッグで確定タブになります。
# (コマンドパレットの「プレビュータブの切替」でも変更できます)
# preview_tabs = true

# 既定の権限モード (claude / codex / agy に自動適用)
#   "ask"   = 毎回ユーザー承認が必要（安全・デフォルト）
#   "auto"  = すべて自動YES（各CLIの bypass フラグを自動付与）
#   "agent" = Agent欄優先（プリセットのコマンドに書かれたフラグをそのまま使う。
#             「(全自動)」プリセットと通常プリセットを使い分けたい場合はこれ）
# ツールバーの 🛡/⚡/👾 ボタンでも切替できます
approval_mode = "ask"

# ── コスト上限とアラート ──────────────────────────────
# エージェントを何本も並列で走らせて「気付いたら $200」を防ぐ見張りです。
# 金額の単位は [pricing] の currency と同じで、消費はローカルのトランスクリプト
# から推定した額です (通信はしません)。上限が 0 のあいだは無制限で、
# ステータスバーにも 1px も出ません。
#   cost_limit_session = このアプリを起動してからの推定コストの上限。
#                        0 = 無制限。集計はトークン集計の窓 (直近 24 時間) の
#                        範囲で数えます。
#   cost_limit_daily   = 1 日ぶんの推定コストの上限。0 = 無制限。
#                        日の境界は UTC で切ります。std にタイムゾーン DB が
#                        無く、ローカル時刻で切ると出張・夏時間・OS のタイム
#                        ゾーン更新で「今日」の始まりが黙って動くためです
#                        (夏時間の移行日は 23 時間 / 25 時間の日が生まれます)。
#   cost_warn_ratio    = 上限の何割を使ったら警告するか (0.0〜1.0)。
#                        既定 0.8。残り 2 割は「走っているターンを終わらせて、
#                        並列度を落とすか上限を上げるかを決める」猶予です。
#   cost_limit_action  = 上限に達したときの動作。
#                        "notify" は知らせるだけ (既定。勝手に止めません)、
#                        "stop" は新規の送信を止めます (理由を画面に出します)。
# cost_limit_session = 0.0
# cost_limit_daily = 0.0
# cost_warn_ratio = 0.8
# cost_limit_action = "notify"

# フォルダを開き直したとき、前回のエージェントタブを復元して会話を再開する
# 既定は false — 起動しただけでは何も立ち上がりません。過去の会話は
# 「💬 セッション」タブから選んで再開します。
# true にすると前回のスクロールバックが見える状態で、claude は --continue /
# codex は resume --last 付きで起動します。
# restore_agents = false

# ターンが終わった時点で、そのエージェント自身の CLI に 2〜5 語の題名を作らせ、
# タブ名にする（並列で走らせたときにサイドバーで見分けるため）
# 既定は false — 有効にしたときだけ、ターンが終わった瞬間に 1 回だけ走ります。
# 手で付けた名前は上書きしません。生成に失敗したら黙って従来の名前のままです。
# 送るのは「あなたがそのエージェントへ送った指示文の冒頭」だけで、
# コードもエージェントの出力も送りません（実行は一時ディレクトリで行います）。
# auto_name_sessions = false

# デスクトップペット (🐾) の表示
show_pet = true

# ── 外出先への通知 (Webhook) ──────────────
# 承認待ち・終了・レート制限のイベントを外部サービスへ POST します (curl 使用)。
# ntfy ならスマホアプリを入れてトピックを購読するだけでプッシュ通知になります。
# Slack / Discord の Incoming Webhook URL はドメインから自動判別して JSON で送ります。
# webhook_url = "https://ntfy.sh/あなたのトピック名"
# webhook_url = "https://hooks.slack.com/services/XXX/YYY/ZZZ"
# webhook_url = "https://discord.com/api/webhooks/XXX/YYY"

# ── SSH リモート ──────────────
# 踏み台の接続先 ("user@host" / "user@host:port")。空 = 未設定。
# 鍵とパスフレーズはここに書きません — 認証は OS の ssh と ~/.ssh/config
# (ssh-agent) に任せます。ここに置くのは接続先だけです。
# ssh_tunnel_host = "user@example.com"

# ── ペットの好み設定 ──────────────
# pet_variant = "blocky"   # 見た目: "blocky" | "crab" | "cat" | "cloud"
# pet_scale = 1.0          # 大きさ: 0.75=小 / 1.0=中 / 1.4=大
# pet_free_roam = true     # うろうろ散歩
# pet_sleep = true         # 無操作で睡眠
# pet_sounds = true        # 効果音
# pet_bubbles = true       # 承認バブル
# pet_auto_yes = false    # 承認プロンプトへ自動でYES (オフ=ユーザー承認必須)
# pet_approve_keys = "\r"    # 承認時にPTYへ送るキー (Enter)
# pet_deny_keys = "\u001B"   # 拒否時にPTYへ送るキー (ESC)

# ── 自動YESの追加ルール ──────────────
# 自動YES は、CLI ごとの承認プロンプトの文言を Zaivern 内部の表と突き合わせて
# 「どのキーを送れば YES になるか」を決めています。CLI 側の更新で文言が変わると
# 表に当たらなくなり、自動YES が素通りします。そのときは Zaivern を入れ直さなくても
# ここへ自分のルールを足せば直せます (組み込みの表より先に評価されます)。
#
# 画面に pattern が含まれていたら reply のキー列を送ります。
#   reply = "\r"    Enter (矢印キー選択メニューの確定。既定)
#   reply = "y\r"   y と Enter ((y/n) 形式)
#   reply = "1"     番号キー (「1. Yes」形式で、カーソルが Yes に無いとき)
#   reply = "3\r"   番号 + Enter (「番号を入力してください」形式のメニュー)
#   agent = "agy"   そのエージェントのタブだけに効かせる (省略すると全部)
#                   agy=Antigravity / claude / codex / gemini … (実行ファイル名)
#
# [[auto_yes_rules]]
# pattern = "Allow access to this file?"
# reply = "\r"
# agent = "agy"
#
# [[auto_yes_rules]]
# pattern = "Continue with this plan?"
# reply = "y\r"
#
# 番号入力メニュー (アンケート等) の既定の選び方を上書きしたいとき。
# 組み込みは「スキップ肢 → 肯定肢 → (評点しか無ければ) 肯定側の端」の順に
# 選びますが、この行があればそちらが優先されます。
#
# [[auto_yes_rules]]
# pattern = "How would you rate"
# reply = "3\r"

# ── 承認ポリシー (統合承認キュー) ──────────────
# すべてのエージェントの承認要求を 1 本のキューへ集め、種別ごとに
# 「常に許可 / 常に拒否 / 毎回聞く」を決められます。全自動YES と違って
# **種別と範囲を絞れる**のと、判断が ~/.zaivern/approvals.jsonl に
# 追記されて後から監査できるのが違いです。
#
# kind     … file_read / file_write / file_delete / shell_command /
#            network_access / git_operation / package_install /
#            privilege / other
# scope    … global (全部) / agent (実行ファイル名) /
#            session (セッションID) / path (パス接頭辞)
# target   … scope の対象値 (global のときは不要)
# decision … ask / allow_once / allow_always / deny_always
#
# 具体的な範囲が優先されます: session > path > agent > global
# (path 同士は深い方が勝つ)。同じ具体性なら後に書いた方が勝ちます。
#
# ★ privilege (管理者権限の昇格) だけは allow_always を書いても効きません。
#   自動承認は決して行わず、必ず本人に聞きます。
#
# 例1: Claude のファイル読み取りは黙って通す
# [[approval_policies]]
# kind = "file_read"
# scope = "agent"
# target = "claude"
# decision = "allow_always"
#
# 例2: ネットワークアクセスはどのエージェントでも必ず拒否
# [[approval_policies]]
# kind = "network_access"
# scope = "global"
# decision = "deny_always"
#
# 例3: このフォルダ配下の書き込みだけ自動許可
# [[approval_policies]]
# kind = "file_write"
# scope = "path"
# target = "/Users/me/work/sandbox"
# decision = "allow_always"

# ── 音声入力 (🎤) ──────────────
# 🎤 を押すと録音が始まり、⏹ を押すまで話した内容がエージェントの入力欄へ
# 流れ込み続けます。Enter は送られないので、内容を確認して自分で Enter を
# 押すまで送信されません。Enter で入力欄が空になっても録音は続いたままなので、
# そのまま次の指示を話せます。ツールバーの 🎤 メニューからも変更できます。
#
# voice_engine = "auto"    # "auto" | "mac" | "powershell" | "browser" | "command" | "off"
# voice_target = "active"  # 届け先: "active"(アクティブなエージェント) | "broadcast"(全員)
# voice_lang = "ja-JP"     # 認識する言語
# voice_keyword = ""       # このキーワードを話すと Enter まで自動送信 ("" = 常に手動)
#
# "auto" は上から順に:
#   macOS                     → "mac"        内蔵の Swift ヘルパー
#   voice_command が設定済み  → "command"    下記の外部コマンド
#   Windows (対応言語あり)    → "powershell" Windows 標準の音声認識 (オフライン)
#   それ以外                  → "browser"    ブラウザの音声入力ページを開く
#
# "browser" はスマホリモートの /voice を 127.0.0.1 で開き、ブラウザの音声認識に
# 喋らせます。マイクはブラウザ側なので、ページを閉じれば止まります。
# Chrome が入っていれば Chrome で開きます (Edge の音声認識は不安定なため)。
#
# 独自の認識エンジンを使う場合は voice_command を設定します。標準出力へ 1 行ずつ
# 認識テキストを吐き、標準入力に "q" が来たら終了するコマンドを想定しています
# ({lang} は言語に置換)。auto のままでも、設定されていれば mac 以外では優先されます。
# voice_engine = "command"
# voice_command = "my-stt --lang {lang} --stream"

# ── AIエージェント / ターミナルのプリセット ──────────────
# command はログインシェル (-lc) 経由で実行されます。
# 空文字 "" は普通のシェルを起動します。
# env でプリセット固有の環境変数を設定できます。
# カタログ登録済みの CLI (claude / codex / gemini / agy / cursor-agent / droid …)
# で始まるコマンドには承認モードが自動適用されます
# (approval_mode = "agent" ならコマンドをそのまま尊重します)。
# ここに無い CLI も「エージェント追加」から選べます (対応 CLI は 30 種類以上)。

[[agents]]
name = "Claude Code"
icon = "👾"
command = "claude"

[[agents]]
name = "Claude Code (全自動)"
icon = "⚡"
command = "claude --dangerously-skip-permissions"

[[agents]]
name = "Codex"
icon = "💡"
command = "codex"

[[agents]]
name = "Codex (全自動)"
icon = "⚡"
command = "codex --dangerously-bypass-approvals-and-sandbox"

[[agents]]
name = "Gemini CLI"
icon = "✨"
command = "gemini"

[[agents]]
name = "Gemini CLI (全自動)"
icon = "⚡"
command = "gemini --yolo"

[[agents]]
name = "Antigravity"
icon = "🚀"
command = "agy"

[[agents]]
name = "Antigravity (全自動)"
icon = "⚡"
command = "agy --dangerously-skip-permissions"

[[agents]]
name = "Cursor"
icon = "🖱"
command = "cursor-agent"

[[agents]]
name = "Cursor (全自動)"
icon = "⚡"
command = "cursor-agent -f"

[[agents]]
name = "Droid"
icon = "👾"
command = "droid"

[[agents]]
name = "Droid (全自動)"
icon = "⚡"
command = "droid --skip-permissions-unsafe"

[[agents]]
name = "Shell"
icon = "🖥"
command = ""

# [[agents]]
# name = "Claude (Opus 明示)"
# icon = "💡"
# command = "claude --model claude-opus-4-8"
# env = { MAX_THINKING_TOKENS = "31999" }

# ── アカウント/プロファイル切替 ──────────────
# env に設定ディレクトリを指定すると、同じ CLI を別アカウント (別サブスク) で
# 並列起動できます。片方の制限に当たっても、もう片方はそのまま走り続けます。
# [[agents]]
# name = "Claude (仕事用アカウント)"
# icon = "🏢"
# command = "claude"
# env = { CLAUDE_CONFIG_DIR = "~/.claude-work" }
#
# [[agents]]
# name = "Codex (サブ垢)"
# icon = "🅾"
# command = "codex"
# env = { CODEX_HOME = "~/.codex-alt" }

# ── レート制限時の自動フェイルオーバー ──────────
# 上限に当たったら、同じ CLI の別プロファイル → 別 CLI の順で切替先を選び、
# 新しいセッションを立てて続きを渡します。上限に当たった側のセッションは
# **そのまま残します** (終了させません)。
# **既定は無効** — 有効にするのは ⌘⇧P →「自動フェイルオーバーを有効化」か、
# 📊 プラン使用量ウィンドウのチェックボックス、あるいは下の enabled = true。
# 上の [[agents]] に別プロファイルを 2 つ以上書いておかないと切替先がありません。
# [failover]
# enabled = false           # 自動で切り替えるか
# max_switches = 3          # 1 セッションあたりの連鎖切替の上限
# max_attempts = 2          # 同じ枠を試し直す上限
# cooldown_secs = 300       # 枯れた枠を寝かせる基準時間 (失敗のたびに倍)
# max_cooldown_secs = 3600  # クールダウンの上限
# verify_secs = 90          # 切替先が動いていると見なすまでの観察時間
# min_screen_hits = 2       # 画面由来の検知を信じるまでの連続一致回数

# ── 🏁 レースの勝者評価 ─────────────────
# 候補の差分から「読ませても意味がないもの」を落とす条件。
# 書式は .gitignore と同じ。書かなければ下の既定がそのまま使われます。
# [race_eval]
# exclude = ["Cargo.lock", "package-lock.json", "target/", "node_modules/", "*.min.js"]
# max_diff_bytes = 24576    # 候補 1 本ぶんの差分の上限 (超えたら切り詰めた旨を出す)
# max_total_bytes = 98304   # 全候補あわせた上限
# max_line_bytes = 4096     # これより長い行を含むファイルは生成物とみなして落とす

# [[agents]]
# name = "Gemini CLI"
# icon = "✨"
# command = "gemini"

# ── キーバインド上書き(例)──────────────
# [keybindings]
# save = "cmd+s"
# toggle_terminal = "ctrl+`"
# toggle_comment = "cmd+/"

# ── プラグイン ──────────────────────
# 標準プラグインは初回起動時に ~/.zaivern/plugins/ へ展開され、
# 何も書かなくてもすべて有効です。切りたいものだけここに並べます。
# [plugins]
# disabled = ["usage-meter"]

# プラグインごとの設定 (マニフェストの [[setting]] key に対応)
# [plugins.settings.worktrees]
# parallel_count = "3"
#
# [plugins.settings.remote-host]
# host = "user@example.com"
# remote_path = "/home/user/work"
"#;

/// Write the default config template if none exists yet.
pub fn ensure_default() {
    let path = config_path();
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, DEFAULT_CONFIG);
    }
}

/// Load global config merged with each root's project overlay.
/// `with_state`: UI 選択 (state.toml) を最後に適用するか。
/// 起動時は true、「設定を再読み込み」では false (config.toml を正とする)。
///
/// マルチルート時のマージ規則: `roots` の順に `<root>/.zaivern.toml` を適用する。
/// つまり **後のルートが前のルートを上書きする (last wins)**。
/// これは「後から追加したフォルダの設定が効く」という直感に沿い、また
/// 単一ルート時の挙動と完全に一致する。
/// ただし `agents` は上書きではなく順に追加、`keybindings` はキー単位で
/// 上書きマージ (last wins) — いずれも従来の単一ルート時の規則そのまま。
pub fn load(roots: &[PathBuf], with_state: bool) -> Config {
    ensure_default();
    load_from_dir(&zaivern_dir(), roots, with_state)
}

/// `load()` の実体。テストから一時ディレクトリを差し込めるよう分離している。
pub(crate) fn load_from_dir(dir: &Path, roots: &[PathBuf], with_state: bool) -> Config {
    let mut cfg: Config = match std::fs::read_to_string(dir.join("config.toml")) {
        Ok(s) => match toml::from_str(&s) {
            Ok(c) => c,
            Err(e) => {
                // 手書きの 1 文字ミスで全設定 (自作プリセット・キーバインド等)
                // が黙って既定値に戻ると「全部消えた」ように見える。
                // 原因を stderr に出し、復旧用に .broken へ控えてから既定値で
                // 起動する (以後の保存で壊れたファイルが上書きされても戻せる)。
                eprintln!("zaivern: config.toml のパースに失敗: {e}");
                let _ = std::fs::copy(dir.join("config.toml"), dir.join("config.toml.broken"));
                eprintln!("zaivern: 壊れた config.toml を config.toml.broken に控えました");
                Config::default()
            }
        },
        Err(_) => Config::default(),
    };

    if cfg.agents.is_empty() {
        cfg.agents = default_agents();
    }

    if with_state {
        if let Ok(s) = std::fs::read_to_string(dir.join("state.toml")) {
            if let Ok(st) = toml::from_str::<UiState>(&s) {
                if let Some(t) = st.theme {
                    cfg.theme = t;
                }
                if let Some(a) = st.approval_mode {
                    cfg.approval_mode = a;
                }
                if let Some(p) = st.show_pet {
                    cfg.show_pet = p;
                }
                if let Some(v) = st.word_wrap {
                    cfg.word_wrap = v;
                }
                if let Some(v) = st.show_whitespace {
                    cfg.show_whitespace = v;
                }
                if let Some(v) = st.ui_zoom {
                    cfg.ui_zoom = v;
                }
                if let Some(v) = st.text_scale {
                    cfg.text_scale = v;
                }
                if let Some(v) = st.last_seen_version {
                    cfg.last_seen_version = v;
                }
                if let Some(v) = st.minimap {
                    cfg.minimap = v;
                }
                if let Some(v) = st.breadcrumbs {
                    cfg.breadcrumbs = v;
                }
                if let Some(v) = st.diff_view {
                    cfg.diff_view = v;
                }
                if let Some(v) = st.git_blame {
                    cfg.git_blame = v;
                }
                if let Some(v) = st.confirm_drag_and_drop {
                    cfg.confirm_drag_and_drop = v;
                }
                if let Some(v) = st.enable_trash {
                    cfg.enable_trash = v;
                }
                if let Some(v) = st.tab_switch_mru {
                    cfg.tab_switch_mru = v;
                }
                if let Some(v) = st.preview_tabs {
                    cfg.preview_tabs = v;
                }
                if st.pet_image.is_some() {
                    cfg.pet_image = st.pet_image;
                }
                if st.pet_x.is_some() {
                    cfg.pet_x = st.pet_x;
                }
                if st.pet_y.is_some() {
                    cfg.pet_y = st.pet_y;
                }
                if let Some(v) = st.pet_variant {
                    cfg.pet_variant = v;
                }
                if let Some(v) = st.pet_scale {
                    cfg.pet_scale = v;
                }
                if let Some(v) = st.pet_free_roam {
                    cfg.pet_free_roam = v;
                }
                if let Some(v) = st.pet_sleep {
                    cfg.pet_sleep = v;
                }
                if let Some(v) = st.pet_sounds {
                    cfg.pet_sounds = v;
                }
                if let Some(v) = st.pet_bubbles {
                    cfg.pet_bubbles = v;
                }
                if let Some(v) = st.pet_auto_yes {
                    cfg.pet_auto_yes = v;
                }
                if let Some(v) = st.pet_approve_keys {
                    cfg.pet_approve_keys = v;
                }
                if let Some(v) = st.pet_deny_keys {
                    cfg.pet_deny_keys = v;
                }
                if let Some(v) = st.voice_engine {
                    cfg.voice_engine = v;
                }
                if let Some(v) = st.voice_target {
                    cfg.voice_target = v;
                }
                if let Some(v) = st.voice_lang {
                    cfg.voice_lang = v;
                }
                if let Some(v) = st.voice_command {
                    cfg.voice_command = v;
                }
                if let Some(v) = st.voice_keyword {
                    cfg.voice_keyword = v;
                }
                if let Some(v) = st.ssh_tunnel_host {
                    cfg.ssh_tunnel_host = v;
                }
                if let Some(v) = st.super_agent_command {
                    cfg.super_agent.command = v;
                }
                if let Some(v) = st.super_agent_session_title {
                    cfg.super_agent.session_title = v;
                }
                if let Some(v) = st.super_agent_enabled {
                    cfg.super_agent.enabled = v;
                }
                if let Some(v) = st.super_agent_timeout_secs {
                    cfg.super_agent.timeout_secs = v;
                }
                if let Some(v) = st.palette_recent {
                    cfg.palette_recent = v;
                }
                if let Some(v) = st.failover_enabled {
                    cfg.failover.enabled = v;
                }
                // 起動バーの割り当ては「空の配列」も意味を持つ
                // (= ユーザーが全部外した → 1px も描かない)。Option のまま渡す。
                if let Some(v) = st.quick_launch {
                    cfg.quick_launch = Some(v);
                }
            }
        }
    }

    // overlay を重ねる前のグローバル値を控える。save_state はこの控えを書く。
    cfg.global_theme = cfg.theme.clone();
    cfg.global_approval_mode = cfg.approval_mode.clone();
    cfg.global_show_pet = cfg.show_pet;
    cfg.global_word_wrap = cfg.word_wrap;
    cfg.global_show_whitespace = cfg.show_whitespace;
    cfg.global_ui_zoom = crate::zoom::clamp(cfg.ui_zoom);
    cfg.global_text_scale = crate::zoom::clamp(cfg.text_scale);
    cfg.global_minimap = cfg.minimap;
    cfg.global_breadcrumbs = cfg.breadcrumbs;
    cfg.global_git_blame = cfg.git_blame;
    cfg.global_plugins = cfg.plugins.clone();

    for root in roots {
        apply_overlay(&mut cfg, root);
    }

    if cfg.approval_mode != "auto" && cfg.approval_mode != "agent" {
        cfg.approval_mode = "ask".into();
    }
    if cfg.global_approval_mode != "auto" && cfg.global_approval_mode != "agent" {
        cfg.global_approval_mode = "ask".into();
    }
    cfg.editor_font_size = cfg.editor_font_size.clamp(8.0, 32.0);
    cfg.terminal_font_size = cfg.terminal_font_size.clamp(7.0, 28.0);
    // ここを外すと `ui_zoom = 0` で UI が 1 ピクセルに潰れて操作不能になる
    // (設定を戻す口も潰れるので、必ず範囲へ収めてから返す)。
    cfg.ui_zoom = crate::zoom::clamp(cfg.ui_zoom);
    // 文字サイズ倍率も同じ理由で必ず範囲へ収める
    // (0 にすると文字が消えて設定を戻す口ごと読めなくなる)。
    cfg.text_scale = crate::zoom::clamp(cfg.text_scale);
    cfg.pet_scale = cfg.pet_scale.clamp(0.5, 2.0);
    // 期限が 0 だと診断側で毎回丸められて分かりにくいので、ここで下限を揃える。
    cfg.super_agent.timeout_secs = cfg.super_agent.timeout_secs.clamp(5, 600);
    // LLM 相談の ON/OFF は「監視役が選ばれているか」から導く。
    // `[supervisor] llm_escalation` を単独で立てても、相談相手が居なければ
    // 何も起きない (request_diagnosis が no-op になる) ため、UI の見え方と
    // 実挙動がずれないようここで一本化する。
    cfg.supervisor.llm_escalation = cfg.super_agent.active_command().is_some();
    // 自動YESのユーザー定義ルールを応答エンジンへ渡す。
    // ここで配るので、設定を読み直せば再起動なしで反映される。
    publish_auto_yes_rules(&cfg);
    // 承認ポリシーと承認/拒否キーも同じタイミングで配る。
    publish_approval_policies(&cfg);
    // 機能が宣言した設定のうち、実行時の旗へ写す必要があるものも配る。
    apply_runtime_flags(&cfg);
    cfg
}

/// 機能の設定を**実行時の旗**へ写す。設定を読み直せば再起動なしで反映される。
///
/// 旗を持っている側 (`notify` 等) は依存を持たない下層なので、設定を読む
/// 経路をそちらへ持ち込まない。**写す向きはここからの一方通行**にして、
/// 「同じ事実を 2 箇所に持って片方だけ更新される」経路を作らない。
///
/// 呼ぶのは 2 か所だけ: [`load`] の最後と [`set_setting_value`] の機能設定枝。
/// この 2 つで「起動 / 設定の読み直し / ワークスペース切り替え / 設定画面での
/// 変更」が全部覆える。
pub fn apply_runtime_flags(cfg: &Config) {
    crate::notify::set_enabled(cfg.feature_bool(crate::features::notifications::KEY_ENABLED));
    crate::notify::set_sound(cfg.feature_bool(crate::features::notifications::KEY_SOUND));
}

/// `[[auto_yes_rules]]` を自動YESの応答エンジン (src/agents.rs) へ登録する。
pub fn publish_auto_yes_rules(cfg: &Config) {
    let rules: Vec<(String, String, String)> = cfg
        .auto_yes_rules
        .iter()
        .map(|r| (r.pattern.clone(), r.reply.clone(), r.agent.clone()))
        .collect();
    crate::agents::set_user_prompt_rules(&rules);
}

/// `[[approval_policies]]` と承認/拒否キーを統合承認キュー
/// (src/approvals.rs) へ登録する。設定を読み直せば再起動なしで反映される。
pub fn publish_approval_policies(cfg: &Config) {
    crate::agents::approvals::set_policies(approval_policies_from_config(cfg));
    crate::agents::approvals::set_reply_keys(&cfg.pet_approve_keys, &cfg.pet_deny_keys);
}

/// `<root>/.zaivern.toml` を 1 枚 `cfg` に重ねる。無ければ何もしない。
fn apply_overlay(cfg: &mut Config, root: &Path) {
    let overlay_path = root.join(".zaivern.toml");
    if let Ok(s) = std::fs::read_to_string(&overlay_path) {
        if let Ok(o) = toml::from_str::<Overlay>(&s) {
            if let Some(t) = o.theme {
                cfg.theme = t;
            }
            if let Some(v) = o.editor_font_size {
                cfg.editor_font_size = v;
            }
            if let Some(v) = o.terminal_font_size {
                cfg.terminal_font_size = v;
            }
            if let Some(v) = o.ui_zoom {
                cfg.ui_zoom = v;
            }
            if let Some(v) = o.text_scale {
                cfg.text_scale = v;
            }
            if let Some(v) = o.show_hidden_files {
                cfg.show_hidden_files = v;
            }
            if let Some(v) = o.respect_gitignore {
                cfg.respect_gitignore = v;
            }
            if let Some(v) = o.dim_ignored_files {
                cfg.dim_ignored_files = v;
            }
            if let Some(v) = o.index_max_files {
                cfg.index_max_files = v;
            }
            if let Some(v) = o.index_max_depth {
                cfg.index_max_depth = v;
            }
            if let Some(v) = o.tree_dir_page {
                cfg.tree_dir_page = v;
            }
            if let Some(v) = o.word_wrap {
                cfg.word_wrap = v;
            }
            if let Some(v) = o.show_whitespace {
                cfg.show_whitespace = v;
            }
            if let Some(v) = o.minimap {
                cfg.minimap = v;
            }
            if let Some(v) = o.breadcrumbs {
                cfg.breadcrumbs = v;
            }
            if let Some(v) = o.git_blame {
                cfg.git_blame = v;
            }
            if let Some(v) = o.approval_mode {
                cfg.approval_mode = v;
            }
            if let Some(v) = o.show_pet {
                cfg.show_pet = v;
            }
            cfg.agents.extend(o.agents);
            // extend ではなくキー単位の上書きマージ
            for (k, v) in o.keybindings {
                cfg.keybindings.insert(k, v);
            }
            if let Some(p) = o.plugins {
                // 無効リストは追記 (プロジェクト側で追加で切れる)
                for name in p.disabled {
                    cfg.plugins.set_enabled(&name, false);
                }
                for (plugin, kv) in p.settings {
                    for (k, v) in kv {
                        cfg.plugins.set_setting(&plugin, &k, &v);
                    }
                }
            }
        }
    }
}

/// config.toml の `[plugins]` 区画だけを現在の設定で書き直す。
///
/// プラグインの有効/無効と設定値は「config.toml が唯一の正」とする。
/// state.toml と二重管理にすると、ユーザーが config.toml を編集しても
/// 効かない状況が生まれて混乱するため。
///
/// `[plugins]` と `[plugins.settings.*]` 以外の行は 1 行も触らないので、
/// ユーザーのコメントや並び順は保たれる (区画内のコメントは失われる)。
pub fn save_plugins_section(cfg: &Config) -> Result<(), String> {
    // セッション中の値 (overlay 適用済み) ではなくグローバルの控えを書く。
    // UI から変更したときは呼び出し側が控えも更新している。
    save_plugins_config(&cfg.global_plugins)
}

/// `[[agents]]` ブロック 1 件分の TOML テキストを作る。
///
/// 手で組み立てずに toml クレートへ通すのは、名前やコマンドに `"` や `\` が
/// 入っていても壊れた config.toml を書かないため。
/// env はインラインテーブルにする。追記位置に関係なく 1 行で閉じるので、
/// 後からさらに `[[agents]]` を足しても前のブロックに吸われる事故が起きない。
fn render_agent_preset(p: &AgentPreset) -> String {
    let mut s = String::from("\n[[agents]]\n");
    let kv = |k: &str, v: &str| format!("{k} = {}\n", toml::Value::String(v.to_string()));
    s.push_str(&kv("name", &p.name));
    s.push_str(&kv("icon", &p.icon));
    s.push_str(&kv("command", &p.command));
    if let Some(cwd) = &p.cwd {
        s.push_str(&kv("cwd", cwd));
    }
    if !p.env.is_empty() {
        // 並びを固定して、書き出しを決定的にする。
        let mut keys: Vec<&String> = p.env.keys().collect();
        keys.sort();
        let body: Vec<String> = keys
            .iter()
            .map(|k| {
                format!(
                    "{} = {}",
                    toml::Value::String((*k).clone()),
                    toml::Value::String(p.env[*k].clone())
                )
            })
            .collect();
        s.push_str(&format!("env = {{ {} }}\n", body.join(", ")));
    }
    s
}

/// config.toml の末尾に `[[agents]]` を 1 件書き足す。
///
/// 既存の行は 1 文字も触らない。カタログは「そこから足す元ネタ」であって
/// 利用者のプリセット一覧の置き換えではないので、手書きのコメントも並び順も
/// そのまま残さなければならない。
pub fn append_agent_preset(preset: &AgentPreset) -> Result<(), String> {
    let path = config_path();
    ensure_default();
    let mut raw = std::fs::read_to_string(&path).unwrap_or_default();
    if !raw.is_empty() && !raw.ends_with('\n') {
        raw.push('\n');
    }
    raw.push_str(&render_agent_preset(preset));
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, raw).map_err(|e| format!("config.toml を書けません: {e}"))
}

/// config.toml から `[plugins]` 区画だけを読む (GUI を起動せずに使える)。
pub fn load_plugins_config() -> PluginsConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| toml::from_str::<Config>(&s).ok())
        .map(|c| c.plugins)
        .unwrap_or_default()
}

/// `[plugins]` 区画だけを書き戻す。CLI と GUI の両方がここを通る。
pub fn save_plugins_config(plugins: &PluginsConfig) -> Result<(), String> {
    let path = config_path();
    ensure_default();
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = rewrite_plugins_section(&raw, plugins);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, updated).map_err(|e| format!("config.toml を書けません: {e}"))
}

/// `[keybindings]` 区画だけを書き戻す。GUI のキーバインド編集がここを通る。
///
/// **他のセクションと手書きのコメントは 1 行も触らない** — config.toml は
/// ユーザーの持ち物で、アプリが丸ごと書き直して良い場所ではない。
/// (`[plugins]` の書き戻しと同じ作法。新しい保存経路は増やさない)
pub fn save_keybindings(overrides: &HashMap<String, String>) -> Result<(), String> {
    let path = config_path();
    ensure_default();
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = rewrite_keybindings_section(&raw, overrides);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, updated).map_err(|e| format!("config.toml を書けません: {e}"))
}

/// 既存の `[keybindings]` 区画を取り除き、末尾に現在の内容を書き足す。
/// 上書きが 1 つも無ければ区画ごと消える (空の見出しを残さない)。
fn rewrite_keybindings_section(raw: &str, overrides: &HashMap<String, String>) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skipping = false;

    for line in raw.lines() {
        let t = line.trim();
        // 見出しかどうか。`# [keybindings]` のようなコメント行は見出しではない
        // (既定テンプレートが持っているので、誤認すると以降が丸ごと消える)。
        let is_header = t.starts_with('[') && t.ends_with(']');
        if is_header {
            let name = t.trim_start_matches('[').trim_end_matches(']');
            let name = name.trim_start_matches('[').trim_end_matches(']');
            skipping = name == "keybindings";
        }
        if !skipping {
            out.push(line);
        }
    }

    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    let mut text = out.join("\n");

    if !overrides.is_empty() {
        // HashMap の順序は不定なので、書くたびに差分が出ないよう名前で並べる
        let mut names: Vec<&String> = overrides.keys().collect();
        names.sort();
        let mut block = String::from("[keybindings]\n");
        for n in names {
            block.push_str(&format!(
                "{} = {}\n",
                n,
                toml::Value::String(overrides[n].clone())
            ));
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(block.trim_end());
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// 既存の `[plugins]` / `[plugins.settings.*]` 区画を取り除き、
/// 末尾に現在の内容を書き足した文字列を返す。
fn rewrite_plugins_section(raw: &str, plugins: &PluginsConfig) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skipping = false;

    for line in raw.lines() {
        let t = line.trim();
        // セクション見出しかどうか (コメント行は見出しではない)
        let is_header = t.starts_with('[') && t.ends_with(']');
        if is_header {
            let name = t.trim_start_matches('[').trim_end_matches(']');
            let name = name.trim_start_matches('[').trim_end_matches(']');
            skipping = name == "plugins" || name.starts_with("plugins.");
        }
        if !skipping {
            out.push(line);
        }
    }

    // 末尾の空行を整理してから追記する
    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    let mut text = out.join("\n");

    let block = render_plugins_section(plugins);
    if !block.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&block);
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// `[plugins]` 区画の本文を組み立てる (空設定なら空文字列)。
fn render_plugins_section(plugins: &PluginsConfig) -> String {
    let has_settings = plugins.settings.values().any(|kv| !kv.is_empty());
    if plugins.disabled.is_empty() && !has_settings {
        return String::new();
    }

    let quote = |s: &str| toml::Value::String(s.to_string()).to_string();
    // TOML の裸キーとして安全な形か。マニフェスト由来の名前は検証済みだが、
    // 手書き config.toml 由来の値 (空白や . 入り) を裸で書き戻すと
    // 不正な TOML になり、次回起動でファイル全体が読めなくなる。
    let bare_ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    let key_str = |s: &str| {
        if bare_ok(s) {
            s.to_string()
        } else {
            quote(s)
        }
    };

    let mut s = String::from("[plugins]\n");
    let items: Vec<String> = plugins.disabled.iter().map(|d| quote(d)).collect();
    s.push_str(&format!("disabled = [{}]\n", items.join(", ")));

    // HashMap の順序は不定なので、書くたびに差分が出ないよう名前で並べる
    let mut names: Vec<&String> = plugins.settings.keys().collect();
    names.sort();
    for name in names {
        let kv = &plugins.settings[name];
        if kv.is_empty() {
            continue;
        }
        s.push_str(&format!("\n[plugins.settings.{}]\n", key_str(name)));
        let mut keys: Vec<&String> = kv.keys().collect();
        keys.sort();
        for k in keys {
            s.push_str(&format!("{} = {}\n", key_str(k), quote(&kv[k])));
        }
    }
    s
}

/// Persist the current UI choices (theme / approval mode / pet) without
/// touching the user's hand-written config.toml.
///
/// theme / approval_mode / show_pet はプロジェクト overlay で上書きされ得るため、
/// セッション中の値ではなく `global_*` の控えを書く。UI から変更したときは
/// 呼び出し側が控えも更新するので、本当の変更はちゃんと永続化される。
pub fn save_state(cfg: &Config) {
    save_state_to_dir(&zaivern_dir(), cfg);
}

/// `save_state()` の実体。テストから一時ディレクトリを差し込めるよう分離している。
fn save_state_to_dir(dir: &Path, cfg: &Config) {
    let st = UiState {
        theme: Some(cfg.global_theme.clone()),
        approval_mode: Some(cfg.global_approval_mode.clone()),
        show_pet: Some(cfg.global_show_pet),
        word_wrap: Some(cfg.global_word_wrap),
        show_whitespace: Some(cfg.global_show_whitespace),
        ui_zoom: Some(crate::zoom::clamp(cfg.global_ui_zoom)),
        text_scale: Some(crate::zoom::clamp(cfg.global_text_scale)),
        last_seen_version: Some(cfg.last_seen_version.clone()),
        minimap: Some(cfg.global_minimap),
        breadcrumbs: Some(cfg.global_breadcrumbs),
        diff_view: Some(cfg.diff_view.clone()),
        git_blame: Some(cfg.global_git_blame),
        confirm_drag_and_drop: Some(cfg.confirm_drag_and_drop),
        enable_trash: Some(cfg.enable_trash),
        tab_switch_mru: Some(cfg.tab_switch_mru),
        preview_tabs: Some(cfg.preview_tabs),
        pet_image: cfg.pet_image.clone(),
        pet_x: cfg.pet_x,
        pet_y: cfg.pet_y,
        pet_variant: Some(cfg.pet_variant.clone()),
        pet_scale: Some(cfg.pet_scale),
        pet_free_roam: Some(cfg.pet_free_roam),
        pet_sleep: Some(cfg.pet_sleep),
        pet_sounds: Some(cfg.pet_sounds),
        pet_bubbles: Some(cfg.pet_bubbles),
        pet_auto_yes: Some(cfg.pet_auto_yes),
        pet_approve_keys: Some(cfg.pet_approve_keys.clone()),
        pet_deny_keys: Some(cfg.pet_deny_keys.clone()),
        voice_engine: Some(cfg.voice_engine.clone()),
        voice_target: Some(cfg.voice_target.clone()),
        voice_lang: Some(cfg.voice_lang.clone()),
        voice_command: Some(cfg.voice_command.clone()),
        voice_keyword: Some(cfg.voice_keyword.clone()),
        ssh_tunnel_host: Some(cfg.ssh_tunnel_host.clone()),
        super_agent_command: Some(cfg.super_agent.command.clone()),
        super_agent_session_title: Some(cfg.super_agent.session_title.clone()),
        super_agent_enabled: Some(cfg.super_agent.enabled),
        super_agent_timeout_secs: Some(cfg.super_agent.timeout_secs),
        failover_enabled: Some(cfg.failover.enabled),
        quick_launch: cfg.quick_launch.clone(),
        palette_recent: Some(cfg.palette_recent.clone()),
    };
    if let Ok(s) = toml::to_string_pretty(&st) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(dir.join("state.toml"), s);
    }
}

// ═════════════════════════════════════════════════════════════════════════
//  設定 GUI の土台 — 一覧 / 検索 / @modified / 既定へ戻す / 書き戻し
// ═════════════════════════════════════════════════════════════════════════
//
// **説明文は 1 か所にしか無い**: [`DEFAULT_CONFIG`] のコメント。
// GUI はそこを実行時に読み取る ([`setting_doc`])。ここに同じ文言を
// 書き写すと、片方だけ直した瞬間に嘘になるので絶対に増やさない。
//
// 書き戻しは `rewrite_keybindings_section` / `rewrite_plugins_section` と
// 同じ作法 — **触るのは対象の行だけ**。手書きのコメントも、GUI が知らない
// 設定も、セクションも 1 行も消さない。

/// 設定 1 項目の型。GUI がどのウィジェットを出すかを決める。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SettingKind {
    Bool,
    /// 整数 (件数・ミリ秒・バイト)。範囲は GUI が丸めるためのもの。
    Int {
        min: i64,
        max: i64,
    },
    Float {
        min: f32,
        max: f32,
    },
    /// 自由入力 (パス・URL・言語タグ)。
    Text,
    /// 決められた候補から選ぶ。
    Choice(&'static [&'static str]),
}

/// 設定 1 項目の値。`Config` のフィールド型の違いをここへ畳む。
#[derive(Clone, PartialEq, Debug)]
pub enum SettingValue {
    Bool(bool),
    Int(i64),
    Float(f32),
    Text(String),
}

impl SettingValue {
    /// config.toml へ書くリテラル表現。
    pub fn to_toml(&self) -> String {
        match self {
            Self::Bool(b) => b.to_string(),
            Self::Int(i) => i.to_string(),
            // 整数に見える浮動小数も TOML の float として書く
            // (`15` と書くと次回 f32 として読めずファイル全体が落ちる)
            Self::Float(f) => {
                let s = format!("{f}");
                if s.contains(['.', 'e', 'E']) {
                    s
                } else {
                    format!("{s}.0")
                }
            }
            Self::Text(s) => toml::Value::String(s.clone()).to_string(),
        }
    }
}

/// 設定 1 項目の定義。**説明文は持たない** (テンプレートから引く)。
#[derive(Clone, Copy)]
pub struct SettingDef {
    /// config.toml のキー名。`Config` のフィールド名と一致させる。
    pub key: &'static str,
    /// 画面上のグループ見出し。
    pub group: &'static str,
    /// 画面上の項目名。
    pub label: &'static str,
    pub kind: SettingKind,
}

const APPROVAL_MODES: &[&str] = &["ask", "auto", "agent"];
const COST_LIMIT_ACTIONS: &[&str] = &["notify", "stop"];

/// コスト上限の入力欄が受け付ける最大値。
///
/// **これは価格ではなく入力ウィジェットの範囲**。`DragValue` に上限が無いと
/// 指が滑っただけで桁が飛び、上限が事実上消える。通貨は設定側 (`[pricing]`)
/// なので、円建てのように 1 単位が小さい通貨でも足りる桁にしてある。
const COST_LIMIT_MAX: f32 = 10_000_000.0;
const DIFF_VIEWS: &[&str] = &["side_by_side", "inline"];
const BLAME_MODES: &[&str] = &[
    BlameMode::Off.config_str(),
    BlameMode::Current.config_str(),
    BlameMode::All.config_str(),
];
const VOICE_ENGINES: &[&str] = &["auto", "mac", "powershell", "browser", "command", "off"];
const VOICE_TARGETS: &[&str] = &["active", "broadcast"];

/// 設定 GUI の「表示言語」に出す候補。**同梱ぶんだけ**を並べる。
///
/// `~/.zaivern/locales/fr.json` のようなコミュニティ言語はここに出ない
/// (`&'static` の一覧に実行時の走査結果は入らない) が、🌐 の言語ピッカーと
/// `config.toml` の直接編集からは選べる。番人は
/// `config::tests::表示言語の候補は同梱言語と一致する`。
const UI_LANGUAGES: &[&str] = &[
    crate::locale::AUTO,
    "en",
    "ja",
    "zh-CN",
    "ko",
    "pt-BR",
    "es",
];

/// GUI に出す設定の一覧。
///
/// ここに無い設定 (エージェントプリセット / キーバインド / プラグイン /
/// 承認ポリシー) は、それぞれ専用の画面を持っているので二重に作らない。
/// 表現しきれないものは「config.toml を直接編集」のボタンから触る。
pub fn setting_defs() -> &'static [SettingDef] {
    use SettingKind::*;
    const G_LOOK: &str = "外観";
    const G_EDITOR: &str = "エディタ";
    const G_FILES: &str = "ファイル";
    const G_SAVE: &str = "保存";
    const G_RESTORE: &str = "復元";
    const G_AGENT: &str = "エージェント";
    const G_COST: &str = "コスト";
    const G_LINK: &str = "音声・連携";
    &[
        SettingDef {
            key: "ui_language",
            group: G_LOOK,
            label: "表示言語",
            kind: Choice(UI_LANGUAGES),
        },
        SettingDef {
            key: "theme",
            group: G_LOOK,
            label: "テーマ",
            kind: Text,
        },
        SettingDef {
            key: "editor_font_size",
            group: G_LOOK,
            label: "エディタの文字サイズ",
            kind: Float {
                min: 6.0,
                max: 48.0,
            },
        },
        SettingDef {
            key: "terminal_font_size",
            group: G_LOOK,
            label: "ターミナルの文字サイズ",
            kind: Float {
                min: 6.0,
                max: 48.0,
            },
        },
        SettingDef {
            key: "ui_zoom",
            group: G_LOOK,
            label: "画面全体のズーム",
            kind: Float { min: 0.5, max: 3.0 },
        },
        SettingDef {
            key: "text_scale",
            group: G_LOOK,
            label: "文字サイズの倍率 (レイアウトは変えない)",
            kind: Float { min: 0.5, max: 3.0 },
        },
        SettingDef {
            key: "show_pet",
            group: G_LOOK,
            label: "デスクトップペットを出す",
            kind: Bool,
        },
        SettingDef {
            key: "word_wrap",
            group: G_EDITOR,
            label: "本文を折り返す",
            kind: Bool,
        },
        SettingDef {
            key: "show_whitespace",
            group: G_EDITOR,
            label: "空白文字を可視化する",
            kind: Bool,
        },
        SettingDef {
            key: "minimap",
            group: G_EDITOR,
            label: "ミニマップを出す",
            kind: Bool,
        },
        SettingDef {
            key: "breadcrumbs",
            group: G_EDITOR,
            label: "ブレッドクラムを出す",
            kind: Bool,
        },
        SettingDef {
            key: "bracket_colorization",
            group: G_EDITOR,
            label: "括弧を深さで色分けする",
            kind: Bool,
        },
        SettingDef {
            key: "inline_diagnostics",
            group: G_EDITOR,
            label: "行末に診断を出す",
            kind: Bool,
        },
        SettingDef {
            key: "inlay_hints",
            group: G_EDITOR,
            label: "インレイヒント (型・引数名) を出す",
            kind: Bool,
        },
        SettingDef {
            key: "lsp_highlight_occurrences",
            group: G_EDITOR,
            label: "同一シンボルをハイライトする",
            kind: Bool,
        },
        SettingDef {
            key: "git_blame",
            group: G_EDITOR,
            label: "ガターに git blame を出す (off / カーソル行 / 全行)",
            kind: Choice(BLAME_MODES),
        },
        SettingDef {
            key: "detect_indentation",
            group: G_EDITOR,
            label: "インデントを本文から推定する",
            kind: Bool,
        },
        SettingDef {
            key: "tab_size",
            group: G_EDITOR,
            label: "インデント幅 (桁)",
            kind: Int { min: 1, max: 16 },
        },
        SettingDef {
            key: "insert_spaces",
            group: G_EDITOR,
            label: "インデントにスペースを使う",
            kind: Bool,
        },
        SettingDef {
            key: "diff_view",
            group: G_EDITOR,
            label: "差分ビューの既定",
            kind: Choice(DIFF_VIEWS),
        },
        SettingDef {
            key: "whichkey_delay_ms",
            group: G_EDITOR,
            label: "続きの打鍵一覧を出すまでの待ち (ms)",
            kind: Int {
                min: 0,
                max: crate::whichkey::MAX_FIRST_DELAY_MS as i64,
            },
        },
        SettingDef {
            key: "undo_merge_ms",
            group: G_EDITOR,
            label: "取り消しをまとめる時間 (ms)",
            kind: Int {
                min: 0,
                max: 10_000,
            },
        },
        SettingDef {
            key: "undo_max_steps",
            group: G_EDITOR,
            label: "取り消しの最大段数",
            kind: Int {
                min: 1,
                max: 100_000,
            },
        },
        SettingDef {
            key: "undo_max_bytes",
            group: G_EDITOR,
            label: "取り消しの合計バイト上限",
            kind: Int {
                min: 1024,
                max: 1 << 30,
            },
        },
        SettingDef {
            key: "show_hidden_files",
            group: G_FILES,
            label: "隠しファイルを表示する",
            kind: Bool,
        },
        SettingDef {
            key: "respect_gitignore",
            group: G_FILES,
            label: ".gitignore を尊重する",
            kind: Bool,
        },
        SettingDef {
            key: "dim_ignored_files",
            group: G_FILES,
            label: "無視されたファイルを薄く出す (既定: VS Code と同じ)",
            kind: Bool,
        },
        SettingDef {
            key: "index_max_files",
            group: G_FILES,
            label: "ファイル索引の上限件数",
            kind: Int {
                min: 100,
                max: 5_000_000,
            },
        },
        SettingDef {
            key: "index_max_depth",
            group: G_FILES,
            label: "ファイル索引の最大深さ",
            kind: Int { min: 1, max: 128 },
        },
        SettingDef {
            key: "tree_dir_page",
            group: G_FILES,
            label: "ツリーの 1 回の表示件数",
            kind: Int {
                min: 10,
                max: 100_000,
            },
        },
        SettingDef {
            key: "confirm_drag_and_drop",
            group: G_FILES,
            label: "ドラッグ移動を確認する",
            kind: Bool,
        },
        SettingDef {
            key: "enable_trash",
            group: G_FILES,
            label: "削除をゴミ箱へ送る",
            kind: Bool,
        },
        SettingDef {
            key: "preview_tabs",
            group: G_FILES,
            label: "プレビュータブを使う",
            kind: Bool,
        },
        SettingDef {
            key: "tab_switch_mru",
            group: G_FILES,
            label: "タブ切替を最近使った順にする",
            kind: Bool,
        },
        SettingDef {
            key: "trim_trailing_whitespace",
            group: G_SAVE,
            label: "保存時に行末の空白を落とす",
            kind: Bool,
        },
        SettingDef {
            key: "trim_final_newlines",
            group: G_SAVE,
            label: "保存時に末尾の空行を落とす",
            kind: Bool,
        },
        SettingDef {
            key: "insert_final_newline",
            group: G_SAVE,
            label: "保存時に最終行へ改行を入れる",
            kind: Bool,
        },
        SettingDef {
            key: "format_on_save",
            group: G_SAVE,
            label: "保存時に整形する",
            kind: Bool,
        },
        SettingDef {
            key: "hot_exit",
            group: G_RESTORE,
            label: "未保存の本文を復元する (Hot Exit)",
            kind: Bool,
        },
        SettingDef {
            key: "hot_exit_max_kb",
            group: G_RESTORE,
            label: "退避の上限 (KiB / バッファ)",
            kind: Int {
                min: 0,
                max: 1 << 20,
            },
        },
        SettingDef {
            key: "hot_exit_interval_ms",
            group: G_RESTORE,
            label: "退避の最短間隔 (ms)",
            kind: Int {
                min: 0,
                max: 600_000,
            },
        },
        SettingDef {
            key: "local_history",
            group: G_RESTORE,
            label: "ローカルヒストリを記録する",
            kind: Bool,
        },
        SettingDef {
            key: "local_history_days",
            group: G_RESTORE,
            label: "ローカルヒストリの保持日数 (活動時間)",
            kind: Int { min: 1, max: 365 },
        },
        SettingDef {
            key: "local_history_gap_hours",
            group: G_RESTORE,
            label: "活動していないと見なす空白 (時間)",
            kind: Int { min: 1, max: 720 },
        },
        SettingDef {
            key: "restore_agents",
            group: G_RESTORE,
            label: "前回のエージェントを復元する",
            kind: Bool,
        },
        SettingDef {
            key: "auto_name_sessions",
            group: G_AGENT,
            label: "ターン終了時にセッション名を自動生成する",
            kind: Bool,
        },
        SettingDef {
            key: "approval_mode",
            group: G_AGENT,
            label: "既定の権限モード",
            kind: Choice(APPROVAL_MODES),
        },
        SettingDef {
            key: "cost_limit_session",
            group: G_COST,
            label: "このセッションの上限 (0 = 無制限)",
            kind: Float {
                min: 0.0,
                max: COST_LIMIT_MAX,
            },
        },
        SettingDef {
            key: "cost_limit_daily",
            group: G_COST,
            label: "1 日 (UTC) の上限 (0 = 無制限)",
            kind: Float {
                min: 0.0,
                max: COST_LIMIT_MAX,
            },
        },
        SettingDef {
            key: "cost_warn_ratio",
            group: G_COST,
            label: "警告を出す割合 (0.0〜1.0)",
            kind: Float { min: 0.0, max: 1.0 },
        },
        SettingDef {
            key: "cost_limit_action",
            group: G_COST,
            label: "上限に達したときの動作",
            kind: Choice(COST_LIMIT_ACTIONS),
        },
        SettingDef {
            key: "voice_engine",
            group: G_LINK,
            label: "音声認識エンジン",
            kind: Choice(VOICE_ENGINES),
        },
        SettingDef {
            key: "voice_target",
            group: G_LINK,
            label: "音声入力の届け先",
            kind: Choice(VOICE_TARGETS),
        },
        SettingDef {
            key: "voice_lang",
            group: G_LINK,
            label: "認識言語 (BCP-47)",
            kind: Text,
        },
        SettingDef {
            key: "webhook_url",
            group: G_LINK,
            label: "通知の Webhook URL",
            kind: Text,
        },
        SettingDef {
            key: "ssh_tunnel_host",
            group: G_LINK,
            label: "SSH リモートの踏み台",
            kind: Text,
        },
    ]
}

/// 現在値を読む。未知のキーは None。
pub fn setting_value(cfg: &Config, key: &str) -> Option<SettingValue> {
    use SettingValue::{Bool as B, Float as F, Int as I, Text as T};
    Some(match key {
        "theme" => T(cfg.theme.clone()),
        "ui_language" => T(cfg.ui_language.clone()),
        "editor_font_size" => F(cfg.editor_font_size),
        "terminal_font_size" => F(cfg.terminal_font_size),
        "ui_zoom" => F(cfg.ui_zoom),
        "text_scale" => F(cfg.text_scale),
        "show_pet" => B(cfg.show_pet),
        "word_wrap" => B(cfg.word_wrap),
        "show_whitespace" => B(cfg.show_whitespace),
        "minimap" => B(cfg.minimap),
        "breadcrumbs" => B(cfg.breadcrumbs),
        "bracket_colorization" => B(cfg.bracket_colorization),
        "inline_diagnostics" => B(cfg.inline_diagnostics),
        "inlay_hints" => B(cfg.inlay_hints),
        "lsp_highlight_occurrences" => B(cfg.lsp_highlight_occurrences),
        "git_blame" => T(cfg.git_blame.config_str().into()),
        "detect_indentation" => B(cfg.detect_indentation),
        "tab_size" => I(cfg.tab_size as i64),
        "insert_spaces" => B(cfg.insert_spaces),
        "diff_view" => T(cfg.diff_view.clone()),
        "undo_merge_ms" => I(cfg.undo_merge_ms as i64),
        "whichkey_delay_ms" => I(cfg.whichkey_delay_ms as i64),
        "undo_max_steps" => I(cfg.undo_max_steps as i64),
        "undo_max_bytes" => I(cfg.undo_max_bytes as i64),
        "show_hidden_files" => B(cfg.show_hidden_files),
        "respect_gitignore" => B(cfg.respect_gitignore),
        "dim_ignored_files" => B(cfg.dim_ignored_files),
        "index_max_files" => I(cfg.index_max_files as i64),
        "index_max_depth" => I(cfg.index_max_depth as i64),
        "tree_dir_page" => I(cfg.tree_dir_page as i64),
        "confirm_drag_and_drop" => B(cfg.confirm_drag_and_drop),
        "enable_trash" => B(cfg.enable_trash),
        "preview_tabs" => B(cfg.preview_tabs),
        "tab_switch_mru" => B(cfg.tab_switch_mru),
        "trim_trailing_whitespace" => B(cfg.trim_trailing_whitespace),
        "trim_final_newlines" => B(cfg.trim_final_newlines),
        "insert_final_newline" => B(cfg.insert_final_newline),
        "format_on_save" => B(cfg.format_on_save),
        "hot_exit" => B(cfg.hot_exit),
        "hot_exit_max_kb" => I(cfg.hot_exit_max_kb as i64),
        "hot_exit_interval_ms" => I(cfg.hot_exit_interval_ms as i64),
        "local_history" => B(cfg.local_history),
        "local_history_days" => I(cfg.local_history_days as i64),
        "local_history_gap_hours" => I(cfg.local_history_gap_hours as i64),
        "restore_agents" => B(cfg.restore_agents),
        "auto_name_sessions" => B(cfg.auto_name_sessions),
        "approval_mode" => T(cfg.approval_mode.clone()),
        "cost_limit_session" => F(cfg.cost_limit_session),
        "cost_limit_daily" => F(cfg.cost_limit_daily),
        "cost_warn_ratio" => F(cfg.cost_warn_ratio),
        "cost_limit_action" => T(cfg.cost_limit_action.clone()),
        "voice_engine" => T(cfg.voice_engine.clone()),
        "voice_target" => T(cfg.voice_target.clone()),
        "voice_lang" => T(cfg.voice_lang.clone()),
        "webhook_url" => T(cfg.webhook_url.clone()),
        "ssh_tunnel_host" => T(cfg.ssh_tunnel_host.clone()),
        // 組み込みに無いキーは機能の設定 (`<module>.<name>`) として引く。
        // 組み込みのキーは点を含まないので取り違えは起こらない。
        _ => return feature_value(cfg, key),
    })
}

/// 値を書き込む。型が合わない / 未知のキーなら false (何もしない)。
///
/// **`global_*` の控えも一緒に更新する** — save_state はそちらを書くので、
/// ここを忘れるとプロジェクト overlay 下で変更が永続化されない。
pub fn set_setting_value(cfg: &mut Config, key: &str, v: &SettingValue) -> bool {
    use SettingValue::{Bool as B, Float as F, Int as I, Text as T};
    // 使う側の取り違えを 1 か所で弾く (型が合わなければ触らない)
    macro_rules! b {
        ($f:expr) => {
            match v {
                B(x) => {
                    $f = *x;
                    true
                }
                _ => false,
            }
        };
    }
    macro_rules! i {
        ($f:expr, $t:ty) => {
            match v {
                I(x) if *x >= 0 => {
                    $f = *x as $t;
                    true
                }
                _ => false,
            }
        };
    }
    macro_rules! f {
        ($f:expr) => {
            match v {
                F(x) => {
                    $f = *x;
                    true
                }
                _ => false,
            }
        };
    }
    macro_rules! t {
        ($f:expr) => {
            match v {
                T(x) => {
                    $f = x.clone();
                    true
                }
                _ => false,
            }
        };
    }
    match key {
        "theme" => {
            let ok = t!(cfg.theme);
            if ok {
                cfg.global_theme = cfg.theme.clone();
            }
            ok
        }
        "ui_language" => t!(cfg.ui_language),
        "editor_font_size" => f!(cfg.editor_font_size),
        "terminal_font_size" => f!(cfg.terminal_font_size),
        "ui_zoom" => {
            let ok = f!(cfg.ui_zoom);
            if ok {
                cfg.global_ui_zoom = cfg.ui_zoom;
            }
            ok
        }
        "text_scale" => {
            let ok = f!(cfg.text_scale);
            if ok {
                cfg.global_text_scale = cfg.text_scale;
            }
            ok
        }
        "show_pet" => {
            let ok = b!(cfg.show_pet);
            if ok {
                cfg.global_show_pet = cfg.show_pet;
            }
            ok
        }
        "word_wrap" => {
            let ok = b!(cfg.word_wrap);
            if ok {
                cfg.global_word_wrap = cfg.word_wrap;
            }
            ok
        }
        "show_whitespace" => {
            let ok = b!(cfg.show_whitespace);
            if ok {
                cfg.global_show_whitespace = cfg.show_whitespace;
            }
            ok
        }
        "minimap" => {
            let ok = b!(cfg.minimap);
            if ok {
                cfg.global_minimap = cfg.minimap;
            }
            ok
        }
        "breadcrumbs" => {
            let ok = b!(cfg.breadcrumbs);
            if ok {
                cfg.global_breadcrumbs = cfg.breadcrumbs;
            }
            ok
        }
        "git_blame" => {
            let ok = match v {
                T(x) => {
                    cfg.git_blame = BlameMode::from_config_str(x);
                    true
                }
                // 旧形式で書かれた設定ファイルから直接来ることがある
                B(x) => {
                    cfg.git_blame = BlameMode::from_flag(*x);
                    true
                }
                _ => false,
            };
            if ok {
                cfg.global_git_blame = cfg.git_blame;
            }
            ok
        }
        "bracket_colorization" => b!(cfg.bracket_colorization),
        "inline_diagnostics" => b!(cfg.inline_diagnostics),
        "inlay_hints" => b!(cfg.inlay_hints),
        "lsp_highlight_occurrences" => b!(cfg.lsp_highlight_occurrences),
        "detect_indentation" => b!(cfg.detect_indentation),
        "tab_size" => i!(cfg.tab_size, usize),
        "insert_spaces" => b!(cfg.insert_spaces),
        "diff_view" => t!(cfg.diff_view),
        "undo_merge_ms" => i!(cfg.undo_merge_ms, u64),
        "whichkey_delay_ms" => i!(cfg.whichkey_delay_ms, u64),
        "undo_max_steps" => i!(cfg.undo_max_steps, usize),
        "undo_max_bytes" => i!(cfg.undo_max_bytes, usize),
        "show_hidden_files" => b!(cfg.show_hidden_files),
        "respect_gitignore" => b!(cfg.respect_gitignore),
        "dim_ignored_files" => b!(cfg.dim_ignored_files),
        "index_max_files" => i!(cfg.index_max_files, usize),
        "index_max_depth" => i!(cfg.index_max_depth, usize),
        "tree_dir_page" => i!(cfg.tree_dir_page, usize),
        "confirm_drag_and_drop" => b!(cfg.confirm_drag_and_drop),
        "enable_trash" => b!(cfg.enable_trash),
        "preview_tabs" => b!(cfg.preview_tabs),
        "tab_switch_mru" => b!(cfg.tab_switch_mru),
        "trim_trailing_whitespace" => b!(cfg.trim_trailing_whitespace),
        "trim_final_newlines" => b!(cfg.trim_final_newlines),
        "insert_final_newline" => b!(cfg.insert_final_newline),
        "format_on_save" => b!(cfg.format_on_save),
        "hot_exit" => b!(cfg.hot_exit),
        "hot_exit_max_kb" => i!(cfg.hot_exit_max_kb, usize),
        "hot_exit_interval_ms" => i!(cfg.hot_exit_interval_ms, u64),
        "local_history" => b!(cfg.local_history),
        "local_history_days" => i!(cfg.local_history_days, u32),
        "local_history_gap_hours" => i!(cfg.local_history_gap_hours, u32),
        "restore_agents" => b!(cfg.restore_agents),
        "auto_name_sessions" => b!(cfg.auto_name_sessions),
        "approval_mode" => {
            let ok = t!(cfg.approval_mode);
            if ok {
                cfg.global_approval_mode = cfg.approval_mode.clone();
            }
            ok
        }
        "cost_limit_session" => f!(cfg.cost_limit_session),
        "cost_limit_daily" => f!(cfg.cost_limit_daily),
        "cost_warn_ratio" => f!(cfg.cost_warn_ratio),
        "cost_limit_action" => t!(cfg.cost_limit_action),
        "voice_engine" => t!(cfg.voice_engine),
        "voice_target" => t!(cfg.voice_target),
        "voice_lang" => t!(cfg.voice_lang),
        "webhook_url" => t!(cfg.webhook_url),
        "ssh_tunnel_host" => t!(cfg.ssh_tunnel_host),
        // 機能が**宣言している**キーだけ機能の設定として書く。
        // 宣言の無いキーは従来どおり false (打ち間違いを黙って通さない)。
        _ => {
            if feature_setting(key).is_none() || !cfg.set_feature(key, v.clone()) {
                return false;
            }
            // 書けたら実行時の旗へも写す (設定画面で切り替えた瞬間に効く)。
            // 個別のキーをここで見分けない — 見分けると設定が増えるたびに
            // この共有ファイルへ追記することになる。
            apply_runtime_flags(cfg);
            true
        }
    }
}

/// 出荷時の値 (`Config::default()`)。「既定へ戻す」と `@modified` の基準。
pub fn setting_default(key: &str) -> Option<SettingValue> {
    // Config::default() は毎回組むと重いので 1 度だけ作って使い回す
    static DEFAULTS: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
    setting_value(DEFAULTS.get_or_init(Config::default), key)
}

/// 既定から変えられているか (VS Code の `@modified` 相当)。
///
/// **既定と同じ値に戻したら false になる** — 「一度触ったから」ではなく
/// 「いま既定と違うか」で判定する (そうでないと戻しても消えない)。
pub fn is_setting_modified(cfg: &Config, key: &str) -> bool {
    match (setting_value(cfg, key), setting_default(key)) {
        (Some(now), Some(def)) => now != def,
        _ => false,
    }
}

/// 検索と `@modified` で絞った表示順の一覧。
///
/// あいまい検索は既存の [`crate::fuzzy`] をそのまま使う (新しいマッチャは
/// 書かない)。キー名・ラベル・グループ・説明文のどれに当たっても拾う。
pub fn settings_rows(cfg: &Config, query: &str, only_modified: bool) -> Vec<&'static SettingDef> {
    // `@modified` はクエリに直接書いても効く (VS Code と同じ書き方)
    let mut modified_only = only_modified;
    let mut q = String::new();
    for tok in query.split_whitespace() {
        if tok.eq_ignore_ascii_case("@modified") {
            modified_only = true;
        } else {
            if !q.is_empty() {
                q.push(' ');
            }
            q.push_str(tok);
        }
    }
    let prepared = crate::fuzzy::PreparedQuery::new(&q);
    let mut hits: Vec<(i32, usize, &'static SettingDef)> = Vec::new();
    // 組み込み + 機能が宣言した設定。**機能側は 1 行も追記しない**のに
    // 検索にも `@modified` にも「既定へ戻す」にも自動で乗る。
    for (i, d) in all_setting_defs().iter().enumerate() {
        if modified_only && !is_setting_modified(cfg, d.key) {
            continue;
        }
        if q.is_empty() {
            hits.push((0, i, d));
            continue;
        }
        // ラベル → キー → グループ → 説明文の順に見て、最初に当たった点を使う
        let doc = setting_doc(d.key);
        let score = [d.label, d.key, d.group, doc.as_str()]
            .iter()
            .filter_map(|t| prepared.score(t))
            .max();
        if let Some(s) = score {
            hits.push((s, i, d));
        }
    }
    // 点が同じなら定義順 (グループの並びが崩れない)
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    hits.into_iter().map(|(_, _, d)| d).collect()
}

// ═════════════════════════════════════════════════════════════════════════
//  機能が自分で宣言する設定 (`[features]`) — 追記ゼロで設定を足す面
// ═════════════════════════════════════════════════════════════════════════
//
// **なぜ要るのか。** 従来は設定を持つ機能を足すたびに、この config.rs の
//   (1) `Config` の構造体   (2) `Config::default()`   (3) `setting_defs()`
//   (4) `setting_value()`   (5) `set_setting_value()`
// へ追記する必要があった。**git が衝突を作るのは「2 つのブランチが同じ
// ファイルの近い行を触った」時だけ**なので、この形では同時に設定を足した
// 2 本のブランチは必ず衝突する (which-key と local_history が実際に 3 ハンク)。
//
// **解き方は追記そのものを無くすこと。** 機能側は
// `src/features/<名前>.rs` に [`crate::feature::Setting`] を並べるだけで、
// 値は [`Config::extra`] へ文字列キーで入る。ここにある関数は全て
// [`crate::feature::REGISTRY`] を**走査して**組み立てるので、
// **config.rs を 1 バイトも触らずに設定が増える**。

/// 機能の設定の入力欄が受け付ける範囲。
///
/// **これは検証ではなくウィジェットの範囲**。[`crate::feature::Setting`] は
/// 範囲を宣言しないので、UI が勝手に狭い範囲を発明してはいけない
/// (機能側が意図した値を入れられなくなる)。桁飛びを防ぐのは
/// `DragValue` の刻みであって、ここではない。
const FEATURE_INT_MIN: i64 = i64::MIN;
const FEATURE_INT_MAX: i64 = i64::MAX;
const FEATURE_FLOAT_MIN: f32 = f32::MIN;
const FEATURE_FLOAT_MAX: f32 = f32::MAX;

/// 機能の設定がまとまる設定画面のグループ見出し。
const G_FEATURE: &str = "機能";

/// `feature::REGISTRY` の全 [`crate::feature::Setting`] をキーで引く表。
///
/// 1 度だけ組んで使い回す (レジストリは実行中に変わらない)。
fn feature_settings() -> &'static HashMap<&'static str, &'static crate::feature::Setting> {
    static MAP: std::sync::OnceLock<HashMap<&'static str, &'static crate::feature::Setting>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(|| {
        let mut m: HashMap<&'static str, &'static crate::feature::Setting> = HashMap::new();
        for f in crate::feature::REGISTRY {
            for s in f.settings {
                // 同じキーが 2 つあったら先に登録されたほうを残す。
                // 番人テスト `機能の設定キーはモジュール接頭辞付きで一意` が
                // 静的に禁止しているので、ここへ来るのは異常な状態。
                m.entry(s.key).or_insert(s);
            }
        }
        m
    })
}

/// 機能の設定の**宣言**を引く。宣言の無いキーは `None`。
///
/// 宣言が無い = この版が知らない設定。値は [`Config::extra`] に残り続けるが
/// 画面には出ないし、型付きアクセサは既定へ落ちる (**panic しない**)。
pub fn feature_setting(key: &str) -> Option<&'static crate::feature::Setting> {
    feature_settings().get(key).copied()
}

/// 宣言された既定値を、設定画面が使う [`SettingValue`] へ写す。
///
/// `Float` は f64 → f32 に落ちる (画面のウィジェットが f32 のため)。
/// **値を読むだけなら [`Config::feature_f64`] を使うこと** — そちらは
/// 落とさずに f64 のまま返す。
fn feature_default_value(s: &crate::feature::Setting) -> SettingValue {
    match s.default {
        crate::feature::SettingValue::Bool(b) => SettingValue::Bool(b),
        crate::feature::SettingValue::Int(i) => SettingValue::Int(i),
        crate::feature::SettingValue::Float(f) => SettingValue::Float(f as f32),
        crate::feature::SettingValue::Text(t) => SettingValue::Text(t.to_string()),
    }
}

/// 宣言された既定値の型から、設定画面が出すウィジェットの種類を決める。
fn feature_kind(s: &crate::feature::Setting) -> SettingKind {
    match s.default {
        crate::feature::SettingValue::Bool(_) => SettingKind::Bool,
        crate::feature::SettingValue::Int(_) => SettingKind::Int {
            min: FEATURE_INT_MIN,
            max: FEATURE_INT_MAX,
        },
        crate::feature::SettingValue::Float(_) => SettingKind::Float {
            min: FEATURE_FLOAT_MIN,
            max: FEATURE_FLOAT_MAX,
        },
        crate::feature::SettingValue::Text(_) => SettingKind::Text,
    }
}

/// [`SettingValue`] を config.toml へ入る [`toml::Value`] へ写す。
fn feature_toml_value(v: &SettingValue) -> toml::Value {
    match v {
        SettingValue::Bool(b) => toml::Value::Boolean(*b),
        SettingValue::Int(i) => toml::Value::Integer(*i),
        SettingValue::Float(f) => toml::Value::Float(*f as f64),
        SettingValue::Text(s) => toml::Value::String(s.clone()),
    }
}

/// 登録済みの機能が宣言した設定を [`SettingDef`] の形で返す。
///
/// [`crate::feature::REGISTRY`] の並び順 = 機能の登録順 → 宣言順。
/// 1 度だけ組んで `'static` として貸すので、設定画面は組み込みの設定と
/// **同じ型で**扱える (専用の描画コードが要らない)。
pub fn feature_setting_defs() -> &'static [SettingDef] {
    static DEFS: std::sync::OnceLock<Vec<SettingDef>> = std::sync::OnceLock::new();
    DEFS.get_or_init(|| {
        let mut v: Vec<SettingDef> = Vec::new();
        for f in crate::feature::REGISTRY {
            for s in f.settings {
                v.push(SettingDef {
                    key: s.key,
                    group: G_FEATURE,
                    label: s.label,
                    kind: feature_kind(s),
                });
            }
        }
        v
    })
    .as_slice()
}

/// 組み込みの設定 ([`setting_defs`]) + 機能が宣言した設定
/// ([`feature_setting_defs`])。
///
/// 設定画面が見るのはこちら。**機能の設定が「自動で出る」のはこの 1 行のため**で、
/// 機能を足す側は `setting_defs()` にも `settings_rows()` にも触らない。
pub fn all_setting_defs() -> &'static [SettingDef] {
    static ALL: std::sync::OnceLock<Vec<SettingDef>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        let mut v = setting_defs().to_vec();
        v.extend_from_slice(feature_setting_defs());
        v
    })
    .as_slice()
}

/// 設定画面へ機能の設定を出すための**純粋なデータ** 1 行分。
///
/// 描画は `app.rs` の担当 — ここが返すのは値だけで、egui には触らない。
#[derive(Clone, Debug, PartialEq)]
pub struct FeatureSettingRow {
    /// `"<module>.<name>"` の安定キー。config.toml の `[features]` にも載る。
    pub key: &'static str,
    /// 画面上の項目名 (**日本語の原文**。表示時に `tr` を通すこと)。
    pub label: &'static str,
    /// 補足説明。空のことがある (機能側が省ける宣言なので)。
    pub help: &'static str,
    /// いまの値。値が無い / 型が違うときは既定と同じ値になる。
    pub value: SettingValue,
    /// 出荷時の値。「既定へ戻す」と変更マーカーの基準。
    pub default: SettingValue,
}

impl FeatureSettingRow {
    /// 既定から変えられているか (VS Code の `@modified` 相当)。
    ///
    /// **既定と同じ値に戻したら false**。「一度触ったから」ではない。
    // 設定画面の共通経路は `is_setting_modified` を通るので、こちらは
    // 専用パネルを作るとき用。現在はテストからのみ参照。
    #[allow(dead_code)]
    pub fn is_modified(&self) -> bool {
        self.value != self.default
    }
}

/// 登録済みの機能が宣言した設定を `(key, label, help, 現在値, 既定値)` で返す。
///
/// **機能の設定は既に設定画面 (⚙) に出ている** — [`all_setting_defs`] が
/// 組み込みの設定と同じ形で並べるので、`app.rs` に描画コードは要らない。
/// こちらは「機能の設定だけを別のパネルに出したい」ときの入口で、
/// 列幅は [`settings_columns`] が可用幅から決めるので値だけを返す。
/// 並びはレジストリの登録順 (= 宣言順) で、**使用頻度で並べ替えない**。
///
/// 現在は `app.rs` に呼び出し口が無く、テストからのみ参照している。
#[allow(dead_code)]
pub fn feature_setting_rows(cfg: &Config) -> Vec<FeatureSettingRow> {
    let mut out = Vec::new();
    for f in crate::feature::REGISTRY {
        for s in f.settings {
            let default = feature_default_value(s);
            let value = feature_value(cfg, s.key).unwrap_or_else(|| default.clone());
            out.push(FeatureSettingRow {
                key: s.key,
                label: s.label,
                help: s.help,
                value,
                default,
            });
        }
    }
    out
}

/// 機能の設定の現在値を [`SettingValue`] で読む。宣言が無ければ `None`。
///
/// 型は**宣言が決める** — 保存されている値の型ではない。設定ファイルに
/// 型違いが書かれていても、画面には宣言どおりのウィジェットが出る。
fn feature_value(cfg: &Config, key: &str) -> Option<SettingValue> {
    let s = feature_setting(key)?;
    Some(match s.default {
        crate::feature::SettingValue::Bool(_) => SettingValue::Bool(cfg.feature_bool(key)),
        crate::feature::SettingValue::Int(_) => SettingValue::Int(cfg.feature_i64(key)),
        crate::feature::SettingValue::Float(_) => SettingValue::Float(cfg.feature_f64(key) as f32),
        crate::feature::SettingValue::Text(_) => SettingValue::Text(cfg.feature_str(key)),
    })
}

// ── 値の解決 (純粋関数) ─────────────────────────────────────────────
//
// レジストリを引く部分と値を決める部分を分けてある。**値の決め方だけを
// 表で検査できる**ようにするためで、レジストリが空でも
// 「値なし / 型違い / 宣言なし」の全組み合わせをテストできる。

/// 保存値と宣言から bool を決める。**どの組み合わせでも panic しない**。
///
/// 保存値 (型が合うとき) → 宣言された既定 → 型の既定 (`false`) の順。
fn value_bool(stored: Option<&toml::Value>, decl: Option<&crate::feature::Setting>) -> bool {
    if let Some(toml::Value::Boolean(b)) = stored {
        return *b;
    }
    match decl.map(|s| s.default) {
        Some(crate::feature::SettingValue::Bool(b)) => b,
        _ => false,
    }
}

/// 保存値と宣言から整数を決める。型の既定は `0`。
fn value_i64(stored: Option<&toml::Value>, decl: Option<&crate::feature::Setting>) -> i64 {
    if let Some(toml::Value::Integer(i)) = stored {
        return *i;
    }
    match decl.map(|s| s.default) {
        Some(crate::feature::SettingValue::Int(i)) => i,
        _ => 0,
    }
}

/// 保存値と宣言から実数を決める。型の既定は `0.0`。
///
/// **整数リテラルも受ける** — TOML は `0.5` を float、`1` を integer にする
/// ので、手書きで `"mymod.ratio" = 1` と書いた瞬間に既定へ落ちると
/// 「書いたのに効かない」になる。
fn value_f64(stored: Option<&toml::Value>, decl: Option<&crate::feature::Setting>) -> f64 {
    match stored {
        Some(toml::Value::Float(f)) => return *f,
        Some(toml::Value::Integer(i)) => return *i as f64,
        _ => {}
    }
    match decl.map(|s| s.default) {
        Some(crate::feature::SettingValue::Float(f)) => f,
        Some(crate::feature::SettingValue::Int(i)) => i as f64,
        _ => 0.0,
    }
}

/// 保存値と宣言から文字列を決める。型の既定は空文字。
fn value_str(stored: Option<&toml::Value>, decl: Option<&crate::feature::Setting>) -> String {
    if let Some(toml::Value::String(s)) = stored {
        return s.clone();
    }
    match decl.map(|s| s.default) {
        Some(crate::feature::SettingValue::Text(t)) => t.to_string(),
        _ => String::new(),
    }
}

/// 書こうとしている値が宣言された型と一致するか。
fn value_matches_decl(v: &SettingValue, decl: &crate::feature::Setting) -> bool {
    matches!(
        (v, decl.default),
        (SettingValue::Bool(_), crate::feature::SettingValue::Bool(_))
            | (SettingValue::Int(_), crate::feature::SettingValue::Int(_))
            | (
                SettingValue::Float(_),
                crate::feature::SettingValue::Float(_)
            )
            | (SettingValue::Text(_), crate::feature::SettingValue::Text(_))
    )
}

impl Config {
    /// 機能の設定を bool で読む。
    ///
    /// **値が無い / 型が違う / 宣言が無い、のどれでも panic しない。**
    /// 保存値 → 宣言された既定 → 型の既定 (`false`) の順に落ちる。
    /// 落ちる先を宣言に置くのが要点で、設定ファイルを手で壊されても
    /// 機能は「出荷時の挙動」で動き続ける。
    pub fn feature_bool(&self, key: &str) -> bool {
        value_bool(self.extra.get(key), feature_setting(key))
    }

    /// 機能の設定を整数で読む。落ち方は [`Config::feature_bool`] と同じ
    /// (型の既定は `0`)。
    pub fn feature_i64(&self, key: &str) -> i64 {
        value_i64(self.extra.get(key), feature_setting(key))
    }

    /// 機能の設定を実数で読む。落ち方は [`Config::feature_bool`] と同じ
    /// (型の既定は `0.0`)。整数リテラルも受ける。
    pub fn feature_f64(&self, key: &str) -> f64 {
        value_f64(self.extra.get(key), feature_setting(key))
    }

    /// 機能の設定を文字列で読む。落ち方は [`Config::feature_bool`] と同じ
    /// (型の既定は空文字)。
    pub fn feature_str(&self, key: &str) -> String {
        value_str(self.extra.get(key), feature_setting(key))
    }

    /// 機能の設定を書く。書けたら `true`。
    ///
    /// * 宣言があるキーは**宣言された型と一致するときだけ**書く
    ///   (型違いを入れると読み出しが既定へ落ちて「設定したのに効かない」になる)。
    /// * 宣言の無いキーはそのまま受け入れる — 新しい版が足した設定を
    ///   古い版が握り潰さないため。
    ///
    /// **config.toml への書き戻しは別**。永続化するなら
    /// [`save_settings`] にこのキーと [`SettingValue::to_toml`] を渡す
    /// (点を含むキーは自動で `[features]` 区画へ入る)。
    pub fn set_feature(&mut self, key: &str, v: SettingValue) -> bool {
        if let Some(s) = feature_setting(key) {
            if !value_matches_decl(&v, s) {
                return false;
            }
        }
        self.extra.insert(key.to_string(), feature_toml_value(&v));
        true
    }
}

// ── 説明文は DEFAULT_CONFIG のコメントが唯一の出どころ ──────────────

/// キー → 説明文。[`DEFAULT_CONFIG`] のコメントから 1 度だけ組む。
pub fn setting_doc(key: &str) -> String {
    // 機能の設定はテンプレートに書きようが無い (機能側のファイルにしか
    // 存在しない) ので、宣言の `help` が唯一の出どころ。
    if let Some(s) = feature_setting(key) {
        return s.help.to_string();
    }
    static DOCS: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
    DOCS.get_or_init(|| template_docs(DEFAULT_CONFIG))
        .get(key)
        .cloned()
        .unwrap_or_default()
}

/// テンプレートの「キー → 直前のコメント」表を作る。
///
/// テンプレートは 2 通りの書き方が混ざっている。両方を拾う:
/// 1. 説明のコメント塊が行の上に積まれている (`theme` など)
/// 2. 1 つの塊が複数キーをまとめて説明し、キーごとに
///    `#   undo_max_bytes = …` の形で名指ししている (`undo_*` など)
///
/// 加えて `# trim_final_newlines = false   # 末尾の空行を落とす` のような
/// **行末コメント**も説明として拾う。セクション見出し (`[...]`) を見たら
/// そこで打ち切る — トップレベルの単純値だけが GUI の相手だから。
pub fn template_docs(raw: &str) -> HashMap<String, String> {
    let mut docs: HashMap<String, String> = HashMap::new();
    let mut block: Vec<String> = Vec::new();
    let mut last_block: Vec<String> = Vec::new();
    for line in raw.replace("\r\n", "\n").lines() {
        let t = line.trim_end();
        let tt = t.trim();
        if tt.starts_with('[') && tt.ends_with(']') {
            break; // 以降はセクション = トップレベルではない
        }
        if tt.is_empty() {
            if !block.is_empty() {
                last_block = std::mem::take(&mut block);
            }
            continue;
        }
        // `#` を 1 枚だけ剥がす。**続く空白は残す** — 塊の中の
        // 「名指し行とその続き」をインデントで見分けるのに使う。
        let body = tt.strip_prefix('#');
        let content = body.unwrap_or(tt);
        // `key = 値 # 行末コメント` に割る。値が TOML リテラルとして
        // 読めるときだけ「設定の行」と見なす。そうしないと
        // `# respect_gitignore = false のときは効きません。` のような
        // **説明文**まで設定の行に数えてしまう。
        let assign = split_assign(content.trim()).map(|(k, v)| {
            let (val, inline) = split_inline_comment(v);
            (k, val.trim(), inline)
        });
        let Some((key, val, inline)) = assign.filter(|(_, v, _)| looks_like_toml_value(v)) else {
            if body.is_some() && !is_rule_line(content) {
                block.push(content.to_string());
            }
            continue;
        };
        let _ = val;
        let mut parts: Vec<String> = Vec::new();
        if let Some(c) = inline {
            parts.push(c);
        }
        if let Some(d) = pick_doc(&block, key) {
            parts.push(d);
        } else if let Some(d) = named_doc(&last_block, key) {
            parts.push(d);
        }
        if !block.is_empty() {
            last_block = std::mem::take(&mut block);
        }
        let doc = parts.join("\n");
        if !doc.is_empty() {
            docs.entry(key.to_string()).or_insert(doc);
        }
    }
    docs
}

/// `key = value` に割る (キーは裸のキーだけを認める)。
fn split_assign(s: &str) -> Option<(&str, &str)> {
    let (k, v) = s.split_once('=')?;
    let k = k.trim();
    if k.is_empty()
        || !k
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some((k, v))
}

/// 値が TOML のリテラルとして読めるか (説明文と設定の行を分ける唯一の基準)。
fn looks_like_toml_value(v: &str) -> bool {
    let v = v.trim();
    if v == "true" || v == "false" {
        return true;
    }
    if v.parse::<i64>().is_ok() || v.parse::<f64>().is_ok() {
        return true;
    }
    (v.starts_with('"') && v.len() >= 2 && v.ends_with('"'))
        || (v.starts_with('[') && v.ends_with(']'))
}

/// 値の行の行末コメントを剥がす (文字列リテラルの中の `#` は残す)。
fn split_inline_comment(s: &str) -> (&str, Option<String>) {
    let mut in_str = false;
    let mut prev_escape = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' if !prev_escape => in_str = !in_str,
            '#' if !in_str => {
                let c = s[i + 1..].trim();
                return (&s[..i], (!c.is_empty()).then(|| c.to_string()));
            }
            _ => {}
        }
        prev_escape = c == '\\' && !prev_escape;
    }
    (s, None)
}

/// 罫線・見出し飾りだけの行か (説明として出しても意味が無い)。
fn is_rule_line(c: &str) -> bool {
    let t = c.trim();
    t.is_empty()
        || t.chars()
            .all(|ch| matches!(ch, '─' | '━' | '═' | '-' | '=' | '│' | ' '))
}

/// 塊からこのキーの説明を採る。名指し行があればそれだけ、無ければ塊全体。
fn pick_doc(block: &[String], key: &str) -> Option<String> {
    if block.is_empty() {
        return None;
    }
    named_doc(block, key).or_else(|| {
        // 塊をそのまま出すときは、コメントのインデントを落とす
        // (画面に出るのは文章であって、テンプレートの見た目ではない)
        let joined = block
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join("\n");
        (!joined.trim().is_empty()).then_some(joined)
    })
}

/// 塊の中の `key = 説明` 行 (と、その下のより深いインデントの続き) を採る。
fn named_doc(block: &[String], key: &str) -> Option<String> {
    let at = block.iter().position(|l| match split_assign(l.trim()) {
        Some((k, v)) => k == key && !v.trim().is_empty(),
        None => false,
    })?;
    let head = split_assign(block[at].trim())?.1.trim().to_string();
    let indent = |s: &str| s.len() - s.trim_start().len();
    let base = indent(&block[at]);
    let mut out = vec![head];
    for l in &block[at + 1..] {
        if indent(l) <= base || split_assign(l.trim()).is_some() {
            break;
        }
        out.push(l.trim().to_string());
    }
    Some(out.join(" "))
}

// ── config.toml への書き戻し ────────────────────────────────────────

/// 設定 GUI からの変更を config.toml へ書き戻す。
///
/// **触るのは渡されたキーの行だけ。** 手書きのコメントも、GUI が知らない
/// 設定も、`[keybindings]` などのセクションも 1 行も消さない
/// (`save_keybindings` / `save_plugins_section` と同じ作法)。
/// 機能の設定 (`<module>.<name>`) も同じ入口で受ける。**点を含むキーは
/// `[features]` 区画へ回す** — トップレベルへ `mymod.flag = true` と書くと
/// TOML の**ドット付きキー**になり、`[features]` からは二度と読めない。
pub fn save_settings(values: &std::collections::BTreeMap<String, String>) -> Result<(), String> {
    if values.is_empty() {
        return Ok(());
    }
    let path = config_path();
    ensure_default();
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = rewrite_settings(&raw, values);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, updated).map_err(|e| format!("config.toml を書けません: {e}"))
}

/// [`save_settings`] のうち**ファイルに触らない部分**。
///
/// 点を含むキー = 機能の設定 → `[features]` 区画。
/// 点を含まないキー = 組み込みの設定 → トップレベル。
/// 分けているのは、トップレベルへ `mymod.flag = true` と書くと TOML の
/// **ドット付きキー** (= `[mymod]` テーブル) になり、`[features]` からは
/// 二度と読めなくなるため。
fn rewrite_settings(raw: &str, values: &std::collections::BTreeMap<String, String>) -> String {
    let mut top: std::collections::BTreeMap<String, String> = Default::default();
    let mut feat: std::collections::BTreeMap<String, String> = Default::default();
    for (k, v) in values {
        if k.contains('.') {
            feat.insert(k.clone(), v.clone());
        } else {
            top.insert(k.clone(), v.clone());
        }
    }
    let mut out = raw.to_string();
    if !top.is_empty() {
        out = rewrite_scalar_settings(&out, &top);
    }
    if !feat.is_empty() {
        out = rewrite_features_section(&out, &feat);
    }
    out
}

/// トップレベルの単純値を差し替えた文字列を返す。
///
/// - **有効な** `key = …` 行があれば、その行だけを置き換える
/// - 無ければ最初のセクション見出しの直前へ足す
///   (コメントアウトされた `# key = …` は説明なので消さない)
/// - セクションの中の同名キーは別物なので触らない
fn rewrite_scalar_settings(
    raw: &str,
    values: &std::collections::BTreeMap<String, String>,
) -> String {
    let src = raw.replace("\r\n", "\n");
    let mut out: Vec<String> = Vec::new();
    let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
    // 追記位置 = 最初のセクション見出しの行番号 (無ければ末尾)
    let mut insert_at: Option<usize> = None;

    for line in src.lines() {
        let t = line.trim();
        let is_header = t.starts_with('[') && t.ends_with(']');
        if is_header && insert_at.is_none() {
            insert_at = Some(out.len());
        }
        if insert_at.is_none() && !t.starts_with('#') {
            let (code, _) = split_inline_comment(t);
            if let Some((k, _)) = split_assign(code.trim()) {
                if let Some(v) = values.get(k) {
                    out.push(format!("{k} = {v}"));
                    done.insert(k.to_string());
                    continue;
                }
            }
        }
        out.push(line.to_string());
    }

    let add: Vec<String> = values
        .iter()
        .filter(|(k, _)| !done.contains(k.as_str()))
        .map(|(k, v)| format!("{k} = {v}"))
        .collect();
    if !add.is_empty() {
        let at = insert_at.unwrap_or(out.len());
        let mut block = add;
        block.push(String::new());
        out.splice(at..at, block);
    }
    let mut text = out.join("\n");
    while text.ends_with("\n\n") {
        text.pop();
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// `"<key>" = …` の引用符付きキーを取り出す。
///
/// `[features]` のキーは `<module>.<name>` で**必ず点を含む**ので、TOML では
/// 引用符付きでしか 1 つのキーにならない (裸で書くと入れ子テーブルになる)。
/// エスケープは扱わない — キーに `"` や `\` は入らない。
fn quoted_key_of(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix('"')?;
    let end = rest.find('"')?;
    let (key, after) = rest.split_at(end);
    // `"…"` の直後は `=` でなければ代入行ではない
    after.get(1..)?.trim_start().strip_prefix('=')?;
    Some(key)
}

/// `[features]` 区画の**渡されたキーの行だけ**を差し替えた文字列を返す。
///
/// [`rewrite_scalar_settings`] / [`rewrite_keybindings_section`] と同じ作法で、
/// **手書きのコメントも、この版が知らない設定も 1 行も消さない**。
/// 区画ごと組み直す方式にすると、新しい版が足した設定を古い版で 1 度
/// 起動しただけで消してしまう (それを避けるのがこの関数の存在理由)。
///
/// - 区画の中に同じキーの行があれば、その行だけを置き換える
/// - 無ければ区画の末尾へ足す
/// - 区画そのものが無ければファイルの末尾に作る
fn rewrite_features_section(
    raw: &str,
    values: &std::collections::BTreeMap<String, String>,
) -> String {
    let src = raw.replace("\r\n", "\n");
    let mut out: Vec<String> = Vec::new();
    let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut in_section = false;
    // 追記位置 = `[features]` 区画の最後の行の直後 (末尾の空行より前)
    let mut insert_at: Option<usize> = None;
    // 末尾の空行を除いた長さ (次の見出しとの間の空行を潰さないため)
    let tail = |o: &Vec<String>| {
        let mut at = o.len();
        while at > 0 && o[at - 1].trim().is_empty() {
            at -= 1;
        }
        at
    };

    for line in src.lines() {
        let t = line.trim();
        // `# [features]` のようなコメント行は見出しではない
        // (既定テンプレートが持ち得るので、誤認すると以降が丸ごと迷子になる)
        let is_header = t.starts_with('[') && t.ends_with(']');
        if is_header {
            if in_section && insert_at.is_none() {
                insert_at = Some(tail(&out));
            }
            in_section = t.trim_start_matches('[').trim_end_matches(']').trim() == "features";
        } else if in_section && !t.starts_with('#') {
            if let Some(k) = quoted_key_of(t) {
                if let Some(v) = values.get(k) {
                    out.push(format!("{} = {v}", toml::Value::String(k.to_string())));
                    done.insert(k.to_string());
                    continue;
                }
            }
        }
        out.push(line.to_string());
    }
    if in_section && insert_at.is_none() {
        insert_at = Some(tail(&out));
    }

    let add: Vec<String> = values
        .iter()
        .filter(|(k, _)| !done.contains(k.as_str()))
        .map(|(k, v)| format!("{} = {v}", toml::Value::String(k.clone())))
        .collect();
    if !add.is_empty() {
        match insert_at {
            Some(at) => {
                out.splice(at..at, add);
            }
            None => {
                while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
                    out.pop();
                }
                if !out.is_empty() {
                    out.push(String::new());
                }
                out.push("[features]".to_string());
                out.extend(add);
            }
        }
    }
    let mut text = out.join("\n");
    while text.ends_with("\n\n") {
        text.pop();
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

// ── 設定画面のレイアウト (純粋関数) ─────────────────────────────────

/// 設定 1 行の列幅。**どの幅でも見切れない**ことを保証するため純関数にする。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SettingsColumns {
    /// 変更マーカーの色バー (VS Code の左端の線)。
    pub marker_w: f32,
    /// 項目名 + 説明文。
    pub label_w: f32,
    /// 値のウィジェット。
    pub value_w: f32,
    /// 「既定へ戻す」ボタン (0 なら出さない)。
    pub reset_w: f32,
    /// 狭いのでボタンをアイコンだけへ縮退させるか。
    pub icon_only: bool,
}

impl SettingsColumns {
    /// 左端からの [開始, 終了] を列順に返す (重なり検査用)。
    pub fn spans(&self) -> Vec<(f32, f32)> {
        let mut out = Vec::with_capacity(4);
        let mut x = 0.0f32;
        for w in [self.marker_w, self.label_w, self.value_w, self.reset_w] {
            if w > 0.0 {
                out.push((x, x + w));
                x += w + SETTINGS_COL_GAP;
            }
        }
        out
    }

    pub fn total_w(&self) -> f32 {
        self.spans().last().map(|(_, e)| *e).unwrap_or(0.0)
    }
}

/// 列と列のあいだ。
pub const SETTINGS_COL_GAP: f32 = 8.0;
/// 変更マーカーの幅 (色バー 1 本)。
const SETTINGS_MARKER_W: f32 = 3.0;
/// 値のウィジェットの幅。
const SETTINGS_VALUE_W: f32 = 190.0;
/// 「既定へ戻す」ボタンの幅。
const SETTINGS_RESET_W: f32 = 96.0;
/// アイコンだけへ縮退したときのボタン幅。
const SETTINGS_RESET_ICON_W: f32 = 30.0;
/// これより狭ければボタンをアイコンだけにする。
const SETTINGS_NARROW_W: f32 = 560.0;
/// 項目名に最低限残す幅。
const SETTINGS_LABEL_MIN_W: f32 = 90.0;

/// 可用幅から列幅を決める。**戻り値の合計は必ず `avail_w` 以下**。
pub fn settings_columns(avail_w: f32) -> SettingsColumns {
    let avail = avail_w.max(0.0);
    let icon_only = avail < SETTINGS_NARROW_W;
    let reset_w = if icon_only {
        SETTINGS_RESET_ICON_W
    } else {
        SETTINGS_RESET_W
    };
    let gaps = SETTINGS_COL_GAP * 3.0;
    let fixed = SETTINGS_MARKER_W + SETTINGS_VALUE_W + reset_w + gaps;
    let label_w = avail - fixed;
    if label_w >= SETTINGS_LABEL_MIN_W {
        return SettingsColumns {
            marker_w: SETTINGS_MARKER_W,
            label_w,
            value_w: SETTINGS_VALUE_W,
            reset_w,
            icon_only,
        };
    }
    // 極端に狭い: 戻すボタンを畳み、名前と値を比率で分ける
    // (マーカーは 3px しか使わないので、どんなに狭くても必ず残す)
    let usable = (avail - SETTINGS_MARKER_W - SETTINGS_COL_GAP * 2.0).max(0.0);
    let value_w = (usable * 0.45).min(SETTINGS_VALUE_W);
    SettingsColumns {
        marker_w: SETTINGS_MARKER_W.min(avail),
        label_w: (usable - value_w).max(0.0),
        value_w,
        reset_w: 0.0,
        icon_only: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧 `git_blame = true / false` (**真偽値**) をそのまま読めること。
    /// 既存ユーザーの config.toml / state.toml / .zaivern.toml を壊さないのが
    /// 3 段化の最優先条件で、ここが落ちたら「設定が黙って off に戻る」になる。
    #[test]
    fn 旧いgit_blameの真偽値をそのまま読める() {
        // config.toml 本体 (真偽値 → true は「全行」)
        let c: Config = toml::from_str("git_blame = true").expect("旧形式が読める");
        assert_eq!(c.git_blame, BlameMode::All);
        let c: Config = toml::from_str("git_blame = false").expect("旧形式が読める");
        assert_eq!(c.git_blame, BlameMode::Off);
        // 新形式の 3 段
        for (t, want) in [
            ("off", BlameMode::Off),
            ("current", BlameMode::Current),
            ("all", BlameMode::All),
        ] {
            let c: Config =
                toml::from_str(&format!("git_blame = \"{t}\"")).expect("新形式が読める");
            assert_eq!(c.git_blame, want, "{t}");
        }
        // 未知の値・空白・大文字混じりでも落ちず既定へ倒す (設定を壊しても動く)
        for t in ["", "  ", "ALL ", "なにか"] {
            let c: Config = toml::from_str(&format!("git_blame = \"{t}\""))
                .unwrap_or_else(|e| panic!("{t:?} で落ちた: {e}"));
            let want = if t.trim().eq_ignore_ascii_case("all") {
                BlameMode::All
            } else {
                BlameMode::Off
            };
            assert_eq!(c.git_blame, want, "{t:?}");
        }
        // state.toml / .zaivern.toml (Option 側) も同じ入口を通る
        let st: UiState = toml::from_str("git_blame = true").expect("state の旧形式");
        assert_eq!(st.git_blame, Some(BlameMode::All));
        let ov: Overlay = toml::from_str("git_blame = false").expect("overlay の旧形式");
        assert_eq!(ov.git_blame, Some(BlameMode::Off));
        let ov: Overlay = toml::from_str(r#"git_blame = "current""#).expect("overlay の新形式");
        assert_eq!(ov.git_blame, Some(BlameMode::Current));
        // 書き戻しは必ず新形式の**文字列** (真偽値へ戻さない)
        #[derive(Serialize)]
        struct W {
            git_blame: BlameMode,
        }
        let out = toml::to_string(&W {
            git_blame: BlameMode::Current,
        })
        .expect("書ける");
        assert!(
            out.contains(r#"git_blame = "current""#),
            "書き戻しが文字列になっていない: {out}"
        );
        // 3 段の往復と、設定画面の選択肢との一致
        for m in [BlameMode::Off, BlameMode::Current, BlameMode::All] {
            assert_eq!(BlameMode::from_config_str(m.config_str()), m);
            assert!(!m.label().is_empty());
            assert!(
                BLAME_MODES.contains(&m.config_str()),
                "{m:?} が選択肢に無い"
            );
        }
        assert_eq!(BLAME_MODES.len(), 3);
        assert!(!BlameMode::Off.is_on());
        assert!(BlameMode::Current.is_on() && BlameMode::All.is_on());
        assert_eq!(BlameMode::default(), BlameMode::Off);
        // setting_value / set_setting も 3 段を往復する (設定画面の経路)
        let mut cfg = Config::default();
        for m in [BlameMode::All, BlameMode::Current, BlameMode::Off] {
            assert!(set_setting_value(
                &mut cfg,
                "git_blame",
                &SettingValue::Text(m.config_str().into())
            ));
            assert_eq!(cfg.git_blame, m);
            assert_eq!(cfg.global_git_blame, m, "グローバル側へ写っていない");
            assert_eq!(
                setting_value(&cfg, "git_blame"),
                Some(SettingValue::Text(m.config_str().into()))
            );
        }
        // 旧形式の真偽値が設定経路から来ても受ける
        assert!(set_setting_value(
            &mut cfg,
            "git_blame",
            &SettingValue::Bool(true)
        ));
        assert_eq!(cfg.git_blame, BlameMode::All);
    }

    // load() / ensure_default() / save_state() は実ユーザーの ~/.zaivern を
    // 読み書きするためテストしない。実体の load_from_dir() / save_state_to_dir()
    // は state_overlay_tests で一時ディレクトリを差し込んで検証する。

    // ---- Config / AgentPreset の既定値 ----

    // ─────────────────────────────────────────────────────────────────
    // 設定 GUI — 一覧 / 説明 / @modified / 既定へ戻す / 書き戻し
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn 全ての設定項目に説明文がテンプレートから届く() {
        // 説明を 2 か所に書かないための番人。GUI へ項目を足したら
        // DEFAULT_CONFIG にもコメントを書く (逆は自由)。
        let missing: Vec<&str> = setting_defs()
            .iter()
            .filter(|d| setting_doc(d.key).trim().is_empty())
            .map(|d| d.key)
            .collect();
        assert!(
            missing.is_empty(),
            "DEFAULT_CONFIG に説明コメントが無い設定: {missing:?}"
        );
    }

    #[test]
    fn 設定の定義はキーが重複せず現在値と既定を必ず引ける() {
        let cfg = Config::default();
        let mut seen = std::collections::HashSet::new();
        for d in setting_defs() {
            assert!(seen.insert(d.key), "キーが重複している: {}", d.key);
            assert!(
                setting_value(&cfg, d.key).is_some(),
                "{} の現在値を引けない",
                d.key
            );
            assert!(
                setting_default(d.key).is_some(),
                "{} の既定を引けない",
                d.key
            );
            assert!(!d.label.is_empty() && !d.group.is_empty());
            // 候補型は既定値が候補の中にあること (GUI で選べない値にしない)
            if let SettingKind::Choice(opts) = d.kind {
                let Some(SettingValue::Text(v)) = setting_default(d.key) else {
                    panic!("{} は Choice なのに既定が文字列ではない", d.key);
                };
                assert!(
                    opts.contains(&v.as_str()),
                    "{} の既定 {v} が候補に無い",
                    d.key
                );
            }
        }
    }

    #[test]
    fn 設定の書き込みは型が合うときだけ通る() {
        let mut cfg = Config::default();
        assert!(set_setting_value(
            &mut cfg,
            "word_wrap",
            &SettingValue::Bool(true)
        ));
        assert!(cfg.word_wrap);
        // 型違いは黙って無視 (壊さない)
        assert!(!set_setting_value(
            &mut cfg,
            "word_wrap",
            &SettingValue::Int(1)
        ));
        assert!(cfg.word_wrap);
        assert!(!set_setting_value(
            &mut cfg,
            "存在しないキー",
            &SettingValue::Bool(true)
        ));
        // global_* の控えも一緒に動く (overlay 下でも永続化されるため)
        assert!(set_setting_value(
            &mut cfg,
            "theme",
            &SettingValue::Text("zaivern-light".into())
        ));
        assert_eq!(cfg.global_theme, "zaivern-light");
    }

    #[test]
    fn modifiedは既定と同じ値へ戻したら外れる() {
        let mut cfg = Config::default();
        assert!(!is_setting_modified(&cfg, "word_wrap"));
        cfg.word_wrap = true;
        assert!(is_setting_modified(&cfg, "word_wrap"));
        // 「一度触ったから」ではなく「いま既定と違うか」で判定する
        cfg.word_wrap = false;
        assert!(!is_setting_modified(&cfg, "word_wrap"));

        // 浮動小数も同じ (15.0 -> 18.0 -> 15.0)
        cfg.editor_font_size = 18.0;
        assert!(is_setting_modified(&cfg, "editor_font_size"));
        cfg.editor_font_size = Config::default().editor_font_size;
        assert!(!is_setting_modified(&cfg, "editor_font_size"));
    }

    #[test]
    fn modifiedフィルタは変えた項目だけを残す() {
        let mut cfg = Config::default();
        assert!(
            settings_rows(&cfg, "", true).is_empty(),
            "既定のままなのに @modified に何か出ている"
        );
        cfg.minimap = true;
        let rows = settings_rows(&cfg, "", true);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "minimap");
        // クエリに @modified と書いても同じ
        let rows = settings_rows(&cfg, "@modified", false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "minimap");
        // 絞り込みと併用できる
        assert!(settings_rows(&cfg, "@modified minimap", false).len() == 1);
        assert!(settings_rows(&cfg, "@modified word_wrap", false).is_empty());
    }

    #[test]
    fn 設定の検索はあいまい一致で拾う() {
        let cfg = Config::default();
        let all = settings_rows(&cfg, "", false);
        // 組み込み + 機能が宣言した設定。機能が 0 個でも成り立つ。
        assert_eq!(all.len(), all_setting_defs().len(), "空クエリで全部出ない");
        let hit = settings_rows(&cfg, "hotexit", false);
        assert!(
            hit.iter().any(|d| d.key == "hot_exit"),
            "hot_exit があいまい検索で出ない"
        );
        // 日本語のラベルでも引ける
        assert!(settings_rows(&cfg, "折り返", false)
            .iter()
            .any(|d| d.key == "word_wrap"));
        assert!(settings_rows(&cfg, "存在しない語彙xyzzy", false).is_empty());
    }

    #[test]
    fn 設定の書き戻しでコメントと未対応の設定が残る() {
        let raw = concat!(
            "# 手書きのコメント (絶対に消さない)\n",
            "theme = \"zaivern-dark\"\n",
            "editor_font_size = 15.0\n",
            "# コメントアウトされた既定はドキュメントなので残す\n",
            "# minimap = false\n",
            "\"未対応の設定\" = 1\n",
            "unknown_key = \"手で書いた値\"\n",
            "\n",
            "[keybindings]\n",
            "save = \"cmd+s\"\n",
            "\n",
            "[plugins]\n",
            "disabled = [\"foo\"]\n",
        );
        let mut vals = std::collections::BTreeMap::new();
        vals.insert("theme".to_string(), "\"zaivern-light\"".to_string());
        vals.insert("minimap".to_string(), "true".to_string());
        let out = rewrite_scalar_settings(raw, &vals);

        assert!(out.contains("# 手書きのコメント (絶対に消さない)"));
        assert!(out.contains("# コメントアウトされた既定はドキュメントなので残す"));
        assert!(out.contains("# minimap = false"), "説明のコメントを消した");
        assert!(out.contains("unknown_key = \"手で書いた値\""));
        assert!(out.contains("\"未対応の設定\" = 1"));
        assert!(out.contains("[keybindings]") && out.contains("save = \"cmd+s\""));
        assert!(out.contains("[plugins]") && out.contains("disabled = [\"foo\"]"));
        // 変えた値は 1 本だけ (既存行の置き換え / 新規は見出しの手前へ)
        assert!(out.contains("theme = \"zaivern-light\""));
        assert!(!out.contains("theme = \"zaivern-dark\""));
        assert_eq!(out.matches("\nminimap = true").count(), 1);
        assert!(
            out.contains("editor_font_size = 15.0"),
            "触っていない値が消えた"
        );
        // 書き戻した結果がそのまま読めること
        let cfg: Config = toml::from_str(&out).expect("書き戻した config.toml が読めない");
        assert_eq!(cfg.theme, "zaivern-light");
        assert!(cfg.minimap);
    }

    #[test]
    fn 設定の書き戻しは何度やっても同じ形になる() {
        let raw = "theme = \"zaivern-dark\"\n\n[plugins]\ndisabled = []\n";
        let mut vals = std::collections::BTreeMap::new();
        vals.insert("word_wrap".to_string(), "true".to_string());
        let once = rewrite_scalar_settings(raw, &vals);
        let twice = rewrite_scalar_settings(&once, &vals);
        assert_eq!(once, twice, "書くたびに差分が出ている");
        assert_eq!(once.matches("word_wrap").count(), 1);
        // 追記はセクションより前 (トップレベルのまま = TOML として正しい)
        let cfg: Config = toml::from_str(&once).unwrap();
        assert!(cfg.word_wrap);
    }

    #[test]
    fn 設定の書き戻しはセクションの中の同名キーを触らない() {
        let raw = concat!(
            "theme = \"zaivern-dark\"\n",
            "\n",
            "[plugins.settings.demo]\n",
            "theme = \"プラグインの設定であって本体ではない\"\n",
        );
        let mut vals = std::collections::BTreeMap::new();
        vals.insert("theme".to_string(), "\"zaivern-light\"".to_string());
        let out = rewrite_scalar_settings(raw, &vals);
        assert!(out.contains("theme = \"プラグインの設定であって本体ではない\""));
        assert!(out.contains("theme = \"zaivern-light\""));
    }

    #[test]
    fn 既定へ戻すは1項目でも全部でも効く() {
        let mut cfg = Config::default();
        cfg.minimap = true;
        cfg.word_wrap = true;
        cfg.tab_size = 8;
        // 1 項目
        let def = setting_default("minimap").unwrap();
        assert!(set_setting_value(&mut cfg, "minimap", &def));
        assert!(!cfg.minimap);
        assert!(cfg.word_wrap, "他の項目まで戻してはいけない");
        // 全部
        for d in setting_defs() {
            let def = setting_default(d.key).unwrap();
            assert!(set_setting_value(&mut cfg, d.key, &def), "{}", d.key);
        }
        assert!(!cfg.word_wrap);
        assert_eq!(cfg.tab_size, Config::default().tab_size);
        assert!(
            setting_defs()
                .iter()
                .all(|d| !is_setting_modified(&cfg, d.key)),
            "全部戻したのに modified が残っている"
        );
    }

    #[test]
    fn テンプレートの説明抽出は塊と名指しと行末コメントを拾う() {
        let raw = concat!(
            "# ────────────────\n",
            "# 塊のコメントはそのまま説明になる\n",
            "alpha = \"x\"\n",
            "# ひとまとめの説明\n",
            "#   beta  = ベータの説明。\n",
            "#           続きの行もつながる。\n",
            "#   gamma = ガンマの説明。\n",
            "# beta = 1\n",
            "# gamma = 2\n",
            "# delta = false   # 行末コメントが説明になる\n",
            "# epsilon = false のときは効きません。\n",
            "[section]\n",
            "zeta = 1\n",
        );
        let docs = template_docs(raw);
        assert_eq!(docs["alpha"], "塊のコメントはそのまま説明になる");
        assert_eq!(docs["beta"], "ベータの説明。 続きの行もつながる。");
        assert_eq!(docs["gamma"], "ガンマの説明。");
        assert_eq!(docs["delta"], "行末コメントが説明になる");
        // 値がリテラルでない = ただの説明文。設定の行として数えない
        assert!(!docs.contains_key("epsilon"));
        // セクションより後ろは見ない
        assert!(!docs.contains_key("zeta"));
    }

    #[test]
    fn 設定表の列はどの幅でも収まり重ならない() {
        // 極端な画面サイズ (900×700 / 1200×300 / 400×700) まで含めて検証する。
        // 幅だけが列を決めるので、高さは行数の目安としてだけ使う。
        for (w, h) in [
            (1200.0f32, 300.0f32),
            (900.0, 700.0),
            (400.0, 700.0),
            (700.0, 700.0),
            (560.0, 400.0),
            (300.0, 300.0),
            (120.0, 200.0),
            (0.0, 0.0),
        ] {
            let c = settings_columns(w);
            let spans = c.spans();
            assert!(
                c.total_w() <= w + 0.01,
                "{w}×{h} で列がはみ出した: {c:?} -> {}",
                c.total_w()
            );
            for s in &spans {
                assert!(s.0 >= -0.01 && s.1 <= w + 0.01, "{w}×{h}: {s:?} が範囲外");
                assert!(s.1 >= s.0, "{w}×{h}: 負の幅 {s:?}");
            }
            for pair in spans.windows(2) {
                assert!(
                    pair[0].1 <= pair[1].0 + 0.01,
                    "{w}×{h}: 列が重なっている {pair:?}"
                );
            }
            if w > 0.0 {
                assert!(c.marker_w > 0.0, "{w}×{h}: 変更マーカーが消えた");
            }
            if w >= 300.0 {
                assert!(
                    c.label_w > 0.0 && c.value_w > 0.0,
                    "{w}×{h}: 名前か値が消えた"
                );
            }
        }
        // 広いときだけ「既定へ戻す」がラベル付きで出る
        assert!(!settings_columns(1200.0).icon_only);
        assert!(settings_columns(1200.0).reset_w > settings_columns(400.0).reset_w);
        assert!(settings_columns(400.0).icon_only);
    }

    #[test]
    fn 設定値のtoml表現がそのまま読み直せる() {
        // f32 は "15" ではなく "15.0" と書く (整数だと次回 float として読めない)
        assert_eq!(SettingValue::Float(15.0).to_toml(), "15.0");
        assert_eq!(SettingValue::Float(1.25).to_toml(), "1.25");
        assert_eq!(SettingValue::Bool(true).to_toml(), "true");
        assert_eq!(SettingValue::Int(400).to_toml(), "400");
        // 引用符とバックスラッシュを含む値もそのまま往復する
        let v = SettingValue::Text("a\"b\\c 日本語".into());
        let line = format!("theme = {}", v.to_toml());
        let cfg: Config = toml::from_str(&line).expect("書いた値が読めない");
        assert_eq!(cfg.theme, "a\"b\\c 日本語");
        // 全項目の既定値を書き出して読み直せること
        let mut vals = std::collections::BTreeMap::new();
        for d in setting_defs() {
            vals.insert(d.key.to_string(), setting_default(d.key).unwrap().to_toml());
        }
        let text = rewrite_scalar_settings("", &vals);
        let cfg: Config = toml::from_str(&text).expect("既定を書き出したら読めなくなった");
        for d in setting_defs() {
            assert!(!is_setting_modified(&cfg, d.key), "{} が既定と違う", d.key);
        }
    }

    #[test]
    fn default_config_has_expected_values() {
        let c = Config::default();
        assert_eq!(c.theme, "zaivern-dark");
        assert_eq!(c.editor_font_size, 15.0);
        assert_eq!(c.terminal_font_size, 13.0);
        assert!(c.show_hidden_files);
        assert_eq!(c.approval_mode, "ask", "既定は必ず安全側 (ask)");
        assert!(
            c.confirm_drag_and_drop,
            "D&D の移動確認は既定でオン (VS Code と同じ)"
        );
        assert!(c.enable_trash, "削除は既定でゴミ箱行き (戻せる側が既定)");
        assert!(c.show_pet);
        assert_eq!(c.pet_image, None);
        assert_eq!(c.pet_x, None);
        assert_eq!(c.pet_y, None);
        assert_eq!(c.pet_variant, "blocky");
        assert_eq!(c.pet_scale, 1.0);
        assert!(c.pet_free_roam);
        assert!(c.pet_sleep);
        assert!(c.pet_sounds);
        assert!(c.pet_bubbles);
        assert!(!c.pet_auto_yes, "自動YESは既定でオフ (ユーザー承認必須)");
        assert_eq!(c.pet_approve_keys, "\r", "承認は Enter");
        assert_eq!(c.pet_deny_keys, "\u{1b}", "拒否は ESC");
        assert_eq!(c.voice_engine, "auto");
        assert_eq!(c.voice_target, "active");
        assert_eq!(c.voice_lang, "ja-JP");
        assert_eq!(c.voice_command, "");
        assert_eq!(c.voice_keyword, "", "空 = 常に手動 Enter");
        assert!(c.keybindings.is_empty());
        assert!(!c.agents.is_empty());
    }

    /// エディタ細部の既定値は **VS Code の既定に合わせる**。
    ///
    /// 保存時の整形は全部オフ (保存しただけで差分が増えないため)、
    /// 括弧の色分けとインデント推定はオン、ルーラーは 1 本も引かない。
    #[test]
    fn 保存時の整形とエディタ表示の既定はvscodeに合わせる() {
        let c = Config::default();
        assert!(!c.trim_trailing_whitespace, "files.trimTrailingWhitespace");
        assert!(!c.trim_final_newlines, "files.trimFinalNewlines");
        assert!(!c.insert_final_newline, "files.insertFinalNewline");
        assert!(!c.format_on_save, "editor.formatOnSave");
        assert!(c.bracket_colorization, "editor.bracketPairColorization");
        assert!(c.rulers.is_empty(), "editor.rulers は既定で空");
        assert!(c.detect_indentation, "editor.detectIndentation");
        assert_eq!(c.tab_size, 4, "editor.tabSize");
        assert!(c.insert_spaces, "editor.insertSpaces");
        // 同梱テンプレートを読んでも同じ既定になる (コメントアウト済み)
        let t: Config = toml::from_str(DEFAULT_CONFIG).expect("同梱テンプレは常にパースできる");
        assert!(!t.trim_trailing_whitespace);
        assert!(!t.trim_final_newlines);
        assert!(!t.insert_final_newline);
        assert!(!t.format_on_save);
        assert!(t.bracket_colorization);
        assert!(t.rulers.is_empty());
        assert!(t.detect_indentation);
        assert_eq!(t.tab_size, 4);
        assert!(t.insert_spaces);
        // 実際に書けば読める (設定として機能する)
        let on: Config = toml::from_str(
            "trim_trailing_whitespace = true\n\
             trim_final_newlines = true\n\
             insert_final_newline = true\n\
             format_on_save = true\n\
             bracket_colorization = false\n\
             rulers = [80, 120]\n\
             detect_indentation = false\n\
             tab_size = 2\n\
             insert_spaces = false\n",
        )
        .expect("個別にオンオフできる");
        assert!(on.trim_trailing_whitespace);
        assert!(on.trim_final_newlines);
        assert!(on.insert_final_newline);
        assert!(on.format_on_save);
        assert!(!on.bracket_colorization);
        assert_eq!(on.rulers, vec![80, 120]);
        assert!(!on.detect_indentation);
        assert_eq!(on.tab_size, 2);
        assert!(!on.insert_spaces);
    }

    #[test]
    fn default_agent_preset_is_plain_shell() {
        let a = AgentPreset::default();
        assert_eq!(a.name, "Shell");
        assert_eq!(a.command, "", "空コマンド = ログインシェル");
        assert_eq!(a.icon, "🖥");
        assert_eq!(a.cwd, None);
        assert!(a.env.is_empty());
    }

    #[test]
    fn default_agents_cover_every_cli() {
        let agents = default_agents();
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Claude Code",
                "Claude Code (全自動)",
                "Codex",
                "Codex (全自動)",
                "Gemini CLI",
                "Gemini CLI (全自動)",
                "Antigravity",
                "Antigravity (全自動)",
                "Cursor",
                "Cursor (全自動)",
                "Droid",
                "Droid (全自動)",
                "Shell",
            ]
        );
    }

    /// 既定プリセットは **カタログから導出** される。おすすめ表に足した CLI が
    /// そのまま既定へ載ること、素のシェルが必ず最後に来ることを固定する。
    #[test]
    fn default_agents_are_derived_from_the_catalog() {
        let agents = default_agents();
        for bin in agents::DEFAULT_PRESET_BINS {
            let spec = agents::spec_for_bin(bin).expect("おすすめ表の bin はカタログにある");
            assert!(
                agents.iter().any(|a| a.command == spec.launch_command()),
                "{bin} の素のプリセットが既定に無い"
            );
            if crate::agent_picker::auto_preset(spec).is_some() {
                assert!(
                    agents
                        .iter()
                        .any(|a| a.name
                            == format!("{}{}", spec.label, crate::agent_picker::AUTO_SUFFIX)),
                    "{bin} の全自動プリセットが既定に無い"
                );
            }
        }
        let last = agents.last().expect("空にはならない");
        assert_eq!(last.command, "", "最後は素のシェル");
    }

    #[test]
    fn default_agents_auto_presets_carry_bypass_flags() {
        let agents = default_agents();
        for a in &agents {
            if a.name.contains("全自動") {
                // フラグ名は CLI ごとに違うので、判定は agents.rs の表に任せる
                assert!(
                    agents::command_is_bypass(&a.command),
                    "{} が bypass と判定されない: {:?}",
                    a.name,
                    a.command
                );
            }
        }
        // 通常プリセットは素の起動コマンドのまま (bypass フラグが混ざらない)
        for a in agents.iter().filter(|a| !a.name.contains("全自動")) {
            assert!(
                !agents::command_is_bypass(&a.command),
                "{} に bypass フラグが混ざっている: {:?}",
                a.name,
                a.command
            );
        }
        assert_eq!(
            agents
                .iter()
                .filter(|a| !a.name.contains("全自動"))
                .map(|a| a.command.as_str())
                .collect::<Vec<_>>(),
            vec![
                "claude",
                "codex",
                "gemini",
                "agy",
                "cursor-agent",
                "droid",
                ""
            ]
        );
    }

    #[test]
    fn default_agents_all_have_icon_and_name() {
        for a in default_agents() {
            assert!(!a.name.is_empty(), "名前が空のプリセットがある");
            assert!(!a.icon.is_empty(), "{} のアイコンが空", a.name);
        }
    }

    // ---- [[agents]] の追記 ----

    #[test]
    fn rendered_agent_preset_parses_back_unchanged() {
        let mut env = HashMap::new();
        env.insert("GOOSE_MODE".to_string(), "auto".to_string());
        let p = AgentPreset {
            name: "Goose (全自動)".into(),
            command: "goose".into(),
            icon: "⚡".into(),
            cwd: None,
            env,
        };
        let text = render_agent_preset(&p);
        let back: Config = toml::from_str(&text).expect("追記したブロックは読み戻せる");
        let a = back.agents.last().expect("agents が空");
        assert_eq!(a.name, p.name);
        assert_eq!(a.command, p.command);
        assert_eq!(a.icon, p.icon);
        assert_eq!(a.env.get("GOOSE_MODE").map(String::as_str), Some("auto"));
    }

    #[test]
    fn rendered_agent_preset_escapes_quotes_and_backslashes() {
        let p = AgentPreset {
            name: "変な \"名前\"".into(),
            command: r#"foo --msg "a\b""#.into(),
            icon: "👾".into(),
            cwd: Some(r"C:\tmp".into()),
            env: HashMap::new(),
        };
        let text = render_agent_preset(&p);
        let back: Config = toml::from_str(&text).expect("引用符が入っても壊れない");
        let a = back.agents.last().unwrap();
        assert_eq!(a.name, p.name);
        assert_eq!(a.command, p.command);
        assert_eq!(a.cwd.as_deref(), Some(r"C:\tmp"));
    }

    #[test]
    fn appending_a_preset_keeps_every_existing_one() {
        // 既存の config.toml を書き換えない ＝ 追記後も元のプリセットが全部残る。
        let base = DEFAULT_CONFIG.to_string();
        let before: Config = toml::from_str(&base).unwrap();
        let p = AgentPreset {
            name: "Qwen Code".into(),
            command: "qwen".into(),
            icon: "🐉".into(),
            cwd: None,
            env: HashMap::new(),
        };
        let after_text = format!("{base}{}", render_agent_preset(&p));
        // 元の本文は 1 文字も変わっていない
        assert!(after_text.starts_with(&base));
        let after: Config = toml::from_str(&after_text).expect("追記後もパースできる");
        assert_eq!(after.agents.len(), before.agents.len() + 1);
        for (i, a) in before.agents.iter().enumerate() {
            assert_eq!(after.agents[i].name, a.name, "既存プリセットの順序が崩れた");
            assert_eq!(after.agents[i].command, a.command);
        }
        assert_eq!(after.agents.last().unwrap().command, "qwen");
    }

    #[test]
    fn appending_twice_does_not_swallow_the_previous_block() {
        // env をインラインテーブルにしている理由の回帰テスト。
        // ヘッダ形式 ([agents.env]) だと、次に足した [[agents]] との間で
        // 所属が壊れやすい。
        let mut env = HashMap::new();
        env.insert("A".to_string(), "1".to_string());
        let first = AgentPreset {
            name: "First".into(),
            command: "goose".into(),
            icon: "🐦".into(),
            cwd: None,
            env,
        };
        let second = AgentPreset {
            name: "Second".into(),
            command: "qwen".into(),
            icon: "🐉".into(),
            cwd: None,
            env: HashMap::new(),
        };
        let text = format!(
            "{}{}{}",
            DEFAULT_CONFIG,
            render_agent_preset(&first),
            render_agent_preset(&second)
        );
        let cfg: Config = toml::from_str(&text).expect("2 回追記してもパースできる");
        let names: Vec<&str> = cfg.agents.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"First") && names.contains(&"Second"));
        let f = cfg.agents.iter().find(|a| a.name == "First").unwrap();
        assert_eq!(f.env.get("A").map(String::as_str), Some("1"));
        let s = cfg.agents.iter().find(|a| a.name == "Second").unwrap();
        assert!(s.env.is_empty(), "後続ブロックが前の env を吸い込んだ");
    }

    // ---- DEFAULT_CONFIG テンプレート ----

    #[test]
    fn default_config_template_parses_into_config() {
        let c: Config = toml::from_str(DEFAULT_CONFIG).expect("同梱テンプレは常にパースできる");
        assert_eq!(c.theme, "zaivern-dark");
        assert_eq!(c.editor_font_size, 15.0);
        assert_eq!(c.terminal_font_size, 13.0);
        assert!(c.show_hidden_files);
        assert_eq!(c.approval_mode, "ask");
        assert!(c.show_pet);
        // コメントアウトされている項目は Default から埋まる
        assert_eq!(c.voice_engine, "auto");
        assert_eq!(c.pet_variant, "blocky");
        assert!(c.keybindings.is_empty(), "keybindings 例はコメントアウト");
    }

    #[test]
    fn default_config_template_agents_match_default_agents() {
        let c: Config = toml::from_str(DEFAULT_CONFIG).expect("parse ok");
        let from_template: Vec<(&str, &str)> = c
            .agents
            .iter()
            .map(|a| (a.name.as_str(), a.command.as_str()))
            .collect();
        let builtin = default_agents();
        let from_code: Vec<(&str, &str)> = builtin
            .iter()
            .map(|a| (a.name.as_str(), a.command.as_str()))
            .collect();
        assert_eq!(
            from_template, from_code,
            "テンプレートと default_agents() がずれている"
        );
    }

    /// **既存ユーザーの config.toml を壊さない**こと。
    ///
    /// 新しい CLI を既定プリセットへ足しても、[[agents]] を書いている設定は
    /// その内容だけが残る (新既定が勝手に混ざらない)。逆に [[agents]] が
    /// 1 つも無い古い設定では、新しい既定一式が入る。
    #[test]
    fn old_config_without_new_presets_still_loads() {
        // 新プリセット導入前の設定 (claude / codex / agy の 3 件だけ)
        let old = r#"
theme = "zaivern-dark"
editor_font_size = 15.0

[[agents]]
name = "Claude Code"
icon = "👾"
command = "claude"

[[agents]]
name = "Codex"
icon = "💡"
command = "codex"

[[agents]]
name = "Antigravity"
icon = "🚀"
command = "agy"
"#;
        let cfg: Config = toml::from_str(old).expect("旧設定が読めなくなった");
        assert_eq!(cfg.theme, "zaivern-dark");
        assert_eq!(cfg.agents.len(), 3, "旧設定のプリセットに新既定が混ざった");
        assert_eq!(cfg.agents[0].command, "claude");
        assert_eq!(cfg.agents[2].command, "agy");
        // 旧設定に無い新フィールドは既定値で埋まる
        assert!(cfg
            .agents
            .iter()
            .all(|a| a.cwd.is_none() && a.env.is_empty()));

        // [[agents]] を書いていない古い設定は、新しい既定一式を受け取る
        let bare: Config = toml::from_str("theme = \"zaivern-dark\"").expect("読める");
        assert_eq!(bare.agents.len(), default_agents().len());
        assert!(
            bare.agents.iter().any(|a| a.command == "gemini"),
            "新しく足した CLI が既定に出てこない"
        );

        // 新しい既定はそのまま書き出して読み戻せる (往復で欠けない)
        for p in default_agents() {
            let text = render_agent_preset(&p);
            let back: Config = toml::from_str(&text).expect("往復で壊れた");
            assert_eq!(back.agents.len(), 1, "{}", p.name);
            assert_eq!(back.agents[0].name, p.name);
            assert_eq!(back.agents[0].command, p.command);
            assert_eq!(back.agents[0].icon, p.icon);
            assert_eq!(back.agents[0].env, p.env);
        }
    }

    // ---- Config のデシリアライズ (正常系) ----

    #[test]
    fn config_from_empty_toml_equals_defaults() {
        let c: Config = toml::from_str("").expect("空 TOML は既定値");
        let d = Config::default();
        assert_eq!(c.theme, d.theme);
        assert_eq!(c.approval_mode, d.approval_mode);
        assert_eq!(c.editor_font_size, d.editor_font_size);
        assert_eq!(c.agents.len(), d.agents.len(), "agents も既定が入る");
    }

    #[test]
    fn config_partial_toml_keeps_other_defaults() {
        let c: Config = toml::from_str("theme = \"zaivern-light\"\n").expect("parse ok");
        assert_eq!(c.theme, "zaivern-light");
        assert_eq!(c.approval_mode, "ask", "書かれていない項目は既定のまま");
        assert_eq!(c.terminal_font_size, 13.0);
    }

    #[test]
    fn config_ignores_unknown_fields() {
        // deny_unknown_fields を付けていないので、将来削除された項目が
        // 残っていても設定全体が壊れない
        let c: Config =
            toml::from_str("theme = \"x\"\nlegacy_option = 42\n").expect("未知のキーは無視される");
        assert_eq!(c.theme, "x");
    }

    #[test]
    fn config_accepts_optional_pet_position() {
        let c: Config = toml::from_str("pet_x = 12.5\npet_y = -3.0\npet_image = \"/tmp/p.png\"\n")
            .expect("parse ok");
        assert_eq!(c.pet_x, Some(12.5));
        assert_eq!(c.pet_y, Some(-3.0));
        assert_eq!(c.pet_image, Some("/tmp/p.png".to_string()));
    }

    #[test]
    fn config_parses_keybindings_table() {
        let c: Config = toml::from_str("[keybindings]\nsave = \"cmd+s\"\n").expect("parse ok");
        assert_eq!(c.keybindings.get("save").map(String::as_str), Some("cmd+s"));
        assert_eq!(c.keybindings.len(), 1);
    }

    #[test]
    fn agent_preset_parses_env_and_cwd() {
        let c: Config = toml::from_str(
            "[[agents]]\nname = \"X\"\ncommand = \"x --go\"\ncwd = \"/tmp\"\nenv = { A = \"1\" }\n",
        )
        .expect("parse ok");
        assert_eq!(c.agents.len(), 1, "書かれた agents が既定を置き換える");
        let a = &c.agents[0];
        assert_eq!(a.name, "X");
        assert_eq!(a.command, "x --go");
        assert_eq!(a.cwd, Some("/tmp".to_string()));
        assert_eq!(a.env.get("A").map(String::as_str), Some("1"));
        assert_eq!(a.icon, "🖥", "icon 省略時は既定アイコン");
    }

    #[test]
    fn agent_preset_allows_all_fields_omitted() {
        let c: Config = toml::from_str("[[agents]]\n").expect("空の agents 要素も既定で埋まる");
        assert_eq!(c.agents.len(), 1);
        assert_eq!(c.agents[0].name, "Shell");
        assert_eq!(c.agents[0].command, "");
    }

    // ---- Config のデシリアライズ (境界値・異常系) ----

    #[test]
    fn config_empty_strings_survive_parsing() {
        // load() 側で正規化されるので、パース段階では空文字がそのまま通る
        let c: Config = toml::from_str("theme = \"\"\napproval_mode = \"\"\nvoice_lang = \"\"\n")
            .expect("parse ok");
        assert_eq!(c.theme, "");
        assert_eq!(c.approval_mode, "");
        assert_eq!(c.voice_lang, "");
    }

    #[test]
    fn config_extreme_font_sizes_parse_unclamped() {
        // clamp は load() の中でのみ行われる (パース自体は素通し)
        let c: Config = toml::from_str("editor_font_size = 999.0\nterminal_font_size = -5.0\n")
            .expect("parse ok");
        assert_eq!(c.editor_font_size, 999.0);
        assert_eq!(c.terminal_font_size, -5.0);
        assert_eq!(c.editor_font_size.clamp(8.0, 32.0), 32.0);
        assert_eq!(c.terminal_font_size.clamp(7.0, 28.0), 7.0);
    }

    #[test]
    fn config_pet_scale_clamp_boundaries() {
        let c: Config = toml::from_str("pet_scale = 0.0\n").expect("parse ok");
        assert_eq!(c.pet_scale.clamp(0.5, 2.0), 0.5);
        let c: Config = toml::from_str("pet_scale = 5.0\n").expect("parse ok");
        assert_eq!(c.pet_scale.clamp(0.5, 2.0), 2.0);
        let c: Config = toml::from_str("pet_scale = 1.4\n").expect("parse ok");
        assert_eq!(c.pet_scale.clamp(0.5, 2.0), 1.4, "範囲内はそのまま");
    }

    #[test]
    fn config_empty_agents_list_parses_as_empty() {
        // load() は空なら default_agents() を入れ直す
        let c: Config = toml::from_str("agents = []\n").expect("parse ok");
        assert!(c.agents.is_empty());
    }

    #[test]
    fn config_rejects_malformed_toml() {
        assert!(toml::from_str::<Config>("theme = ").is_err(), "値が無い");
        assert!(
            toml::from_str::<Config>("[[agents\n").is_err(),
            "括弧が閉じていない"
        );
        assert!(toml::from_str::<Config>("= \"x\"\n").is_err(), "キーが無い");
    }

    #[test]
    fn config_rejects_wrong_field_types() {
        assert!(
            toml::from_str::<Config>("editor_font_size = \"big\"\n").is_err(),
            "f32 に文字列"
        );
        assert!(
            toml::from_str::<Config>("show_hidden_files = 3\n").is_err(),
            "bool に整数"
        );
        assert!(
            toml::from_str::<Config>("theme = true\n").is_err(),
            "String に真偽値"
        );
        assert!(
            toml::from_str::<Config>("agents = \"claude\"\n").is_err(),
            "配列に文字列"
        );
        assert!(
            toml::from_str::<Config>("keybindings = 1\n").is_err(),
            "テーブルに整数"
        );
    }

    // ---- Overlay (<workspace>/.zaivern.toml) ----

    #[test]
    fn overlay_empty_is_all_none() {
        let o: Overlay = toml::from_str("").expect("空でも成立する");
        assert_eq!(o.theme, None);
        assert_eq!(o.editor_font_size, None);
        assert_eq!(o.terminal_font_size, None);
        assert_eq!(o.show_hidden_files, None);
        assert_eq!(o.approval_mode, None);
        assert_eq!(o.show_pet, None);
        assert!(o.agents.is_empty(), "overlay の agents は既定を持たない");
        assert!(o.keybindings.is_empty());
    }

    #[test]
    fn overlay_parses_only_present_fields() {
        let o: Overlay =
            toml::from_str("theme = \"zaivern-midnight\"\nshow_pet = false\n").expect("parse ok");
        assert_eq!(o.theme, Some("zaivern-midnight".to_string()));
        assert_eq!(o.show_pet, Some(false));
        assert_eq!(o.approval_mode, None, "未指定はグローバル設定を残す");
        assert_eq!(o.editor_font_size, None);
    }

    #[test]
    fn overlay_agents_are_appended_not_replaced() {
        // load() は cfg.agents.extend(o.agents) するので、overlay 側は追加分だけ
        let o: Overlay =
            toml::from_str("[[agents]]\nname = \"Proj\"\ncommand = \"make\"\n").expect("parse ok");
        assert_eq!(o.agents.len(), 1);
        assert_eq!(o.agents[0].name, "Proj");

        let mut merged = default_agents();
        let before = merged.len();
        merged.extend(o.agents);
        assert_eq!(merged.len(), before + 1);
        assert_eq!(merged.last().map(|a| a.name.as_str()), Some("Proj"));
    }

    #[test]
    fn overlay_keybindings_merge_per_key() {
        let o: Overlay =
            toml::from_str("[keybindings]\nsave = \"ctrl+s\"\nrun = \"f5\"\n").expect("parse ok");
        let mut base: HashMap<String, String> = HashMap::new();
        base.insert("save".into(), "cmd+s".into());
        base.insert("quit".into(), "cmd+q".into());
        for (k, v) in o.keybindings {
            base.insert(k, v);
        }
        assert_eq!(
            base.get("save").map(String::as_str),
            Some("ctrl+s"),
            "上書き"
        );
        assert_eq!(base.get("run").map(String::as_str), Some("f5"), "追加");
        assert_eq!(base.get("quit").map(String::as_str), Some("cmd+q"), "温存");
        assert_eq!(base.len(), 3);
    }

    #[test]
    fn overlay_rejects_wrong_types_and_malformed_toml() {
        assert!(toml::from_str::<Overlay>("show_pet = \"yes\"\n").is_err());
        assert!(toml::from_str::<Overlay>("editor_font_size = \"big\"\n").is_err());
        assert!(toml::from_str::<Overlay>("theme = \n").is_err());
    }

    #[test]
    fn overlay_ignores_fields_it_does_not_own() {
        // pet_* や voice_* はプロジェクト overlay の対象外だが、書かれていても壊れない
        let o: Overlay = toml::from_str("theme = \"x\"\nvoice_lang = \"en-US\"\npet_scale = 2.0\n")
            .expect("未知キーは無視");
        assert_eq!(o.theme, Some("x".to_string()));
    }

    // ---- UiState (~/.zaivern/state.toml) ----

    #[test]
    fn ui_state_roundtrip_preserves_values() {
        let st = UiState {
            theme: Some("zaivern-light".into()),
            approval_mode: Some("auto".into()),
            ui_zoom: Some(1.25),
            text_scale: Some(1.5),
            last_seen_version: Some("0.9.0".into()),
            show_pet: Some(false),
            word_wrap: Some(true),
            show_whitespace: Some(true),
            tab_switch_mru: Some(false),
            preview_tabs: Some(false),
            minimap: Some(true),
            breadcrumbs: Some(false),
            diff_view: Some("inline".into()),
            git_blame: Some(BlameMode::Current),
            confirm_drag_and_drop: Some(false),
            enable_trash: Some(false),
            pet_image: Some("/tmp/p.png".into()),
            pet_x: Some(10.0),
            pet_y: Some(20.5),
            pet_variant: Some("cat".into()),
            pet_scale: Some(1.4),
            pet_free_roam: Some(false),
            pet_sleep: Some(false),
            pet_sounds: Some(true),
            pet_bubbles: Some(true),
            pet_auto_yes: Some(true),
            pet_approve_keys: Some("\r".into()),
            pet_deny_keys: Some("\u{1b}".into()),
            voice_engine: Some("command".into()),
            voice_target: Some("broadcast".into()),
            voice_lang: Some("en-US".into()),
            voice_command: Some("my-stt --lang {lang}".into()),
            voice_keyword: Some("送信".into()),
            ssh_tunnel_host: Some("user@bastion.example:2222".into()),
            super_agent_command: Some("claude".into()),
            super_agent_session_title: Some("Claude Code (全自動) #3".into()),
            super_agent_enabled: Some(true),
            super_agent_timeout_secs: Some(45),
            failover_enabled: Some(true),
            quick_launch: Some(vec!["Codex".into(), "Claude Code".into()]),
            palette_recent: Some(vec![PaletteRecent {
                label: "保存".into(),
                icon: "💾".into(),
                uses: 3,
            }]),
        };
        let s = toml::to_string_pretty(&st).expect("UiState は TOML 化できる");
        let back: UiState = toml::from_str(&s).expect("読み戻せる");
        assert_eq!(back.theme, Some("zaivern-light".to_string()));
        assert_eq!(back.approval_mode, Some("auto".to_string()));
        assert_eq!(back.ui_zoom, Some(1.25));
        assert_eq!(back.text_scale, Some(1.5));
        assert_eq!(back.last_seen_version.as_deref(), Some("0.9.0"));
        assert_eq!(back.show_pet, Some(false));
        assert_eq!(back.word_wrap, Some(true));
        assert_eq!(back.show_whitespace, Some(true));
        assert_eq!(back.minimap, Some(true));
        assert_eq!(back.breadcrumbs, Some(false));
        assert_eq!(back.git_blame, Some(BlameMode::Current));
        assert_eq!(back.pet_image, Some("/tmp/p.png".to_string()));
        assert_eq!(back.pet_x, Some(10.0));
        assert_eq!(back.pet_y, Some(20.5));
        assert_eq!(back.pet_variant, Some("cat".to_string()));
        assert_eq!(back.pet_scale, Some(1.4));
        assert_eq!(back.pet_free_roam, Some(false));
        assert_eq!(back.pet_auto_yes, Some(true));
        assert_eq!(back.voice_keyword, Some("送信".to_string()));
        assert_eq!(
            back.ssh_tunnel_host,
            Some("user@bastion.example:2222".to_string()),
            "接続先は残す (鍵・パスフレーズは保存しない)"
        );
        // エスケープが必要な制御文字も往復する
        assert_eq!(back.pet_approve_keys, Some("\r".to_string()));
        assert_eq!(back.pet_deny_keys, Some("\u{1b}".to_string()));
        // 監視役 LLM の選択も state に残る (指名セッションのタイトル含む)
        assert_eq!(back.super_agent_command, Some("claude".to_string()));
        assert_eq!(
            back.super_agent_session_title,
            Some("Claude Code (全自動) #3".to_string())
        );
        assert_eq!(back.super_agent_enabled, Some(true));
        assert_eq!(back.super_agent_timeout_secs, Some(45));
        // 自動フェイルオーバーの有効/無効も state に残る
        assert_eq!(back.failover_enabled, Some(true));
        // 起動バーの割り当ては**保存した順のまま**戻る (並べ替えない)
        assert_eq!(
            back.quick_launch,
            Some(vec!["Codex".to_string(), "Claude Code".to_string()]),
        );
        // パレットの MRU も state に残る (アクションは保存しない)
        assert_eq!(
            back.palette_recent,
            Some(vec![PaletteRecent {
                label: "保存".into(),
                icon: "💾".into(),
                uses: 3,
            }])
        );
    }

    #[test]
    fn ui_state_skips_none_fields() {
        let st = UiState {
            theme: Some("zaivern-dark".into()),
            ..Default::default()
        };
        let s = toml::to_string_pretty(&st).expect("None 混じりでも TOML 化できる");
        assert!(s.contains("theme"));
        assert!(!s.contains("pet_image"), "None は書き出されない: {s}");
        let back: UiState = toml::from_str(&s).expect("読み戻せる");
        assert_eq!(back.theme, Some("zaivern-dark".to_string()));
        assert_eq!(back.pet_image, None);
    }

    #[test]
    fn ui_state_empty_toml_is_all_none() {
        let st: UiState = toml::from_str("").expect("空でも成立する");
        assert_eq!(st.theme, None);
        assert_eq!(st.approval_mode, None);
        assert_eq!(st.pet_scale, None);
        assert_eq!(st.voice_engine, None);
    }

    #[test]
    fn ui_state_rejects_wrong_types() {
        assert!(toml::from_str::<UiState>("pet_scale = \"big\"\n").is_err());
        assert!(toml::from_str::<UiState>("show_pet = 1\n").is_err());
    }

    // ---- approval_mode 正規化 (load() 末尾のロジックと同じ規則) ----

    #[test]
    fn approval_mode_normalization_rules() {
        let normalize = |m: &str| -> String {
            if m != "auto" && m != "agent" {
                "ask".to_string()
            } else {
                m.to_string()
            }
        };
        assert_eq!(normalize("auto"), "auto");
        assert_eq!(normalize("agent"), "agent");
        assert_eq!(normalize("ask"), "ask");
        assert_eq!(normalize(""), "ask", "空文字は安全側へ");
        assert_eq!(normalize("AUTO"), "ask", "大文字は認識されない (現仕様)");
        assert_eq!(normalize(" auto "), "ask", "前後の空白は許容されない");
        assert_eq!(normalize("yolo"), "ask", "未知の値は安全側へ");
    }

    // ---- パス解決 ----

    #[test]
    fn config_and_state_paths_share_zaivern_dir() {
        let c = config_path();
        let s = state_path();
        assert_eq!(c.file_name().and_then(|f| f.to_str()), Some("config.toml"));
        assert_eq!(s.file_name().and_then(|f| f.to_str()), Some("state.toml"));
        assert_eq!(c.parent(), s.parent(), "同じ ~/.zaivern に置かれる");
        assert!(c.parent().is_some_and(|p| p.ends_with(".zaivern")));
        assert!(
            c.is_absolute() || c.starts_with("."),
            "home 不明時は ./.zaivern"
        );
    }

    #[test]
    fn zaivern_dir_is_home_or_dot_fallback() {
        let d = zaivern_dir();
        assert!(d.ends_with(".zaivern"));
        match dirs::home_dir() {
            Some(h) => assert_eq!(d, h.join(".zaivern")),
            None => assert_eq!(d, PathBuf::from(".").join(".zaivern")),
        }
    }

    #[test]
    fn overlay_path_is_workspace_local() {
        let ws = Path::new("/tmp/some-workspace");
        let p: PathBuf = ws.join(".zaivern.toml");
        assert_eq!(p, PathBuf::from("/tmp/some-workspace/.zaivern.toml"));
        assert!(p.starts_with(ws));
    }
}

#[cfg(test)]
mod supervisor_field_tests {
    use super::*;

    /// `[supervisor]` セクションが無い既存の config.toml が、
    /// これまでどおり読めて既定値が入ることを確かめる。
    #[test]
    fn config_without_supervisor_section_still_loads() {
        assert!(
            !DEFAULT_CONFIG.contains("[supervisor]"),
            "この検証は [supervisor] を書いていない設定を前提にしている"
        );
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("既定の設定が読めなくなった");
        assert_eq!(cfg.theme, "zaivern-dark");
        // プリセット件数はテンプレートと default_agents() で必ず一致する
        // (`default_config_template_agents_match_default_agents` が担保)
        assert_eq!(cfg.agents.len(), default_agents().len());
        // supervisor は SupervisorConfig の既定値で埋まる
        let d = crate::supervisor::SupervisorConfig::default();
        assert_eq!(cfg.supervisor.enabled, d.enabled);
        assert_eq!(cfg.supervisor.sample_interval_ms, d.sample_interval_ms);
        assert_eq!(cfg.supervisor.allow_auto_restart, d.allow_auto_restart);
    }

    /// 手元の `~/.zaivern/config.toml` があるなら、それも読めることを確かめる。
    /// 無い環境では何もしない (CI で落とさない)。
    #[test]
    fn existing_user_config_still_loads() {
        let Ok(s) = std::fs::read_to_string(config_path()) else {
            return;
        };
        let cfg: Config = toml::from_str(&s).expect("既存の config.toml が読めなくなった");
        assert!(!cfg.theme.is_empty());
        // 新しく生えた [super_agent] を書いていない既存ファイルでも既定値で埋まる
        assert_eq!(cfg.super_agent, SuperAgentConfig::default());
    }
}

#[cfg(test)]
mod failover_field_tests {
    use super::*;

    /// `[failover]` を書いていない設定は **必ず無効** で読み込まれる。
    /// (勝手に別アカウントへ移る = 課金先が変わる。既定で入ってはいけない)
    #[test]
    fn failoverセクションが無ければ既定で無効() {
        assert!(
            !DEFAULT_CONFIG.contains("\n[failover]"),
            "テンプレートは [failover] をコメントのままにしておく (既定は無効)"
        );
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("既定の設定が読める");
        assert_eq!(cfg.failover, crate::failover::FailoverConfig::default());
        assert!(!cfg.failover.enabled, "既定は必ず無効");
    }

    /// テンプレートに書いた `[failover]` の例 (コメントを外した形) が実際に読める。
    /// 書き方の見本がそのままでは動かない、を防ぐ。
    #[test]
    fn テンプレートのfailover例はコメントを外せば読める() {
        let uncommented: String = DEFAULT_CONFIG
            .lines()
            .skip_while(|l| !l.starts_with("# [failover]"))
            .take_while(|l| l.starts_with('#'))
            .map(|l| l.trim_start_matches("# ").trim_start_matches('#'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            uncommented.starts_with("[failover]"),
            "テンプレートに [failover] の例が無い: {uncommented:?}"
        );
        let cfg: Config = toml::from_str(&uncommented).expect("例がそのまま読める");
        assert!(!cfg.failover.enabled, "例の既定値も無効のまま");
        assert_eq!(cfg.failover.max_switches, 3);
        assert_eq!(cfg.failover.min_screen_hits, 2);
        // 有効化した形も読める。
        let on = uncommented.replace("enabled = false", "enabled = true");
        let cfg: Config = toml::from_str(&on).expect("有効化した形も読める");
        assert!(cfg.failover.enabled);
    }
}

#[cfg(test)]
mod super_agent_field_tests {
    use super::*;

    /// DoD: `[super_agent]` セクションが無い既存の config.toml が、
    /// これまでどおり読めて「なし」の既定値が入ること。
    #[test]
    fn super_agentセクションが無い設定も読める() {
        assert!(
            !DEFAULT_CONFIG.contains("[super_agent]"),
            "この検証は [super_agent] を書いていない設定を前提にしている"
        );
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("既定の設定が読めなくなった");
        assert_eq!(cfg.super_agent.command, "");
        assert!(!cfg.super_agent.enabled);
        assert_eq!(cfg.super_agent.timeout_secs, 60);
        assert_eq!(cfg.super_agent.active_command(), None);
    }

    /// 何も書かれていない TOML でも既定値どおりに読める。
    #[test]
    fn 空のtomlでも既定の監視役はなし() {
        let cfg: Config = toml::from_str("").expect("空の TOML");
        assert_eq!(cfg.super_agent, SuperAgentConfig::default());
    }

    /// 部分指定 (コマンドだけ) でも他は既定値のまま。
    #[test]
    fn 監視役の部分指定が読める() {
        let cfg: Config =
            toml::from_str("[super_agent]\ncommand = \"claude\"\nenabled = true\n").expect("TOML");
        assert_eq!(cfg.super_agent.command, "claude");
        assert!(cfg.super_agent.enabled);
        assert_eq!(cfg.super_agent.timeout_secs, 60);
        assert_eq!(cfg.super_agent.active_command(), Some("claude"));
    }

    /// DoD: 有効フラグだけ立っていてコマンドが空なら、監視役は居ない扱い。
    /// ここを取り違えると「誰も選ばれていないのに相談モードが ON」になる。
    #[test]
    fn コマンドが空なら有効フラグだけでは動かない() {
        let c = SuperAgentConfig {
            command: "   ".into(),
            enabled: true,
            timeout_secs: 30,
            ..Default::default()
        };
        assert_eq!(c.active_command(), None);
    }

    /// 無効化されていれば、コマンドが入っていても動かない。
    #[test]
    fn 無効化されていれば監視役は居ない() {
        let c = SuperAgentConfig {
            command: "claude".into(),
            enabled: false,
            timeout_secs: 30,
            ..Default::default()
        };
        assert_eq!(c.active_command(), None);
    }

    /// 前後の空白は落として渡す (診断側のカタログ照合が空白で失敗しないように)。
    #[test]
    fn コマンドの前後空白は落ちる() {
        let c = SuperAgentConfig {
            command: "  codex  ".into(),
            enabled: true,
            timeout_secs: 30,
            ..Default::default()
        };
        assert_eq!(c.active_command(), Some("codex"));
    }
}

#[cfg(test)]
mod plugins_config_tests {
    use super::*;

    #[test]
    fn 未記載のプラグインは有効() {
        let p = PluginsConfig::default();
        assert!(p.is_enabled("worktrees"));
    }

    #[test]
    fn 無効化と再有効化が往復する() {
        let mut p = PluginsConfig::default();
        p.set_enabled("worktrees", false);
        assert!(!p.is_enabled("worktrees"));
        assert_eq!(p.disabled, vec!["worktrees".to_string()]);

        // 二重に無効化しても重複しない
        p.set_enabled("worktrees", false);
        assert_eq!(p.disabled.len(), 1);

        p.set_enabled("worktrees", true);
        assert!(p.is_enabled("worktrees"));
        assert!(p.disabled.is_empty());
    }

    #[test]
    fn 設定値の読み書き() {
        let mut p = PluginsConfig::default();
        assert_eq!(p.setting("remote-host", "host"), None);
        p.set_setting("remote-host", "host", "user@example.com");
        assert_eq!(p.setting("remote-host", "host"), Some("user@example.com"));
        // 別キーを足しても既存キーは残る
        p.set_setting("remote-host", "remote_path", "/srv/work");
        assert_eq!(p.setting("remote-host", "host"), Some("user@example.com"));
        assert_eq!(p.setting("remote-host", "remote_path"), Some("/srv/work"));
        // 未知のプラグインは None
        assert_eq!(p.setting("nope", "host"), None);
    }

    #[test]
    fn toml_を往復できる() {
        let mut p = PluginsConfig::default();
        p.set_enabled("usage-meter", false);
        p.set_setting("worktrees", "parallel_count", "5");
        let s = toml::to_string_pretty(&p).expect("serialize");
        let back: PluginsConfig = toml::from_str(&s).expect("deserialize");
        assert!(!back.is_enabled("usage-meter"));
        assert_eq!(back.setting("worktrees", "parallel_count"), Some("5"));
    }

    #[test]
    fn plugins_セクションを省略した設定も読める() {
        // 既存ユーザーの config.toml には [plugins] が無い
        let cfg: Config = toml::from_str("theme = \"dark\"\n").expect("parse");
        assert!(cfg.plugins.disabled.is_empty());
        assert!(cfg.plugins.is_enabled("worktrees"));
    }

    #[test]
    fn plugins区画の書き換えでコメントが残る() {
        let raw = "# 大事なメモ\ntheme = \"dark\"\n\n[plugins]\ndisabled = [\"old\"]\n\n[plugins.settings.foo]\na = \"1\"\n\n[keybindings]\nsave = \"cmd+s\"\n";
        let mut p = PluginsConfig::default();
        p.set_enabled("new-one", false);
        p.set_setting("bar", "host", "example.com");
        let out = rewrite_plugins_section(raw, &p);

        assert!(out.contains("# 大事なメモ"), "区画外のコメントが消えた");
        assert!(out.contains("save = \"cmd+s\""), "他セクションが消えた");
        assert!(!out.contains("\"old\""), "古い disabled が残っている");
        assert!(
            !out.contains("[plugins.settings.foo]"),
            "古い設定テーブルが残っている"
        );
        assert!(out.contains("[plugins.settings.bar]"));

        let back: Config = toml::from_str(&out).expect("書き戻した config.toml が壊れている");
        assert!(!back.plugins.is_enabled("new-one"));
        assert_eq!(back.plugins.setting("bar", "host"), Some("example.com"));
    }

    // ── [keybindings] 区画の書き戻し (GUI のキーバインド編集) ──────────

    #[test]
    fn keybindings区画の書き換えでコメントと他の設定が残る() {
        let raw = concat!(
            "# 大事なメモ\n",
            "theme = \"dark\"\n",
            "\n",
            "[keybindings]\n",
            "# 手書きの覚え書き\n",
            "save = \"cmd+s\"\n",
            "\n",
            "[plugins]\n",
            "disabled = [\"old\"]\n",
        );
        let mut ov = HashMap::new();
        ov.insert("save".to_string(), "cmd+alt+s".to_string());
        ov.insert("keybind_editor".to_string(), "cmd+k cmd+s".to_string());
        let out = rewrite_keybindings_section(raw, &ov);

        assert!(out.contains("# 大事なメモ"), "区画外のコメントが消えた");
        assert!(out.contains("theme = \"dark\""), "他のキーが消えた");
        assert!(out.contains("[plugins]"), "他のセクションが消えた");
        assert!(
            out.contains("disabled = [\"old\"]"),
            "他セクションの中身が消えた"
        );
        assert!(
            !out.contains("# 手書きの覚え書き"),
            "古い区画の中身が残っている"
        );
        assert!(!out.contains("\"cmd+s\""), "古い割り当てが残っている");

        let back: Config = toml::from_str(&out).expect("書き戻した config.toml が壊れている");
        assert_eq!(back.theme, "dark");
        assert_eq!(
            back.keybindings.get("save").map(String::as_str),
            Some("cmd+alt+s")
        );
        assert_eq!(
            back.keybindings.get("keybind_editor").map(String::as_str),
            Some("cmd+k cmd+s")
        );
        assert_eq!(back.keybindings.len(), 2);
        assert!(
            !back.plugins.is_enabled("old"),
            "plugins 区画の意味が変わった"
        );
    }

    #[test]
    fn keybindingsが空なら区画ごと消える() {
        let raw = "theme = \"dark\"\n\n[keybindings]\nsave = \"cmd+s\"\n";
        let out = rewrite_keybindings_section(raw, &HashMap::new());
        assert!(!out.contains("[keybindings]"), "空なら区画ごと消えるべき");
        assert!(out.contains("theme"));
        let back: Config = toml::from_str(&out).expect("parse");
        assert!(back.keybindings.is_empty());
    }

    #[test]
    fn keybindings区画が無くても追記できる() {
        let mut ov = HashMap::new();
        ov.insert("find".to_string(), "ctrl+f".to_string());
        let out = rewrite_keybindings_section("theme = \"dark\"\n", &ov);
        let back: Config = toml::from_str(&out).expect("parse");
        assert_eq!(
            back.keybindings.get("find").map(String::as_str),
            Some("ctrl+f")
        );
        assert_eq!(back.theme, "dark");
    }

    #[test]
    fn keybindings_コメントアウトされた見出しは区画扱いしない() {
        // 既定テンプレートは "# [keybindings]" を含む。本物の見出しと
        // 誤認すると、以降の行が丸ごと消えてしまう。
        let out = rewrite_keybindings_section(DEFAULT_CONFIG, &HashMap::new());
        assert!(out.contains("# [keybindings]"), "コメント行が消えた");
        assert!(out.contains("[[agents]]"), "エージェント定義が消えた");
        let back: Config = toml::from_str(&out).expect("既定テンプレートが壊れた");
        assert!(!back.agents.is_empty());
    }

    #[test]
    fn keybindings区画は書くたびに同じ並びになる() {
        // HashMap の順序は不定。書くたびに差分が出ると git が汚れる。
        let mut ov = HashMap::new();
        for (k, v) in [
            ("zoom_in", "cmd+plus"),
            ("find", "ctrl+f"),
            ("save", "cmd+k cmd+s"),
            ("new_file", "alt+n"),
        ] {
            ov.insert(k.to_string(), v.to_string());
        }
        let a = rewrite_keybindings_section("theme = \"dark\"\n", &ov);
        let b = rewrite_keybindings_section("theme = \"dark\"\n", &ov);
        assert_eq!(a, b);
        let names: Vec<&str> = a
            .lines()
            .skip_while(|l| l.trim() != "[keybindings]")
            .skip(1)
            .filter_map(|l| l.split(" = ").next())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "名前順に並んでいない: {names:?}");
    }

    #[test]
    fn 書き戻した区画をkeybindsが読み直せる() {
        // config.rs の書き戻し → keybinds の読み込み、の一周を固定する。
        let mut kb = crate::keybinds::Keybinds::default();
        kb.set(
            crate::keybinds::BindAction::Save,
            crate::keybinds::Binding::Chord(
                egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::K),
                egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::W),
            ),
        );
        let out = rewrite_keybindings_section("# メモ\ntheme = \"dark\"\n", &kb.overrides());
        assert!(out.contains("# メモ"));
        let back: Config = toml::from_str(&out).expect("parse");
        let kb2 = crate::keybinds::Keybinds::from_overrides(&back.keybindings);
        assert_eq!(
            kb2.binding(crate::keybinds::BindAction::Save),
            kb.binding(crate::keybinds::BindAction::Save)
        );
    }

    #[test]
    fn 設定が空なら区画を書かない() {
        let raw = "theme = \"dark\"\n\n[plugins]\ndisabled = [\"x\"]\n";
        let out = rewrite_plugins_section(raw, &PluginsConfig::default());
        assert!(!out.contains("[plugins]"), "空なら区画ごと消えるべき");
        assert!(out.contains("theme"));
        let back: Config = toml::from_str(&out).expect("parse");
        assert!(back.plugins.is_enabled("x"));
    }

    #[test]
    fn 既存区画が無くても追記できる() {
        let out = rewrite_plugins_section("theme = \"dark\"\n", &{
            let mut p = PluginsConfig::default();
            p.set_enabled("z", false);
            p
        });
        let back: Config = toml::from_str(&out).expect("parse");
        assert!(!back.plugins.is_enabled("z"));
    }

    #[test]
    fn コメントアウトされた見出しは区画扱いしない() {
        // 既定テンプレートは "# [plugins]" を含む。これを本物の見出しと
        // 誤認すると、以降の行が丸ごと消えてしまう。
        let out = rewrite_plugins_section(DEFAULT_CONFIG, &PluginsConfig::default());
        assert!(out.contains("# [plugins]"), "コメント行が消えた");
        assert!(out.contains("[[agents]]"), "エージェント定義が消えた");
        let back: Config = toml::from_str(&out).expect("既定テンプレートが壊れた");
        assert!(!back.agents.is_empty());
    }

    #[test]
    fn 既定テンプレートがそのまま読める() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("既定 config.toml が壊れている");
        assert!(cfg.plugins.is_enabled("worktrees"));
        assert!(!cfg.agents.is_empty());
    }

    // ---- 自動YESのユーザー定義ルール ([[auto_yes_rules]]) ----

    #[test]
    fn 自動yesルールが書いて読んで往復する() {
        let src = r#"
theme = "dark"

[[auto_yes_rules]]
pattern = "Allow access to this file?"
reply = "\r"
agent = "agy"

[[auto_yes_rules]]
pattern = "Continue with this plan?"
reply = "y\r"
"#;
        let cfg: Config = toml::from_str(src).expect("auto_yes_rules が読めない");
        assert_eq!(cfg.auto_yes_rules.len(), 2);
        assert_eq!(cfg.auto_yes_rules[0].pattern, "Allow access to this file?");
        assert_eq!(cfg.auto_yes_rules[0].reply, "\r", "TOML のエスケープが効く");
        assert_eq!(cfg.auto_yes_rules[0].agent, "agy");
        // agent 省略時は「全エージェント」を表す空文字。
        assert_eq!(cfg.auto_yes_rules[1].reply, "y\r");
        assert_eq!(cfg.auto_yes_rules[1].agent, "");

        // 書き戻して読み直しても同じ (シリアライズ側の欠落よけ)。
        let out = toml::to_string(&cfg).expect("書き戻せない");
        let back: Config = toml::from_str(&out).expect("書き戻したものが読めない");
        assert_eq!(back.auto_yes_rules, cfg.auto_yes_rules);
    }

    #[test]
    fn キーの無い古い設定ファイルもそのまま読める() {
        // 既存ユーザーの config.toml には auto_yes_rules が無い。
        // 追加したキーのせいで設定全体が既定へ戻る事故を防ぐ。
        let old = "theme = \"dark\"\npet_auto_yes = true\n";
        let cfg: Config = toml::from_str(old).expect("古い設定が読めなくなった");
        assert_eq!(cfg.theme, "dark");
        assert!(cfg.pet_auto_yes, "既存のキーが読めている");
        assert!(
            cfg.auto_yes_rules.is_empty(),
            "未指定なら空 (既定の表だけ使う)"
        );
    }

    #[test]
    fn 既定テンプレートの自動yesルール例はコメントアウトされている() {
        // 例をそのまま有効にすると、書いた覚えの無いルールが効いてしまう。
        assert!(
            DEFAULT_CONFIG.contains("# [[auto_yes_rules]]"),
            "DEFAULT_CONFIG に auto_yes_rules の記入例が無い"
        );
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("既定 config.toml が壊れている");
        assert!(
            cfg.auto_yes_rules.is_empty(),
            "記入例が有効になっている (コメントアウトのはず)"
        );
    }

    #[test]
    fn 番号入力メニューのユーザールールが書ける() {
        // 「アンケートを数字で入力しないと進まない」画面の選び方を、
        // 再ビルド無しで config.toml から上書きできること。
        let src = r#"
theme = "dark"

[[auto_yes_rules]]
pattern = "How would you rate"
reply = "3\r"
"#;
        let cfg: Config = toml::from_str(src).expect("番号メニューのルールが読めない");
        assert_eq!(cfg.auto_yes_rules.len(), 1);
        assert_eq!(
            cfg.auto_yes_rules[0].reply, "3\r",
            "番号 + Enter が書けない"
        );
        assert!(
            cfg.auto_yes_rules[0].agent.is_empty(),
            "省略時は全エージェント"
        );
        // 記入例が既定 config.toml に載っていること (ユーザーが真似できる)
        assert!(
            DEFAULT_CONFIG.contains("reply = \"3\\r\""),
            "DEFAULT_CONFIG に番号入力メニューの記入例が無い"
        );
    }

    // ---- 承認ポリシー ([[approval_policies]]) ----

    #[test]
    fn 承認ポリシーが書いて読んで往復する() {
        use crate::agents::approvals::{ApprovalKind, Decision, Scope};
        let src = r#"
theme = "dark"

[[approval_policies]]
kind = "file_read"
scope = "agent"
target = "claude"
decision = "allow_always"

[[approval_policies]]
kind = "network_access"
decision = "deny_always"

[[approval_policies]]
kind = "file_write"
scope = "path"
target = "/repo/src"
decision = "allow_once"
"#;
        let cfg: Config = toml::from_str(src).expect("approval_policies が読めない");
        assert_eq!(cfg.approval_policies.len(), 3);
        // scope 省略時は既定の "global"。
        assert_eq!(cfg.approval_policies[1].scope, "global");

        let ps = approval_policies_from_config(&cfg);
        assert_eq!(ps.len(), 3);
        assert_eq!(ps[0].kind, ApprovalKind::FileRead);
        assert_eq!(ps[0].scope, Scope::Agent("claude".into()));
        assert_eq!(ps[0].decision, Decision::AllowAlways);
        assert_eq!(ps[1].scope, Scope::Global);
        assert_eq!(ps[1].decision, Decision::DenyAlways);
        assert_eq!(ps[2].scope, Scope::PathPrefix("/repo/src".into()));
        assert_eq!(ps[2].decision, Decision::AllowOnce);

        // 書き戻して読み直しても同じ (シリアライズ側の欠落よけ)。
        let out = toml::to_string(&cfg).expect("書き戻せない");
        let back: Config = toml::from_str(&out).expect("書き戻したものが読めない");
        assert_eq!(back.approval_policies, cfg.approval_policies);
        assert_eq!(approval_policies_from_config(&back), ps);
    }

    #[test]
    fn 未知の値の承認ポリシー行は捨てる() {
        // 新しい Zaivern が書いた種別を古いバイナリで読んだときに、
        // 近い種別へ丸めて自動承認してしまう事故を防ぐ。
        let src = r#"
[[approval_policies]]
kind = "quantum_teleport"
decision = "allow_always"

[[approval_policies]]
kind = "file_read"
scope = "brainwave"
decision = "allow_always"

[[approval_policies]]
kind = "file_read"
decision = "yolo"

[[approval_policies]]
kind = "file_read"
decision = "allow_always"
"#;
        let cfg: Config = toml::from_str(src).expect("読めない");
        assert_eq!(cfg.approval_policies.len(), 4);
        let ps = approval_policies_from_config(&cfg);
        assert_eq!(ps.len(), 1, "妥当な 1 行だけ残るはず");
        assert_eq!(ps[0].kind, crate::agents::approvals::ApprovalKind::FileRead);
    }

    #[test]
    fn 承認ポリシーキーの無い古い設定ファイルもそのまま読める() {
        let old = "theme = \"dark\"\npet_auto_yes = true\n";
        let cfg: Config = toml::from_str(old).expect("古い設定が読めなくなった");
        assert_eq!(cfg.theme, "dark");
        assert!(cfg.pet_auto_yes);
        assert!(
            cfg.approval_policies.is_empty(),
            "未指定なら空 (= 従来どおり全部ユーザーに聞く)"
        );
        assert!(approval_policies_from_config(&cfg).is_empty());
    }

    #[test]
    fn 既定テンプレートの承認ポリシー例はコメントアウトされている() {
        assert!(
            DEFAULT_CONFIG.contains("# [[approval_policies]]"),
            "DEFAULT_CONFIG に approval_policies の記入例が無い"
        );
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("既定 config.toml が壊れている");
        assert!(
            cfg.approval_policies.is_empty(),
            "記入例が有効になっている (コメントアウトのはず)"
        );
    }
}

#[cfg(test)]
mod state_overlay_tests {
    use super::*;

    // プロジェクト overlay とグローバル state.toml の分離。
    // 実ユーザーの ~/.zaivern に触れないよう、実体の load_from_dir() /
    // save_state_to_dir() に一時ディレクトリを差し込んで検証する。

    #[test]
    fn プロジェクトoverlayの値はsave_stateでグローバルに漏れない() {
        let home = crate::test_util::unique_temp_dir("zaivern-config-test", "no-leak-home");
        let root = crate::test_util::unique_temp_dir("zaivern-config-test", "no-leak-root");
        std::fs::write(
            home.join("state.toml"),
            "theme = \"global-theme\"\napproval_mode = \"auto\"\nshow_pet = false\n",
        )
        .expect("write state.toml");
        std::fs::write(
            root.join(".zaivern.toml"),
            "theme = \"project-theme\"\napproval_mode = \"agent\"\nshow_pet = true\n",
        )
        .expect("write .zaivern.toml");

        let cfg = load_from_dir(&home, std::slice::from_ref(&root), true);
        // セッション中はプロジェクトの値が効く
        assert_eq!(cfg.theme, "project-theme");
        assert_eq!(cfg.approval_mode, "agent");
        assert!(cfg.show_pet);

        // プロジェクトを開いただけで保存されても、グローバルは壊れない
        save_state_to_dir(&home, &cfg);
        let raw = std::fs::read_to_string(home.join("state.toml")).expect("re-read state.toml");
        let st: UiState = toml::from_str(&raw).expect("parse state.toml");
        assert_eq!(st.theme.as_deref(), Some("global-theme"));
        assert_eq!(st.approval_mode.as_deref(), Some("auto"));
        assert_eq!(st.show_pet, Some(false));

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 折り返しと空白表示はstateへ永続化される() {
        let home = crate::test_util::unique_temp_dir("zaivern-config-test", "wrap-ws-home");
        let mut cfg = Config::default();
        assert!(!cfg.word_wrap && !cfg.show_whitespace, "既定はどちらもオフ");

        // UI からの切替相当 (Cmd::ToggleWordWrap 等): 控えも一緒に更新する
        cfg.word_wrap = true;
        cfg.global_word_wrap = true;
        cfg.show_whitespace = true;
        cfg.global_show_whitespace = true;
        save_state_to_dir(&home, &cfg);

        let loaded = load_from_dir(&home, &[], true);
        assert!(loaded.word_wrap, "折り返しが state.toml から復元される");
        assert!(
            loaded.show_whitespace,
            "空白表示が state.toml から復元される"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn パレットのmruは保存と読み込みで順序が保たれる() {
        let home = crate::test_util::unique_temp_dir("zaivern-config-test", "palette-mru");
        let mut cfg = Config::default();
        assert!(cfg.palette_recent.is_empty(), "既定は空");
        cfg.palette_recent = vec![
            PaletteRecent {
                label: "ターミナル表示切替".into(),
                icon: "🖥".into(),
                uses: 5,
            },
            PaletteRecent {
                label: "保存".into(),
                icon: "💾".into(),
                uses: 2,
            },
            PaletteRecent {
                label: "絵文字🎨と日本語".into(),
                icon: String::new(),
                uses: 1,
            },
        ];
        save_state_to_dir(&home, &cfg);
        let loaded = load_from_dir(&home, &[], true);
        assert_eq!(
            loaded.palette_recent, cfg.palette_recent,
            "MRU が往復で変わった (順序・回数・アイコン)"
        );

        // 別の UI 操作 (テーマ変更) で save_state しても MRU は消えない
        let mut next = loaded;
        next.global_theme = "zaivern-light".into();
        save_state_to_dir(&home, &next);
        let again = load_from_dir(&home, &[], true);
        assert_eq!(again.palette_recent, cfg.palette_recent, "MRU が消えた");
        assert_eq!(again.theme, "zaivern-light");

        // config.toml (手書き) には書かない = state.toml 側だけに現れる
        let state = std::fs::read_to_string(home.join("state.toml")).expect("state.toml");
        assert!(state.contains("palette_recent"), "state.toml に無い");

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn 壊れたstateファイルでもmruはパニックせず既定に戻る() {
        for (tag, body) in [
            ("broken-toml", "palette_recent = [[[\n"),
            ("wrong-type", "palette_recent = 42\n"),
            (
                "missing-fields",
                "theme = \"zaivern-dark\"\n[[palette_recent]]\n",
            ),
            ("empty", ""),
        ] {
            let home = crate::test_util::unique_temp_dir("zaivern-config-test", tag);
            std::fs::write(home.join("state.toml"), body).expect("write state.toml");
            // 壊れていても load は成立し、MRU は既定 (空 or 既定値) に落ちる
            let cfg = load_from_dir(&home, &[], true);
            for r in &cfg.palette_recent {
                // 欠けたフィールドは serde の default (空文字 / 0) で埋まる
                assert!(r.uses < u32::MAX, "tag={tag}");
            }
            if tag != "missing-fields" {
                assert!(cfg.palette_recent.is_empty(), "tag={tag} は空に戻るはず");
            }
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    // ── 起動バー (⌃1〜⌃9) の割り当て ────────────────────────────
    #[test]
    fn 起動バーの割り当ては保存と読み込みで順序が保たれる() {
        let home = crate::test_util::unique_temp_dir("zaivern-config-test", "quick-launch");
        let mut cfg = Config::default();
        // 既定は「まだ決めていない」= プリセットの並びの先頭から
        assert!(cfg.quick_launch.is_none(), "既定は None (プリセットの並び)");
        let names: Vec<String> = cfg
            .agents
            .iter()
            .rev()
            .take(4)
            .map(|p| p.name.clone())
            .collect();
        let slots_before = quick_launch_slots(&cfg.agents, Some(&names));
        cfg.quick_launch = Some(names.clone());
        save_state_to_dir(&home, &cfg);

        let loaded = load_from_dir(&home, &[], true);
        assert_eq!(
            loaded.quick_launch.as_deref(),
            Some(names.as_slice()),
            "保存した並びがそのまま戻らない"
        );
        assert_eq!(
            quick_launch_slots(&loaded.agents, loaded.quick_launch.as_deref()),
            slots_before,
            "読み直しで番号が動いた"
        );

        // もう一度読み書きしても 1 つも動かない (再起動の繰り返しに耐える)
        save_state_to_dir(&home, &loaded);
        let again = load_from_dir(&home, &[], true);
        assert_eq!(
            again.quick_launch, loaded.quick_launch,
            "2 周目で並びが変わった"
        );

        // 「全部外した」も意味を持つ状態として往復する (既定へ勝手に戻さない)
        let mut empty = again;
        empty.quick_launch = Some(Vec::new());
        save_state_to_dir(&home, &empty);
        let back = load_from_dir(&home, &[], true);
        assert_eq!(
            back.quick_launch,
            Some(Vec::new()),
            "空の割り当てが復元されない"
        );
        assert!(
            quick_launch_slots(&back.agents, back.quick_launch.as_deref()).is_empty(),
            "空なのに起動バーが出てしまう"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn 壊れた起動バー設定でもパニックしない() {
        for (tag, body) in [
            ("broken-toml", "quick_launch = [[[\n"),
            ("wrong-type", "quick_launch = 42\n"),
            ("wrong-elem", "quick_launch = [1, 2, 3]\n"),
            (
                "unknown-names",
                "quick_launch = [\"居ないプリセット\", \"\"]\n",
            ),
            ("empty", ""),
        ] {
            let home = crate::test_util::unique_temp_dir("zaivern-config-test", tag);
            std::fs::write(home.join("state.toml"), body).expect("write state.toml");
            let cfg = load_from_dir(&home, &[], true);
            // 壊れていても落ちず、番号の解決も落ちない
            let slots = quick_launch_slots(&cfg.agents, cfg.quick_launch.as_deref());
            assert!(slots.len() <= QUICK_LAUNCH_SLOTS, "tag={tag}");
            if tag == "unknown-names" {
                assert!(slots.is_empty(), "tag={tag}: 居ない名前が番号を取っている");
            }
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn セッション自動命名の既定はオフ() {
        assert!(
            !Config::default().auto_name_sessions,
            "既定でオンにすると、起動しただけで外部プロセスが走る"
        );
        let t: Config = toml::from_str(DEFAULT_CONFIG).expect("同梱テンプレはパースできる");
        assert!(
            !t.auto_name_sessions,
            "同梱テンプレの既定がオンになっている"
        );
        // 設定 GUI から切り替えられる (到達経路がある)
        assert!(
            setting_defs().iter().any(|d| d.key == "auto_name_sessions"),
            "設定 GUI に項目が無い"
        );
        let mut cfg = Config::default();
        assert!(set_setting_value(
            &mut cfg,
            "auto_name_sessions",
            &SettingValue::Bool(true)
        ));
        assert!(cfg.auto_name_sessions);
    }

    #[test]
    fn ミニマップとブレッドクラムの既定と永続化() {
        let home = crate::test_util::unique_temp_dir("zaivern-config-test", "mm-bc-home");
        let root = crate::test_util::unique_temp_dir("zaivern-config-test", "mm-bc-root");
        let cfg = Config::default();
        // 既定: ミニマップはオフ (本文の幅を奪うので使う人だけが払う)
        //       ブレッドクラムはオン (高さ 1 行・LSP 無しでも必ず出せる)
        assert!(!cfg.minimap, "ミニマップの既定はオフ");
        assert!(cfg.breadcrumbs, "ブレッドクラムの既定はオン");

        // UI からの切替相当: 控えも一緒に更新する
        let mut cfg = cfg;
        cfg.minimap = true;
        cfg.global_minimap = true;
        cfg.breadcrumbs = false;
        cfg.global_breadcrumbs = false;
        save_state_to_dir(&home, &cfg);
        let loaded = load_from_dir(&home, &[], true);
        assert!(loaded.minimap, "ミニマップが state.toml から復元される");
        assert!(
            !loaded.breadcrumbs,
            "ブレッドクラムが state.toml から復元される"
        );

        // プロジェクト overlay で上書きでき、グローバルの控えへは漏れない
        std::fs::write(
            root.join(".zaivern.toml"),
            "minimap = false\nbreadcrumbs = true\n",
        )
        .expect("write .zaivern.toml");
        let cfg = load_from_dir(&home, std::slice::from_ref(&root), true);
        assert!(!cfg.minimap && cfg.breadcrumbs, "プロジェクト値が効く");
        assert!(
            cfg.global_minimap && !cfg.global_breadcrumbs,
            "控えは overlay 前のまま"
        );

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 折り返しはプロジェクトoverlayでも上書きできる() {
        let home = crate::test_util::unique_temp_dir("zaivern-config-test", "wrap-ov-home");
        let root = crate::test_util::unique_temp_dir("zaivern-config-test", "wrap-ov-root");
        std::fs::write(
            root.join(".zaivern.toml"),
            "word_wrap = true\nshow_whitespace = true\n",
        )
        .expect("write .zaivern.toml");

        let cfg = load_from_dir(&home, std::slice::from_ref(&root), true);
        assert!(cfg.word_wrap && cfg.show_whitespace, "プロジェクト値が効く");
        // グローバルの控えは overlay 適用前の値のまま → state.toml へ漏れない
        assert!(!cfg.global_word_wrap && !cfg.global_show_whitespace);
        save_state_to_dir(&home, &cfg);
        let raw = std::fs::read_to_string(home.join("state.toml")).expect("re-read");
        let st: UiState = toml::from_str(&raw).expect("parse");
        assert_eq!(st.word_wrap, Some(false));
        assert_eq!(st.show_whitespace, Some(false));

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ユーザーが変えた値はoverlay下でも永続化される() {
        let home = crate::test_util::unique_temp_dir("zaivern-config-test", "persist-home");
        let root = crate::test_util::unique_temp_dir("zaivern-config-test", "persist-root");
        std::fs::write(home.join("state.toml"), "theme = \"global-theme\"\n")
            .expect("write state.toml");
        std::fs::write(root.join(".zaivern.toml"), "theme = \"project-theme\"\n")
            .expect("write .zaivern.toml");

        let mut cfg = load_from_dir(&home, std::slice::from_ref(&root), true);
        // UI からの変更相当 (Cmd::SetTheme / Cmd::TogglePet): 控えも一緒に更新する
        cfg.theme = "user-picked".into();
        cfg.global_theme = "user-picked".into();
        cfg.show_pet = false;
        cfg.global_show_pet = false;

        save_state_to_dir(&home, &cfg);
        let raw = std::fs::read_to_string(home.join("state.toml")).expect("re-read state.toml");
        let st: UiState = toml::from_str(&raw).expect("parse state.toml");
        assert_eq!(st.theme.as_deref(), Some("user-picked"));
        assert_eq!(st.show_pet, Some(false));

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// 単価表 (`[pricing]`) — quota の推定コストが引く側。
#[cfg(test)]
mod quota_pricing_tests {
    use super::*;
    use crate::coordinator::quota::PriceLookup;

    /// 既定の表は「入力 < 出力」「キャッシュ読 < 入力 < キャッシュ書」を守る。
    /// 値そのものは変わりうるが、この大小関係が壊れたら表の書き間違い。
    #[test]
    fn 既定の単価表の大小関係が正しい() {
        let p = PricingConfig::default();
        assert!(p.enabled);
        assert!(
            !p.models.is_empty(),
            "既定値が空だと推定が全部「不明」になる"
        );
        for (name, m) in &p.models {
            assert!(m.input > 0.0, "{name}: 入力単価が 0");
            assert!(m.output > m.input, "{name}: 出力が入力より安い");
            assert!(
                m.cache_write > m.input,
                "{name}: キャッシュ書が入力より安い"
            );
            assert!(m.cache_read < m.input, "{name}: キャッシュ読が入力より高い");
        }
    }

    /// 完全一致 → 最長の前方一致 の順で引く。
    #[test]
    fn 単価は完全一致と最長前方一致で引く() {
        let mut p = PricingConfig {
            enabled: true,
            currency: "$".into(),
            models: HashMap::new(),
        };
        let r = |i: f64| ModelPrice {
            input: i,
            output: i * 5.0,
            cache_write: i * 1.25,
            cache_read: i * 0.1,
        };
        p.models.insert("mdl".into(), r(1.0));
        p.models.insert("mdl-pro".into(), r(9.0));
        // 完全一致
        assert_eq!(p.lookup("mdl").unwrap().input, 1.0);
        assert_eq!(p.lookup("mdl-pro").unwrap().input, 9.0);
        // 最長の前方一致が勝つ (短い "mdl" に吸われない)
        assert_eq!(p.lookup("mdl-pro-20260809").unwrap().input, 9.0);
        assert_eq!(p.lookup("mdl-lite").unwrap().input, 1.0);
        // どれにも当たらなければ None (**0 円にしない**)
        assert!(p.lookup("other-vendor-model").is_none());
        assert!(p.lookup("").is_none());
    }

    /// 無効にすると全部「不明」になる (0 円で埋めない)。
    #[test]
    fn 無効なら単価を返さない() {
        let mut p = PricingConfig::default();
        let any = p.models.keys().next().cloned().expect("既定の表が空でない");
        assert!(p.lookup(&any).is_some());
        p.enabled = false;
        assert!(p.lookup(&any).is_none());
    }

    /// 日付サフィックス付きの ID も前方一致で当たる。
    #[test]
    fn 日付サフィックス付きの_id_も引ける() {
        let p = PricingConfig::default();
        let base = p
            .models
            .keys()
            .max_by_key(|k| k.len())
            .cloned()
            .expect("既定の表が空でない");
        let dated = format!("{base}-20991231");
        assert_eq!(
            p.lookup(&dated).map(|m| m.input),
            p.lookup(&base).map(|m| m.input),
            "{dated} が {base} に当たる"
        );
    }

    /// `PriceLookup` として quota から引ける (単価が素通しで渡る)。
    #[test]
    fn quota_の_pricelookup_として使える() {
        let p = PricingConfig::default();
        let name = p.models.keys().next().cloned().unwrap();
        let src = p.models[&name];
        let got = PriceLookup::rate(&p, &name).expect("引けること");
        assert_eq!(got.input, src.input);
        assert_eq!(got.output, src.output);
        assert_eq!(got.cache_write, src.cache_write);
        assert_eq!(got.cache_read, src.cache_read);
        assert_eq!(PriceLookup::currency(&p), p.currency);
        assert!(PriceLookup::rate(&p, "no-such-model-anywhere").is_none());
    }

    // ── コスト上限 ────────────────────────────────────────────────────

    /// **出荷時は上限なし。** 設定しない限り何も止まらないし何も出ない。
    #[test]
    fn コスト上限の既定は無制限で通知だけ() {
        use crate::coordinator::quota::LimitAction;
        let c = Config::default();
        assert_eq!(c.cost_limit_session, 0.0);
        assert_eq!(c.cost_limit_daily, 0.0);
        assert_eq!(c.cost_warn_ratio, DEFAULT_COST_WARN_RATIO);
        assert_eq!(c.cost_limit_action, "notify");
        let l = c.cost_limits();
        assert!(!l.any(), "既定で見張りが動いてはいけない");
        assert_eq!(l.action, LimitAction::Notify, "既定で勝手に止めない");
        assert_eq!(l.blocks(1e9, 1e9), None);
    }

    /// 設定 → [`crate::coordinator::quota::CostLimits`] の畳み方。
    #[test]
    fn コスト上限は設定から畳まれる() {
        use crate::coordinator::quota::{BudgetKind, BudgetState, LimitAction};
        let mut c = Config::default();
        c.cost_limit_session = 10.0;
        c.cost_limit_daily = 100.0;
        c.cost_warn_ratio = 0.5;
        c.cost_limit_action = "stop".into();
        let l = c.cost_limits();
        assert!(l.any());
        assert_eq!(l.session, 10.0);
        assert_eq!(l.daily, 100.0);
        assert_eq!(l.warn_ratio, 0.5);
        assert_eq!(l.action, LimitAction::Stop);
        let w = l.worst(6.0, 1.0).expect("上限がある");
        assert_eq!(w.kind, BudgetKind::Session);
        assert_eq!(w.state, BudgetState::Warn);
        // 打ち間違いは既定 (notify) へ倒す — 勝手に止まるほうが害が大きい
        c.cost_limit_action = "sotp".into();
        assert_eq!(c.cost_limits().action, LimitAction::Notify);
        // 負の値は無制限として扱う (0 未満を上限にしない)
        c.cost_limit_session = -5.0;
        c.cost_limit_daily = -1.0;
        assert!(!c.cost_limits().any());
    }

    /// 設定 GUI から編集できる (一覧に載り、現在値と既定を引け、書き戻せる)。
    #[test]
    fn コスト上限は設定_gui_から編集できる() {
        let mut cfg = Config::default();
        let keys = [
            "cost_limit_session",
            "cost_limit_daily",
            "cost_warn_ratio",
            "cost_limit_action",
        ];
        for k in keys {
            assert!(
                setting_defs().iter().any(|d| d.key == k),
                "{k} が設定一覧に無い = GUI から届かない"
            );
            assert!(setting_value(&cfg, k).is_some(), "{k} の現在値が読めない");
            assert!(setting_default(k).is_some(), "{k} の既定が引けない");
            assert!(
                !setting_doc(k).trim().is_empty(),
                "{k} の説明がテンプレートから届かない"
            );
            assert!(!is_setting_modified(&cfg, k), "{k} が最初から変更扱い");
        }
        // 書き戻し (型が合うものだけ通る)
        assert!(set_setting_value(
            &mut cfg,
            "cost_limit_daily",
            &SettingValue::Float(25.0)
        ));
        assert_eq!(cfg.cost_limit_daily, 25.0);
        assert!(is_setting_modified(&cfg, "cost_limit_daily"));
        assert!(set_setting_value(
            &mut cfg,
            "cost_limit_action",
            &SettingValue::Text("stop".into())
        ));
        assert_eq!(cfg.cost_limit_action, "stop");
        // 型違いは触らない
        assert!(!set_setting_value(
            &mut cfg,
            "cost_limit_daily",
            &SettingValue::Text("25".into())
        ));
        assert_eq!(cfg.cost_limit_daily, 25.0);
        // 選択肢は notify / stop の 2 つだけ
        let d = setting_defs()
            .iter()
            .find(|d| d.key == "cost_limit_action")
            .unwrap();
        assert_eq!(d.kind, SettingKind::Choice(COST_LIMIT_ACTIONS));
        assert_eq!(COST_LIMIT_ACTIONS, ["notify", "stop"]);
        // バッジから開く絞り込み語 "cost_" で 4 項目とも出る
        let rows = settings_rows(&cfg, "cost_", false);
        for k in keys {
            assert!(
                rows.iter().any(|d| d.key == k),
                "バッジから開いた設定画面に {k} が出ない"
            );
        }
    }

    /// TOML を往復しても表が壊れない (ユーザーが上書きできる)。
    #[test]
    fn 単価表は_toml_で上書きできる() {
        let raw = r#"
[pricing]
enabled = true
currency = "¥"

[pricing.models."my-model"]
input = 100.0
output = 500.0
cache_write = 125.0
cache_read = 10.0
"#;
        let cfg: Config = toml::from_str(raw).expect("読めること");
        assert_eq!(cfg.pricing.currency, "¥");
        let m = cfg.pricing.lookup("my-model").expect("引けること");
        assert_eq!(m.input, 100.0);
        assert_eq!(m.output, 500.0);
        // 書き戻しても読み直せる
        let back = toml::to_string(&cfg).expect("書けること");
        let again: Config = toml::from_str(&back).expect("読み直せること");
        assert_eq!(
            again.pricing.lookup("my-model").map(|m| m.input),
            Some(100.0)
        );
    }
}

#[cfg(test)]
mod feature_settings_tests {
    use super::*;
    use crate::feature::{Setting, SettingValue as Decl};

    /// テスト用の宣言。**レジストリが空でも**値の決め方を表で検査するために、
    /// `feature::REGISTRY` ではなく手で組んだ宣言を純粋関数へ渡す。
    const fn decl(key: &'static str, default: Decl) -> Setting {
        Setting {
            key,
            label: "テスト設定",
            help: "テストの説明",
            default,
        }
    }

    // ── 番人 ──────────────────────────────────────────────────────

    #[test]
    fn 機能の設定キーはモジュール接頭辞付きで一意() {
        // これが無いと、2 つのブランチが偶然同じキーを選んだときに
        // 片方の設定が静かに死ぬ (先に登録されたほうが勝つ)。
        let mut seen: Vec<&str> = Vec::new();
        for f in crate::feature::REGISTRY {
            for s in f.settings {
                let prefix = format!("{}.", f.module);
                assert!(
                    s.key.starts_with(&prefix) && s.key.len() > prefix.len(),
                    "設定キー {:?} は {:?} で始めること (接頭辞が衝突回避の要)",
                    s.key,
                    prefix
                );
                assert!(
                    !seen.contains(&s.key),
                    "設定キーが重複している: {:?}",
                    s.key
                );
                seen.push(s.key);
                assert!(!s.label.trim().is_empty(), "{:?} のラベルが空", s.key);
                assert!(
                    setting_defs().iter().all(|d| d.key != s.key),
                    "{:?} が組み込みの設定と衝突している",
                    s.key
                );
            }
        }
    }

    // ── 後方互換 / 未知のキー ─────────────────────────────────────

    #[test]
    fn features区画の無い古いconfigでも読める() {
        let cfg: Config = toml::from_str("theme = \"zaivern-light\"\nword_wrap = true\n")
            .expect("features を持たない config.toml が読めない");
        assert_eq!(cfg.theme, "zaivern-light");
        assert!(cfg.word_wrap);
        assert!(cfg.extra.is_empty(), "無い区画を勝手に埋めている");
        // 出荷時のテンプレートもそのまま読める
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("既定テンプレートが読めない");
        assert!(cfg.extra.is_empty());
    }

    #[test]
    fn 機能設定の未知のキーは読み書きしても消えない() {
        // 「新しい版が足した設定を、古い版で 1 度起動しただけで消す」事故を防ぐ。
        let raw = r#"
theme = "zaivern-dark"

[features]
"whichkey.delay_ms" = 300
"みらいの機能.flag" = true
"#;
        let cfg: Config = toml::from_str(raw).expect("[features] を持つ config が読めない");
        assert_eq!(cfg.extra.len(), 2);
        assert_eq!(cfg.feature_i64("whichkey.delay_ms"), 300);
        assert!(cfg.feature_bool("みらいの機能.flag"));
        // 往復 (読み → 書き → 読み) で 1 つも落ちない
        let back = toml::to_string(&cfg).expect("Config を書けない");
        let again: Config = toml::from_str(&back).expect("書いたものを読み直せない");
        assert_eq!(again.extra, cfg.extra, "往復で未知のキーが消えた");
    }

    #[test]
    fn 機能設定の書き戻しは知らないキーもコメントも消さない() {
        let raw = concat!(
            "theme = \"zaivern-dark\"\n",
            "\n",
            "[features]\n",
            "# 手書きのコメント\n",
            "\"whichkey.delay_ms\" = 300\n",
            "\"みらいの機能.flag\" = true\n",
            "\n",
            "[keybindings]\n",
            "palette = \"⌘K\"\n",
        );
        let mut vals: std::collections::BTreeMap<String, String> = Default::default();
        vals.insert("whichkey.delay_ms".into(), "500".into());
        let out = rewrite_features_section(raw, &vals);
        assert!(
            out.contains("\"whichkey.delay_ms\" = 500"),
            "対象の行が置き換わっていない:\n{out}"
        );
        assert!(
            out.contains("\"みらいの機能.flag\" = true"),
            "この版が知らないキーが消えた:\n{out}"
        );
        assert!(out.contains("# 手書きのコメント"), "コメントが消えた");
        assert!(out.contains("[keybindings]"), "他の区画が消えた");
        // 何度書いても同じ (べき等)
        assert_eq!(rewrite_features_section(&out, &vals), out);
        let cfg: Config = toml::from_str(&out).expect("書き戻した結果が読めない");
        assert_eq!(cfg.feature_i64("whichkey.delay_ms"), 500);
        assert!(cfg.feature_bool("みらいの機能.flag"));
    }

    // ── 通知のオン/オフ ──────────────────────────────────────────────

    /// **通知の旗を触るテストを直列化する。**
    ///
    /// `notify::enabled()` / `sound()` は**プロセス共通の `AtomicBool`** で、
    /// `apply_runtime_flags` が書き換える。テストは既定で並列に走るので、
    /// 「旗を立てる → 読む」のあいだに**別のテストが倒せる**。
    /// 実際にフルスイートでだけ落ちた (単独では通る = いちばん質の悪い形)。
    ///
    /// 「オンへ倒す向きなら衝突しない」は**誤り**だった —
    /// 立ててから読むまでが不可分でないので、向きは関係ない。
    ///
    /// 毒された (panic を跨いだ) ロックも受け取る。ここが守るのは
    /// 「同時に触らない」ことだけで、中身の一貫性ではない。
    fn notify_flag_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 欄の無い古い `config.toml` (と、そもそも設定を作っていない利用者) が
    /// **オンのまま**であること。ここがオフに倒れると、更新しただけで
    /// 通知が消えたように見える。
    #[test]
    fn 通知の既定はオンで欄の無い設定ファイルも壊れない() {
        use crate::features::notifications::KEY_ENABLED;
        // 欄が 1 つも無い config.toml
        let cfg: Config = toml::from_str("theme = \"zaivern-dark\"\n").expect("読めない");
        assert!(!cfg.extra.contains_key(KEY_ENABLED), "無い欄を勝手に埋めた");
        assert!(cfg.feature_bool(KEY_ENABLED), "欄が無いのにオフへ倒れた");
        // 既定の Config も同じ
        assert!(Config::default().feature_bool(KEY_ENABLED));
        // 旗へ写す側も同じ値を配る。**旗はプロセス共通**なので直列化する。
        let _g = notify_flag_lock();
        apply_runtime_flags(&cfg);
        assert!(crate::notify::enabled());
    }

    /// 設定画面 (⚙) の行として出ていること。**ここが空だと切り替えられない**
    /// (レジストリに登録しただけで到達経路が無い状態になる)。
    #[test]
    fn 通知の設定は設定画面の行として出る() {
        use crate::features::notifications::KEY_ENABLED;
        let d = all_setting_defs()
            .iter()
            .find(|d| d.key == KEY_ENABLED)
            .expect("設定画面に行が出ていない");
        assert!(
            matches!(d.kind, SettingKind::Bool),
            "チェックボックスで出る"
        );
        // 検索でも引ける (設定画面は settings_rows 越しに描く)
        let cfg = Config::default();
        assert!(
            settings_rows(&cfg, "通知", false)
                .iter()
                .any(|r| r.key == KEY_ENABLED),
            "「通知」で検索しても出てこない"
        );
    }

    /// 設定画面から切り替えると、値が保存側にも実行時の旗にも届くこと。
    #[test]
    fn 通知の設定を切り替えると値が保存側へ届く() {
        use crate::features::notifications::KEY_ENABLED;
        let mut cfg = Config::default();
        assert!(
            set_setting_value(&mut cfg, KEY_ENABLED, &SettingValue::Bool(false)),
            "設定画面からの書き込みが弾かれた"
        );
        assert!(!cfg.feature_bool(KEY_ENABLED), "オフが保存されていない");
        // 実際に鳴らすかどうかは notify 側の純粋な判定が持つ
        // (旗はプロセス共通なので、ここでオフを観測しに行くと
        //  同時に走る他のテストの `load` と取り合って偽の赤が出る)。
        assert!(set_setting_value(
            &mut cfg,
            KEY_ENABLED,
            &SettingValue::Bool(true)
        ));
        assert!(cfg.feature_bool(KEY_ENABLED));
        let _g = notify_flag_lock();
        apply_runtime_flags(&cfg);
        assert!(crate::notify::enabled(), "オンへ戻しても鳴らない");
        // 型違いは書かせない (書けると読み出しが既定へ落ちて「効かない」になる)
        assert!(!set_setting_value(
            &mut cfg,
            KEY_ENABLED,
            &SettingValue::Int(1)
        ));
    }

    /// 欄の無い古い `config.toml` が **「通知オン・音あり」** のまま読めること。
    /// ここが無音へ倒れると、更新しただけで音が消えたように見える。
    #[test]
    fn 通知音の既定はオンで欄の無い設定ファイルも壊れない() {
        use crate::features::notifications::KEY_SOUND;
        let cfg: Config = toml::from_str("theme = \"zaivern-dark\"\n").expect("読めない");
        assert!(!cfg.extra.contains_key(KEY_SOUND), "無い欄を勝手に埋めた");
        assert!(cfg.feature_bool(KEY_SOUND), "欄が無いのに無音へ倒れた");
        assert!(Config::default().feature_bool(KEY_SOUND));
        // 旗へ写す側も同じ値を配る (オンの向きは他のテストと衝突しない)
        let _g = notify_flag_lock();
        apply_runtime_flags(&cfg);
        assert!(crate::notify::sound(), "旗へ音ありが届いていない");
    }

    /// 通知音が設定画面 (⚙) の行として、通知本体とは**別のチェック**で出ること。
    /// ここが空だと「通知は見たいが音は要らない」を選べない (元の不満そのもの)。
    #[test]
    fn 通知音の設定は設定画面の独立した行として出る() {
        use crate::features::notifications::{KEY_ENABLED, KEY_SOUND};
        let d = all_setting_defs()
            .iter()
            .find(|d| d.key == KEY_SOUND)
            .expect("設定画面に行が出ていない");
        assert!(
            matches!(d.kind, SettingKind::Bool),
            "チェックボックスで出る"
        );
        let cfg = Config::default();
        let rows = settings_rows(&cfg, "通知", false);
        // 「通知」で検索すると 2 行とも出る = 独立して切り替えられる
        assert!(rows.iter().any(|r| r.key == KEY_ENABLED));
        assert!(rows.iter().any(|r| r.key == KEY_SOUND));
    }

    /// 音だけをオフにしても、通知本体はオンのままであること
    /// (2 つの設定が独立していることの検査)。
    #[test]
    fn 音だけを切っても通知本体はオンのまま() {
        use crate::features::notifications::{KEY_ENABLED, KEY_SOUND};
        let mut cfg = Config::default();
        assert!(
            set_setting_value(&mut cfg, KEY_SOUND, &SettingValue::Bool(false)),
            "設定画面からの書き込みが弾かれた"
        );
        assert!(!cfg.feature_bool(KEY_SOUND), "無音が保存されていない");
        assert!(cfg.feature_bool(KEY_ENABLED), "音を切ったら通知まで消えた");
        // 実際に音を鳴らすかどうかの判定は notify 側の純関数が持つ
        // (旗はプロセス共通なので、ここでオフを観測しに行かない)。
        assert!(set_setting_value(
            &mut cfg,
            KEY_SOUND,
            &SettingValue::Bool(true)
        ));
        // 型違いは書かせない
        assert!(!set_setting_value(
            &mut cfg,
            KEY_SOUND,
            &SettingValue::Int(0)
        ));
    }

    #[test]
    fn features区画が無ければ末尾に作る() {
        let mut vals: std::collections::BTreeMap<String, String> = Default::default();
        vals.insert("mymod.on".into(), "true".into());
        let out = rewrite_features_section("theme = \"zaivern-dark\"\n", &vals);
        assert!(out.contains("[features]"), "区画が作られていない:\n{out}");
        let cfg: Config = toml::from_str(&out).expect("作った区画が読めない");
        assert!(cfg.feature_bool("mymod.on"));
        // 空のファイルからでも壊れない
        let out = rewrite_features_section("", &vals);
        let cfg: Config = toml::from_str(&out).expect("空から作った区画が読めない");
        assert!(cfg.feature_bool("mymod.on"));
    }

    #[test]
    fn 書き戻しは点付きのキーだけをfeatures区画へ回す() {
        // トップレベルへ `mymod.delay_ms = …` と書くと TOML の**ドット付きキー**
        // (= `[mymod]` テーブル) になり、`[features]` からは二度と読めない。
        let mut vals: std::collections::BTreeMap<String, String> = Default::default();
        vals.insert("word_wrap".into(), "true".into());
        vals.insert("mymod.delay_ms".into(), "300".into());
        let out = rewrite_settings("theme = \"zaivern-dark\"\n", &vals);
        let cfg: Config = toml::from_str(&out).expect("書き戻した結果が読めない");
        assert!(cfg.word_wrap, "組み込みの設定が効いていない");
        assert!(
            cfg.extra.contains_key("mymod.delay_ms"),
            "機能の設定が [features] に入っていない:\n{out}"
        );
        assert_eq!(cfg.feature_i64("mymod.delay_ms"), 300);
    }

    #[test]
    fn 引用符付きキーの取り出しは代入行だけを拾う() {
        assert_eq!(quoted_key_of("\"a.b\" = 1"), Some("a.b"));
        assert_eq!(quoted_key_of("  \"a.b\"   =   1  "), Some("a.b"));
        // 裸のキーは別物 (TOML では入れ子テーブルになる)
        assert_eq!(quoted_key_of("a.b = 1"), None);
        // 代入ではない / 閉じていない / 空 — どれも panic しない
        assert_eq!(quoted_key_of("\"a.b\""), None);
        assert_eq!(quoted_key_of("\"a.b\" x = 1"), None);
        assert_eq!(quoted_key_of("\""), None);
        assert_eq!(quoted_key_of(""), None);
        assert_eq!(quoted_key_of("# \"a.b\" = 1"), None);
    }

    // ── 値の解決 (既定へ落ちる / panic しない) ───────────────────

    #[test]
    fn 機能の設定は値なし型違い宣言なしでも既定へ落ちる() {
        let b = decl("t.b", Decl::Bool(true));
        let i = decl("t.i", Decl::Int(7));
        let f = decl("t.f", Decl::Float(1.5));
        let s = decl("t.s", Decl::Text("既定の文字列"));
        let wrong = toml::Value::String("ちがう型".into());

        // 値が無い → 宣言された既定
        assert!(value_bool(None, Some(&b)));
        assert_eq!(value_i64(None, Some(&i)), 7);
        assert_eq!(value_f64(None, Some(&f)), 1.5);
        assert_eq!(value_str(None, Some(&s)), "既定の文字列");

        // 型が違う → 宣言された既定 (panic しない)
        assert!(value_bool(Some(&wrong), Some(&b)));
        assert_eq!(value_i64(Some(&wrong), Some(&i)), 7);
        assert_eq!(value_f64(Some(&wrong), Some(&f)), 1.5);
        assert_eq!(
            value_str(Some(&toml::Value::Integer(1)), Some(&s)),
            "既定の文字列"
        );
        assert_eq!(value_i64(Some(&toml::Value::Float(1.5)), Some(&i)), 7);

        // 宣言が無い → 型の既定
        assert!(!value_bool(None, None));
        assert_eq!(value_i64(None, None), 0);
        assert_eq!(value_f64(None, None), 0.0);
        assert_eq!(value_str(None, None), "");
        assert!(!value_bool(Some(&wrong), None));
        assert_eq!(value_f64(Some(&wrong), None), 0.0);

        // 型が合う保存値は宣言より優先
        assert!(!value_bool(Some(&toml::Value::Boolean(false)), Some(&b)));
        assert_eq!(value_i64(Some(&toml::Value::Integer(9)), Some(&i)), 9);
        assert_eq!(
            value_str(Some(&toml::Value::String("上書き".into())), Some(&s)),
            "上書き"
        );
        // float は整数リテラルも受ける (手書きの `= 2` を黙って捨てない)
        assert_eq!(value_f64(Some(&toml::Value::Integer(2)), Some(&f)), 2.0);
        // int の宣言でも float の既定を持てないので 0 ではなく宣言値へ
        assert_eq!(value_f64(None, Some(&i)), 7.0);
    }

    #[test]
    fn 未宣言の機能設定を読み書きしてもpanicしない() {
        let mut cfg = Config::default();
        assert!(!cfg.feature_bool("だれも.宣言していない"));
        assert_eq!(cfg.feature_i64("だれも.宣言していない"), 0);
        assert_eq!(cfg.feature_f64(""), 0.0);
        assert_eq!(cfg.feature_str("."), "");
        // 未宣言でも書ける (新しい版が足した設定を握り潰さないため)
        assert!(cfg.set_feature("みらいの機能.flag", SettingValue::Bool(true)));
        assert!(cfg.feature_bool("みらいの機能.flag"));
        // 組み込みの設定の口からは未宣言のキーは通らない (打ち間違いを通さない)
        assert!(!set_setting_value(
            &mut cfg,
            "みらいの機能.flag2",
            &SettingValue::Bool(true)
        ));
        assert!(!cfg.extra.contains_key("みらいの機能.flag2"));
    }

    #[test]
    fn 機能の設定は宣言と型が違えば書かない() {
        let b = decl("t.b", Decl::Bool(true));
        let s = decl("t.s", Decl::Text("あ"));
        assert!(value_matches_decl(&SettingValue::Bool(false), &b));
        assert!(!value_matches_decl(&SettingValue::Int(1), &b));
        assert!(!value_matches_decl(&SettingValue::Text("1".into()), &b));
        assert!(value_matches_decl(&SettingValue::Text("い".into()), &s));
        assert!(!value_matches_decl(&SettingValue::Float(1.0), &s));
    }

    #[test]
    fn 宣言の型から設定画面のウィジェットが決まる() {
        assert_eq!(
            feature_kind(&decl("t.b", Decl::Bool(false))),
            SettingKind::Bool
        );
        assert!(matches!(
            feature_kind(&decl("t.i", Decl::Int(0))),
            SettingKind::Int { .. }
        ));
        assert!(matches!(
            feature_kind(&decl("t.f", Decl::Float(0.0))),
            SettingKind::Float { .. }
        ));
        assert_eq!(
            feature_kind(&decl("t.s", Decl::Text(""))),
            SettingKind::Text
        );
        assert_eq!(
            feature_default_value(&decl("t.s", Decl::Text("あ"))),
            SettingValue::Text("あ".into())
        );
        assert_eq!(
            feature_default_value(&decl("t.i", Decl::Int(3))),
            SettingValue::Int(3)
        );
    }

    // ── 設定画面へ出すデータ ──────────────────────────────────────

    #[test]
    fn 機能の設定は設定画面の共通経路に全部乗る() {
        let cfg = Config::default();
        let rows = feature_setting_rows(&cfg);
        assert_eq!(rows.len(), feature_setting_defs().len());
        assert_eq!(
            all_setting_defs().len(),
            setting_defs().len() + rows.len(),
            "組み込みと機能の設定の件数が合わない"
        );
        for r in &rows {
            // 出荷時は「変更なし」から始まる
            assert!(!r.is_modified(), "{} が最初から変更扱い", r.key);
            // 設定画面が使う共通経路 (現在値 / 既定 / 説明 / 一覧) が全部引ける
            assert_eq!(setting_value(&cfg, r.key).as_ref(), Some(&r.value));
            assert_eq!(setting_default(r.key).as_ref(), Some(&r.default));
            assert_eq!(setting_doc(r.key), r.help);
            assert!(!is_setting_modified(&cfg, r.key));
            assert!(
                settings_rows(&cfg, "", false)
                    .iter()
                    .any(|d| d.key == r.key),
                "{} が設定画面の一覧に出ない",
                r.key
            );
        }
        // 組み込みの設定は機能の設定に混ざらない
        assert!(rows.iter().all(|r| r.key.contains('.')));
    }

    #[test]
    fn 機能の設定は設定画面から変えて既定へ戻せる() {
        // 宣言が無くても通る経路 (set_feature) で往復を確認する。
        // 宣言のある往復は上の `機能の設定は設定画面の共通経路に全部乗る` が
        // レジストリの実物で押さえる。
        let mut cfg = Config::default();
        assert!(cfg.set_feature("mymod.count", SettingValue::Int(5)));
        assert_eq!(cfg.feature_i64("mymod.count"), 5);
        assert!(cfg.set_feature("mymod.count", SettingValue::Int(0)));
        assert_eq!(cfg.feature_i64("mymod.count"), 0);
        // 書いた値はそのまま config.toml のリテラルになる
        let mut vals: std::collections::BTreeMap<String, String> = Default::default();
        vals.insert("mymod.count".into(), SettingValue::Int(5).to_toml());
        let out = rewrite_settings("", &vals);
        let back: Config = toml::from_str(&out).expect("書いたものが読めない");
        assert_eq!(back.feature_i64("mymod.count"), 5);
    }
}

#[cfg(test)]
mod feature_settings_registry_tests {
    use super::*;

    /// 実際に登録されている機能の設定を、設定画面と同じ経路で往復させる。
    ///
    /// `feature_settings_tests` が手で組んだ宣言で「値の決め方」を押さえるのに対し、
    /// こちらは **`feature::REGISTRY` の実物**で
    /// 「設定画面で変える → config.toml へ書く → 読み直す」を通す。
    /// 設定を持つ機能が 1 つも無いうちは何も検査しないが、
    /// 機能が足された瞬間に**その機能を 1 行も書かずに**検査が始まる。
    #[test]
    fn 登録済みの機能の設定は設定画面から変えて書いて読み直せる() {
        let mut cfg = Config::default();
        let rows = feature_setting_rows(&cfg);
        let mut vals: std::collections::BTreeMap<String, String> = Default::default();
        for r in &rows {
            // 既定と必ず違う値を作る (型ごとに 1 段ずらす)
            let changed = match &r.default {
                SettingValue::Bool(b) => SettingValue::Bool(!b),
                SettingValue::Int(i) => SettingValue::Int(i.wrapping_add(1)),
                SettingValue::Float(f) => SettingValue::Float(f + 1.0),
                SettingValue::Text(s) => SettingValue::Text(format!("{s}x")),
            };
            assert!(
                set_setting_value(&mut cfg, r.key, &changed),
                "{} を設定画面の経路で書けない",
                r.key
            );
            assert_eq!(setting_value(&cfg, r.key).as_ref(), Some(&changed));
            assert!(
                is_setting_modified(&cfg, r.key),
                "{} が変更扱いにならない",
                r.key
            );
            // 型違いは弾く (弾かないと読み出しが既定へ落ちて「効かない」になる)
            let wrong = match &r.default {
                SettingValue::Bool(_) => SettingValue::Int(1),
                _ => SettingValue::Bool(true),
            };
            assert!(
                !set_setting_value(&mut cfg, r.key, &wrong),
                "{} が型違いを受け入れた",
                r.key
            );
            assert_eq!(setting_value(&cfg, r.key).as_ref(), Some(&changed));
            vals.insert(r.key.to_string(), changed.to_toml());
        }
        if vals.is_empty() {
            return;
        }
        // config.toml へ書いて読み直しても同じ値
        let text = rewrite_settings("", &vals);
        let back: Config = toml::from_str(&text).expect("書き出した config.toml が読めない");
        for r in &rows {
            assert_eq!(
                setting_value(&back, r.key),
                setting_value(&cfg, r.key),
                "{} が往復で変わった:\n{text}",
                r.key
            );
            assert!(
                is_setting_modified(&back, r.key),
                "{} の変更が残らない",
                r.key
            );
        }
        // 既定へ戻すと `@modified` から外れる
        for r in &rows {
            let def = setting_default(r.key).expect("既定が引けない");
            assert!(set_setting_value(&mut cfg, r.key, &def));
            assert!(!is_setting_modified(&cfg, r.key), "{} が戻らない", r.key);
        }
    }
}
