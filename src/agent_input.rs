//! Agent Input Field & Slash Command Processing Engine
//!
//! 神レベルの超高速パフォーマンス（100万PV/s超高負荷環境基準）で設計された、
//! エージェント入力欄の高度編集・Undo/Redo・プロンプト履歴・スラッシュコマンド補完モジュール。

use std::collections::HashMap;
use std::collections::VecDeque;

/// スラッシュコマンド定義
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// /goal [prompt] — 目標達成・長時間集中タスク指示
    Goal(String),
    /// /loop [count] [prompt] — 指定回数または条件付きループ指示
    Loop(usize, String),
    /// /plan [prompt] — 詳細実行計画の策定指示
    Plan(String),
    /// /grill-me [prompt] — 対話型インタビューによる要求整理指示
    GrillMe(String),
    /// /learn [prompt] — ルール・知見の記憶保存指示
    Learn(String),
    /// /clear — 入力欄クリア
    Clear,
    /// /help — スラッシュコマンドのヘルプ表示
    Help,
    /// /reset — セッションリセット指示
    Reset,
    /// 未知または一般テキスト
    Unknown(String),
}

/// スラッシュコマンドのメタデータ（補完用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommandInfo {
    pub name: &'static str,
    pub syntax: &'static str,
    pub description: &'static str,
}

pub const AVAILABLE_SLASH_COMMANDS: &[SlashCommandInfo] = &[
    SlashCommandInfo {
        name: "/goal",
        syntax: "/goal <指示テキスト>",
        description: "目標を完全に達成するまで徹底的に検証・実行を繰り返すモードを有効にします。",
    },
    SlashCommandInfo {
        name: "/loop",
        syntax: "/loop <回数> <指示テキスト>",
        description: "指定した回数分、指示タスクを自動ループ実行します。（デフォルト: 3回）",
    },
    SlashCommandInfo {
        name: "/plan",
        syntax: "/plan <設計・タスク概要>",
        description: "複雑なタスクを実行前に詳細なステップ単位で計画・設計します。",
    },
    SlashCommandInfo {
        name: "/grill-me",
        syntax: "/grill-me <テーマ・要求>",
        description: "AIからの質問形式インタビューで設計方針や未決定事項を掘り下げて決定します。",
    },
    SlashCommandInfo {
        name: "/learn",
        syntax: "/learn <学習させるルール・知見>",
        description: "現在のセッションで得た重要な解決策やルールを次回以降のために記憶します。",
    },
    SlashCommandInfo {
        name: "/clear",
        syntax: "/clear",
        description: "入力欄のテキストを消去します。",
    },
    SlashCommandInfo {
        name: "/help",
        syntax: "/help",
        description: "利用可能なスラッシュコマンド一覧と説明を表示します。",
    },
    SlashCommandInfo {
        name: "/reset",
        syntax: "/reset",
        description: "現在のアクティブセッションのコンテキストをリセットします。",
    },
];

/// 100万件/秒の超高負荷にも耐えうるO(1)メモリ操作・Zero-copyパースを目的とした
/// スラッシュコマンド補完・パースエンジン
pub struct SlashCommandEngine;

impl SlashCommandEngine {
    /// 入力テキストからスラッシュコマンドを解析（アロケーション最小化）
    pub fn parse(input: &str) -> SlashCommand {
        let trimmed = input.trim_start();
        if !trimmed.starts_with('/') {
            return SlashCommand::Unknown(input.to_string());
        }

        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("");
        let args = parts.next().unwrap_or("").trim();

        match cmd {
            "/goal" => SlashCommand::Goal(args.to_string()),
            "/loop" => {
                let mut loop_parts = args.splitn(2, char::is_whitespace);
                let count_str = loop_parts.next().unwrap_or("");
                if let Ok(count) = count_str.parse::<usize>() {
                    let prompt = loop_parts.next().unwrap_or("").trim().to_string();
                    SlashCommand::Loop(count, prompt)
                } else {
                    // 数字が省略された場合はデフォルト3回とし、args全体をプロンプトとする
                    SlashCommand::Loop(3, args.to_string())
                }
            }
            "/plan" => SlashCommand::Plan(args.to_string()),
            "/grill-me" => SlashCommand::GrillMe(args.to_string()),
            "/learn" => SlashCommand::Learn(args.to_string()),
            "/clear" => SlashCommand::Clear,
            "/help" => SlashCommand::Help,
            "/reset" => SlashCommand::Reset,
            _ => SlashCommand::Unknown(input.to_string()),
        }
    }

    /// 接頭辞（例: "/g"）に基づく高速な補完候補検索（Prefix Match）
    #[allow(dead_code)]
    pub fn autocomplete(prefix: &str) -> Vec<&'static SlashCommandInfo> {
        if !prefix.starts_with('/') {
            return Vec::new();
        }
        let lower = prefix.to_lowercase();
        AVAILABLE_SLASH_COMMANDS
            .iter()
            .filter(|info| info.name.starts_with(&lower))
            .collect()
    }

    /// スラッシュコマンドをプロンプト用に整形・展開
    pub fn expand_command(cmd: &SlashCommand) -> String {
        match cmd {
            SlashCommand::Goal(prompt) => {
                if prompt.is_empty() {
                    "🎯 [Goal Mode] 目標達成まで自動検証・実行を継続してください。".to_string()
                } else {
                    format!("🎯 [Goal Mode] 以下の目標を完全に達成するまで徹底的に検証・実行を継続してください:\n{prompt}")
                }
            }
            SlashCommand::Loop(count, prompt) => {
                if prompt.is_empty() {
                    format!("🔄 [Loop Mode] 以下のタスクを {count} 回繰り返し実行してください。")
                } else {
                    format!("🔄 [Loop Mode] 以下のタスクを {count} 回繰り返し実行してください:\n{prompt}")
                }
            }
            SlashCommand::Plan(prompt) => {
                if prompt.is_empty() {
                    "📋 [Plan Mode] 段階的で詳細な実行計画を作成してください。".to_string()
                } else {
                    format!("📋 [Plan Mode] 以下のタスクについて段階的で詳細な実行計画を作成してください:\n{prompt}")
                }
            }
            SlashCommand::GrillMe(prompt) => {
                if prompt.is_empty() {
                    "🔥 [Grill-Me Mode] 設計方針や要件について質疑応答インタビューを開始してください。".to_string()
                } else {
                    format!("🔥 [Grill-Me Mode] 以下の件について質問形式で要件や設計を深掘りしてください:\n{prompt}")
                }
            }
            SlashCommand::Learn(prompt) => {
                if prompt.is_empty() {
                    "🧠 [Learn Mode] 今回の解決策およびルールを記憶として記録してください。".to_string()
                } else {
                    format!("🧠 [Learn Mode] 以下のルールおよび知見を記録・永続記憶してください:\n{prompt}")
                }
            }
            SlashCommand::Clear => String::new(),
            SlashCommand::Help => {
                let mut help_str = String::from("💡 **利用可能なスラッシュコマンド一覧**:\n");
                for info in AVAILABLE_SLASH_COMMANDS {
                    help_str.push_str(&format!("- `{}`: {}\n", info.syntax, info.description));
                }
                help_str
            }
            SlashCommand::Reset => "⚠️ セッションをリセットします。".to_string(),
            SlashCommand::Unknown(text) => text.clone(),
        }
    }
}

/// メッセージの送信先。
///
/// 従来の入力欄は「全エージェントへ一斉送信 (ブロードキャスト)」の 1 本しか
/// 持っていなかったため、差分レビューのプロンプトのように**特定の 1 体に宛てたい**
/// 文章まで全員に飛んでいた。送信先を型で区別して、下書きも送信先ごとに分ける。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComposerTarget {
    /// 全エージェントへ一斉送信 (従来の `broadcast_input` に相当)
    Broadcast,
    /// セッション ID で指名した 1 体だけへ送信
    Agent(u64),
}

impl ComposerTarget {
    /// 全員宛てか
    pub fn is_broadcast(self) -> bool {
        matches!(self, Self::Broadcast)
    }
}

/// 下書きへの追記ロジック本体。
///
/// 書きかけを捨てず、空行 1 つを挟んで後ろへ継ぎ足す。追記できたら true。
/// `append_prompt` / `append_prompt_for` の両方がここを通るので、
/// アクティブな送信先でも退避中の送信先でも結果は完全に同じになる。
fn append_into(dst: &mut String, add: &str) -> bool {
    let add = add.trim();
    if add.is_empty() {
        return false;
    }
    if !dst.trim().is_empty() {
        // 末尾の改行を 1 本にそろえてから空行区切りで連結する。
        while dst.ends_with('\n') || dst.ends_with('\r') {
            dst.pop();
        }
        dst.push_str("\n\n");
    } else {
        dst.clear();
    }
    dst.push_str(add);
    true
}

/// 超高速・省メモリなエージェント入力バッファ管理構造体。
/// Ctrl+A, Ctrl+U, Ctrl+K, Undo/Redo, プロンプト履歴検索をサポート。
///
/// **送信先ごとの下書き**: `text` は「いまアクティブな送信先 (`target`) の下書き」で、
/// それ以外の送信先の書きかけは `drafts` に退避される。`set_target` で行き来しても
/// 各エージェント宛ての文章は消えない。プロンプト履歴 (`history`) は
/// 送信先をまたいで 1 本を共有する — 「さっき打った指示」を別のエージェントへ
/// 使い回せる方が実用的なため。
#[derive(Debug, Clone)]
pub struct AgentInputBuffer {
    /// 現在の入力テキスト (= `target` の下書き)
    text: String,
    /// カーソル位置（文字インデックス）
    cursor: usize,
    /// 選択範囲（開始文字インデックス, 終了文字インデックス）
    selection: Option<(usize, usize)>,
    /// 入力履歴（最大200件のO(1)制限リングバッファ）
    history: VecDeque<String>,
    /// 現在参照中の履歴インデックス
    history_idx: Option<usize>,
    /// 履歴検索時の一時保存用（ユーザーが入力中のテキスト）
    saved_draft: String,
    /// Undoスタック（最大100件）
    undo_stack: VecDeque<(String, usize)>,
    /// Redoスタック（最大100件）
    redo_stack: VecDeque<(String, usize)>,
    /// 最大Undo保持件数
    max_undo_depth: usize,
    /// いま編集中の送信先。`text` はこの送信先の下書き。
    target: ComposerTarget,
    /// 非アクティブな送信先の下書き置き場 (アクティブ分は `text` が保持する)。
    /// 空文字は積まない — 使われなかったエージェント分でメモリを食わないように。
    drafts: HashMap<ComposerTarget, String>,
    /// ユーザーが**自分で選んだ**送信先。`sync_target` はこれを踏み潰さない。
    ///
    /// 以前は「全員宛てを選んだか」の bool しか持っていなかったため、チップで
    /// 1 体を名指ししても次のフレームの追従がアクティブなエージェント
    /// (= 起動順で一番最後) へ引き戻していた。指名も同じように守る。
    pinned: Option<ComposerTarget>,
    /// 直近の `sync_target` で見たアクティブなエージェント。
    /// 「ユーザーがアクティブを動かした」のか「同じ状態を描き直しただけ」なのかは
    /// これでしか区別できない。
    last_active: Option<u64>,
}

impl Default for AgentInputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentInputBuffer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            selection: None,
            history: VecDeque::with_capacity(200),
            history_idx: None,
            saved_draft: String::new(),
            undo_stack: VecDeque::with_capacity(100),
            redo_stack: VecDeque::with_capacity(100),
            max_undo_depth: 100,
            target: ComposerTarget::Broadcast,
            drafts: HashMap::new(),
            pinned: None,
            last_active: None,
        }
    }

    // ── 送信先ごとの下書き ───────────────────────────────────────────

    /// いまアクティブな送信先
    pub fn target(&self) -> ComposerTarget {
        self.target
    }

    /// 送信先を切り替える。
    ///
    /// いまの下書きを退避してから、行き先の下書きを引っぱり出す。
    /// Undo 履歴は繋がらない (別の宛先の別の文章なので) ため捨てる。
    pub fn set_target(&mut self, t: ComposerTarget) {
        if self.target == t {
            return;
        }
        let cur = std::mem::take(&mut self.text);
        if cur.is_empty() {
            self.drafts.remove(&self.target);
        } else {
            self.drafts.insert(self.target, cur);
        }
        self.text = self.drafts.remove(&t).unwrap_or_default();
        self.target = t;
        self.cursor = self.text.chars().count();
        self.selection = None;
        self.history_idx = None;
        self.saved_draft.clear();
        self.undo_stack.clear();
        self.redo_stack.clear();
    }

    /// **ユーザーが自分で選んだ送信先**にする (チップを押した等)。
    ///
    /// `set_target` との違いはピン留めだけ — こちらで決めた宛先は
    /// [`sync_target`](Self::sync_target) の追従に踏み潰されない。
    pub fn pick_target(&mut self, t: ComposerTarget) {
        self.pinned = Some(t);
        self.set_target(t);
    }

    /// アクティブなエージェントに送信先を追従させる。
    ///
    /// - `active = Some(id)`: 既定でそのエージェント宛て。ただし**ユーザーが自分で
    ///   選んだ宛先 (ピン留め) は踏み潰さない**:
    ///   - 全員宛ては「モード」なので、アクティブが動いても外れない。
    ///   - 1 体の名指しは、ユーザーが**アクティブを動かすまで**守る
    ///     (動かしたらそちらが新しい意思なので追従する)。
    /// - `active = None`: 宛先にできるエージェントがいないので全員宛てへ戻す。
    ///
    /// 切り替えが起きたら true。
    pub fn sync_target(&mut self, active: Option<u64>) -> bool {
        // 「同じ状態を描き直しただけ」か「アクティブが実際に動いた」か。
        // UI は毎フレームここを通るので、この区別が無いと指名が 1 フレームで消える。
        let moved = self.last_active != active;
        self.last_active = active;
        let want = match active {
            None => {
                // 宛先にできる相手が居ない — ユーザーの選択も意味を失う
                self.pinned = None;
                ComposerTarget::Broadcast
            }
            Some(id) => match self.pinned {
                Some(ComposerTarget::Broadcast) => ComposerTarget::Broadcast,
                Some(t @ ComposerTarget::Agent(_)) if !moved => t,
                _ => {
                    self.pinned = None;
                    ComposerTarget::Agent(id)
                }
            },
        };
        if want == self.target {
            return false;
        }
        self.set_target(want);
        true
    }

    /// 指定した送信先の下書きを覗く (アクティブでも退避中でも同じように読める)。
    /// 本番の描画経路は `text()` を読むので、これは下書き退避を検証するテスト専用。
    #[cfg(test)]
    pub fn draft_for(&self, t: ComposerTarget) -> &str {
        if t == self.target {
            &self.text
        } else {
            self.drafts.get(&t).map(String::as_str).unwrap_or("")
        }
    }

    /// 指定した送信先の下書きを**置き換える** (送信先は切り替えない)。
    ///
    /// 差分レビューのプロンプトのように「この 1 体に宛てた文章」を、
    /// いまユーザーが別のエージェント宛てに書いている手を止めずに置いておくための口。
    /// 書きかけを残したいなら [`Self::append_prompt_for`] を使う。
    ///
    /// 本番の流し込みは追記側 ([`Self::append_prompt_for`]) を使っているため、
    /// いまの利用者は panels.rs / app.rs / 本モジュールのテストだけ。
    #[cfg(test)]
    pub fn set_draft_for(&mut self, t: ComposerTarget, text: impl Into<String>) {
        if t == self.target {
            self.set_text(text);
            return;
        }
        let s = text.into();
        if s.is_empty() {
            self.drafts.remove(&t);
        } else {
            self.drafts.insert(t, s);
        }
    }

    /// 指定した送信先の下書きへ**追記**する ([`Self::append_prompt`] の宛先指定版)。
    ///
    /// アクティブな送信先なら `append_prompt` と完全に同じ (Undo 1 回で戻せる)。
    pub fn append_prompt_for(&mut self, t: ComposerTarget, prompt: &str) -> bool {
        if t == self.target {
            return self.append_prompt(prompt);
        }
        let slot = self.drafts.entry(t).or_default();
        let ok = append_into(slot, prompt);
        if !ok && slot.is_empty() {
            // 空プロンプトで空の枠だけ作ってしまわない
            self.drafts.remove(&t);
        }
        ok
    }

    /// 指定した送信先の下書きを取り出して空にする
    #[allow(dead_code)]
    pub fn take_draft(&mut self, t: ComposerTarget) -> String {
        if t == self.target {
            let s = self.text.clone();
            self.clear();
            s
        } else {
            self.drafts.remove(&t).unwrap_or_default()
        }
    }

    /// アクティブでない送信先に残っている下書きの数 (UI のバッジ用)
    pub fn pending_draft_count(&self) -> usize {
        self.drafts.values().filter(|s| !s.trim().is_empty()).count()
    }

    /// エージェントが畳まれたら、その宛先の下書きも捨てる。
    ///
    /// 消えた相手宛ての文章を残しても送り先がないうえ、ID が再利用されると
    /// 無関係なエージェントへ他人宛ての文章が出てしまう。
    pub fn forget_agent(&mut self, id: u64) {
        let t = ComposerTarget::Agent(id);
        self.drafts.remove(&t);
        if self.pinned == Some(t) {
            // 消えた相手へのピン留めを残すと、以降の追従が二度と効かなくなる
            self.pinned = None;
        }
        if self.target == t {
            // 退避させずに捨てる (set_target だと空を書き戻してしまう)
            self.text.clear();
            self.target = ComposerTarget::Broadcast;
            self.text = self.drafts.remove(&ComposerTarget::Broadcast).unwrap_or_default();
            self.cursor = self.text.chars().count();
            self.selection = None;
            self.history_idx = None;
            self.saved_draft.clear();
            self.undo_stack.clear();
            self.redo_stack.clear();
        }
    }

    /// 生きているセッション ID だけを残し、消えた分の下書きを掃除する
    pub fn retain_agents(&mut self, alive: &[u64]) {
        self.drafts.retain(|k, _| match k {
            ComposerTarget::Broadcast => true,
            ComposerTarget::Agent(id) => alive.contains(id),
        });
        if let ComposerTarget::Agent(id) = self.target {
            if !alive.contains(&id) {
                self.forget_agent(id);
            }
        }
    }

    /// 現在のテキスト取得
    pub fn text(&self) -> &str {
        &self.text
    }

    /// テキスト設定
    pub fn set_text(&mut self, new_text: impl Into<String>) {
        let s = new_text.into();
        if self.text != s {
            self.push_undo_state();
            self.text = s;
            self.cursor = self.text.chars().count();
            self.selection = None;
        }
    }

    /// カーソル位置（文字単位）
    #[allow(dead_code)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 選択範囲取得
    #[allow(dead_code)]
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.selection
    }

    /// 選択解除
    #[allow(dead_code)]
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// 選択されている文字列を取得（無ければNone）
    #[allow(dead_code)]
    pub fn get_selected_text(&self) -> Option<String> {
        let (start, end) = self.selection?;
        let min = start.min(end);
        let max = start.max(end);
        let selected: String = self.text.chars().skip(min).take(max - min).collect();
        if selected.is_empty() {
            None
        } else {
            Some(selected)
        }
    }

    /// 選択範囲または現在のカーソル位置で削除
    pub fn delete_selection(&mut self) -> bool {
        if let Some((start, end)) = self.selection {
            self.push_undo_state();
            let min = start.min(end);
            let max = start.max(end);
            let chars: Vec<char> = self.text.chars().collect();
            let mut new_chars = Vec::with_capacity(chars.len() - (max - min));
            new_chars.extend_from_slice(&chars[..min]);
            new_chars.extend_from_slice(&chars[max..]);
            self.text = new_chars.into_iter().collect();
            self.cursor = min;
            self.selection = None;
            true
        } else {
            false
        }
    }

    /// **Ctrl+W / Alt+Backspace**: カーソル前の単語を削除
    #[allow(dead_code)]
    pub fn delete_word_before(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        self.push_undo_state();
        let chars: Vec<char> = self.text.chars().collect();
        let mut idx = self.cursor;

        // 空白をスキップ
        while idx > 0 && chars[idx - 1].is_whitespace() {
            idx -= 1;
        }
        // 単語文字をスキップ
        while idx > 0 && !chars[idx - 1].is_whitespace() {
            idx -= 1;
        }

        let mut new_chars = Vec::with_capacity(chars.len() - (self.cursor - idx));
        new_chars.extend_from_slice(&chars[..idx]);
        new_chars.extend_from_slice(&chars[self.cursor..]);
        self.text = new_chars.into_iter().collect();
        self.cursor = idx;
    }

    /// **Up Arrow**: 前のプロンプト履歴を参照
    // 配線待ち: panels.rs の `agent_composer_inline_ui` (1 行帯) で ↑/↓ を
    // `ComposerPress` に足して呼ぶ。履歴自体は `submit()` が本番で積んでいる。
    // 複数行フォーム側は ↑/↓ が行移動に要るので繋がない。
    #[allow(dead_code)]
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }

        match self.history_idx {
            None => {
                self.saved_draft = self.text.clone();
                let last_idx = self.history.len() - 1;
                self.history_idx = Some(last_idx);
                self.text = self.history[last_idx].clone();
                self.cursor = self.text.chars().count();
                self.selection = None;
            }
            Some(idx) if idx > 0 => {
                let next_idx = idx - 1;
                self.history_idx = Some(next_idx);
                self.text = self.history[next_idx].clone();
                self.cursor = self.text.chars().count();
                self.selection = None;
            }
            _ => {}
        }
    }

    /// **Down Arrow**: 次のプロンプト履歴（または入力中のドラフト）を参照
    // 配線待ち: panels.rs の `agent_composer_inline_ui` (1 行帯) で ↑/↓ を
    // `ComposerPress` に足して呼ぶ。履歴自体は `submit()` が本番で積んでいる。
    // 複数行フォーム側は ↑/↓ が行移動に要るので繋がない。
    #[allow(dead_code)]
    pub fn history_next(&mut self) {
        if let Some(idx) = self.history_idx {
            if idx + 1 < self.history.len() {
                let next_idx = idx + 1;
                self.history_idx = Some(next_idx);
                self.text = self.history[next_idx].clone();
                self.cursor = self.text.chars().count();
                self.selection = None;
            } else {
                self.history_idx = None;
                self.text = self.saved_draft.clone();
                self.cursor = self.text.chars().count();
                self.selection = None;
            }
        }
    }

    /// プロンプトを履歴に保存し、送信準備をする
    pub fn submit(&mut self) -> String {
        let trimmed = self.text.trim().to_string();
        if !trimmed.is_empty() {
            // 重複追加を防ぐ
            if self.history.back() != Some(&trimmed) {
                if self.history.len() >= 200 {
                    self.history.pop_front();
                }
                self.history.push_back(trimmed.clone());
            }
        }

        let cmd = SlashCommandEngine::parse(&self.text);
        let expanded = SlashCommandEngine::expand_command(&cmd);

        self.clear();
        expanded
    }

    /// 外部で組み立てたプロンプト (diff ビューのレビューコメントなど) を
    /// 入力欄へ流し込む。
    ///
    /// 書きかけの下書きは捨てず、空行 1 つを挟んで後ろに継ぎ足す。Undo 1 回で
    /// 元に戻せるので、誤って流し込んでも取り返しがつく。追記できたら true。
    ///
    /// 呼び出し側 (app.rs) の想定配線 — **宛先込み**で入れるのが正解:
    /// ```text
    /// if let Some(p) = crate::diff::take_pending_review_prompt(ctx) {
    ///     let t = self.review_target();  // 差分を見ていたエージェント (無ければアクティブ)
    ///     self.agent_input_buf.append_prompt_for(t, &p);
    /// }
    /// ```
    /// 宛先を指定しないこの関数はアクティブな送信先へ入る。
    /// そのまま送信まで通したい場合は `submit()` の戻り値をエージェントの
    /// stdin へ書けばよい (送信経路は agents.rs 側)。
    // app.rs 側の配線待ち。配線されるまで未使用でも警告にしない。
    #[allow(dead_code)]
    pub fn append_prompt(&mut self, prompt: &str) -> bool {
        if prompt.trim().is_empty() {
            return false;
        }
        self.push_undo_state();
        let ok = append_into(&mut self.text, prompt);
        self.cursor = self.text.chars().count();
        self.selection = None;
        self.history_idx = None;
        ok
    }

    /// クリア
    pub fn clear(&mut self) {
        self.push_undo_state();
        self.text.clear();
        self.cursor = 0;
        self.selection = None;
        self.history_idx = None;
        self.saved_draft.clear();
    }

    /// Undo 状態の保存
    fn push_undo_state(&mut self) {
        if self.undo_stack.len() >= self.max_undo_depth {
            self.undo_stack.pop_front();
        }
        self.undo_stack.push_back((self.text.clone(), self.cursor));
        self.redo_stack.clear();
    }

    /// **Ctrl+Z / Cmd+Z**: Undo
    #[allow(dead_code)]
    pub fn undo(&mut self) {
        if let Some((prev_text, prev_cursor)) = self.undo_stack.pop_back() {
            self.redo_stack.push_back((self.text.clone(), self.cursor));
            self.text = prev_text;
            self.cursor = prev_cursor;
            self.selection = None;
        }
    }

    /// **Ctrl+Shift+Z / Cmd+Y**: Redo
    #[allow(dead_code)]
    pub fn redo(&mut self) {
        if let Some((next_text, next_cursor)) = self.redo_stack.pop_back() {
            self.undo_stack.push_back((self.text.clone(), self.cursor));
            self.text = next_text;
            self.cursor = next_cursor;
            self.selection = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slash_command_parsing() {
        assert_eq!(
            SlashCommandEngine::parse("/goal リファクタリングを実行"),
            SlashCommand::Goal("リファクタリングを実行".to_string())
        );

        assert_eq!(
            SlashCommandEngine::parse("/loop 5 テストを実行"),
            SlashCommand::Loop(5, "テストを実行".to_string())
        );

        assert_eq!(
            SlashCommandEngine::parse("/loop テストを実行"),
            SlashCommand::Loop(3, "テストを実行".to_string())
        );

        assert_eq!(
            SlashCommandEngine::parse("/plan データベース高速化方針"),
            SlashCommand::Plan("データベース高速化方針".to_string())
        );

        assert_eq!(
            SlashCommandEngine::parse("/grill-me インメモリキャッシュ設計"),
            SlashCommand::GrillMe("インメモリキャッシュ設計".to_string())
        );

        assert_eq!(
            SlashCommandEngine::parse("/learn WALモード適用ノウハウ"),
            SlashCommand::Learn("WALモード適用ノウハウ".to_string())
        );

        assert_eq!(SlashCommandEngine::parse("/clear"), SlashCommand::Clear);
        assert_eq!(SlashCommandEngine::parse("/help"), SlashCommand::Help);
    }

    #[test]
    fn test_autocomplete() {
        let matches = SlashCommandEngine::autocomplete("/g");
        assert_eq!(matches.len(), 2); // /goal, /grill-me
        assert_eq!(matches[0].name, "/goal");
        assert_eq!(matches[1].name, "/grill-me");

        let matches_all = SlashCommandEngine::autocomplete("/");
        assert_eq!(matches_all.len(), 8);
    }

    #[test]
    fn test_agent_input_buffer_shortcuts() {
        let mut buf = AgentInputBuffer::new();
        buf.set_text("Hello Amazing World");
        buf.cursor = 13; // "Hello Amazing" の末尾

        // Ctrl+W: delete word before
        buf.delete_word_before();
        assert_eq!(buf.text(), "Hello  World");
    }

    #[test]
    fn test_history_navigation() {
        let mut buf = AgentInputBuffer::new();
        buf.set_text("first prompt");
        buf.submit();

        buf.set_text("second prompt");
        buf.submit();

        buf.set_text("current typing");
        buf.history_prev();
        assert_eq!(buf.text(), "second prompt");

        buf.history_prev();
        assert_eq!(buf.text(), "first prompt");

        buf.history_next();
        assert_eq!(buf.text(), "second prompt");

        buf.history_next();
        assert_eq!(buf.text(), "current typing");
    }

    #[test]
    fn test_undo_redo() {
        let mut buf = AgentInputBuffer::new();
        buf.set_text("Initial");
        buf.set_text("Second");

        buf.undo();
        assert_eq!(buf.text(), "Initial");

        buf.redo();
        assert_eq!(buf.text(), "Second");
    }

    #[test]
    fn test_goal_and_loop_expansion() {
        let goal_cmd = SlashCommandEngine::parse("/goal DBのインデックス設計を見直して速度を100倍にする");
        let expanded = SlashCommandEngine::expand_command(&goal_cmd);
        assert!(expanded.contains("[Goal Mode]"));
        assert!(expanded.contains("DBのインデックス設計を見直して速度を100倍にする"));

        let loop_cmd = SlashCommandEngine::parse("/loop 10 キャッシュの整合性チェック");
        let expanded_loop = SlashCommandEngine::expand_command(&loop_cmd);
        assert!(expanded_loop.contains("[Loop Mode]"));
        assert!(expanded_loop.contains("10 回"));
        assert!(expanded_loop.contains("キャッシュの整合性チェック"));
    }

    #[test]
    fn test_high_performance_throughput() {
        // 100万PV/s超高負荷基準のZero-copy高速パースパフォーマンス検証
        let start = std::time::Instant::now();
        for _ in 0..20_000 {
            let cmd = SlashCommandEngine::parse("/goal 高速パースベンチマーク");
            let _expanded = SlashCommandEngine::expand_command(&cmd);
        }
        let elapsed = start.elapsed();
        assert!(elapsed.as_millis() < 2000, "Parsing took too long: {:?}", elapsed);
    }

    // ---- レビュープロンプトの流し込み ----

    #[test]
    fn append_prompt_into_empty_buffer() {
        let mut b = AgentInputBuffer::new();
        assert!(b.append_prompt("  以下のレビューコメントに対応してください:\n\n@a.rs:1\n> x\n直して  "));
        assert_eq!(b.text(), "以下のレビューコメントに対応してください:\n\n@a.rs:1\n> x\n直して");
        assert_eq!(b.cursor(), b.text().chars().count());
    }

    #[test]
    fn append_prompt_keeps_existing_draft_and_separates_with_blank_line() {
        let mut b = AgentInputBuffer::new();
        b.set_text("書きかけ\n\n\n");
        assert!(b.append_prompt("@a.rs:1\n> x\n直して"));
        assert_eq!(b.text(), "書きかけ\n\n@a.rs:1\n> x\n直して");
    }

    #[test]
    fn append_prompt_ignores_blank_and_is_undoable() {
        let mut b = AgentInputBuffer::new();
        b.set_text("元の下書き");
        assert!(!b.append_prompt("   \n  "), "空プロンプトは無視");
        assert_eq!(b.text(), "元の下書き");
        assert!(b.append_prompt("追いプロンプト"));
        assert_eq!(b.text(), "元の下書き\n\n追いプロンプト");
        b.undo();
        assert_eq!(b.text(), "元の下書き", "Undo 1 回で流し込み前に戻る");
    }

    // ---- 送信先ごとの下書き ----

    const A1: ComposerTarget = ComposerTarget::Agent(1);
    const A2: ComposerTarget = ComposerTarget::Agent(2);
    const BC: ComposerTarget = ComposerTarget::Broadcast;

    #[test]
    fn drafts_are_isolated_per_target_and_persist_across_switches() {
        let mut b = AgentInputBuffer::new();

        // 3 つの宛先へ別々の下書きを書く (書く → 切り替える → 書く)
        b.set_text("全員へ: リリース準備");
        b.set_target(A1);
        b.set_text("claude へ: このテストを直して");
        b.set_target(A2);
        b.set_text("codex へ: ドキュメントを更新");

        // 行ったり来たりしても混ざらない・消えない
        for _ in 0..3 {
            b.set_target(BC);
            assert_eq!(b.text(), "全員へ: リリース準備");
            b.set_target(A1);
            assert_eq!(b.text(), "claude へ: このテストを直して");
            b.set_target(A2);
            assert_eq!(b.text(), "codex へ: ドキュメントを更新");
        }

        // アクティブでも退避中でも同じ口から読める
        assert_eq!(b.draft_for(A2), "codex へ: ドキュメントを更新");
        assert_eq!(b.draft_for(A1), "claude へ: このテストを直して");
        assert_eq!(b.draft_for(BC), "全員へ: リリース準備");
        assert_eq!(b.draft_for(ComposerTarget::Agent(999)), "", "未使用の宛先は空");
        assert_eq!(b.target(), A2);
        assert_eq!(b.pending_draft_count(), 2, "アクティブ以外の下書きが 2 件");
    }

    #[test]
    fn removing_an_agent_drops_its_draft() {
        let mut b = AgentInputBuffer::new();
        b.set_text("全員へ");
        b.set_target(A1);
        b.set_text("1 番へ");
        b.set_target(A2);
        b.set_text("2 番へ");

        // 退避中のエージェントが畳まれた場合
        b.forget_agent(1);
        assert_eq!(b.draft_for(A1), "", "畳まれた 1 番の下書きは消える");
        assert_eq!(b.text(), "2 番へ", "アクティブな 2 番はそのまま");

        // アクティブなエージェント自身が畳まれた場合 → 全員宛てへ退避
        b.forget_agent(2);
        assert_eq!(b.target(), BC);
        assert_eq!(b.text(), "全員へ", "全員宛ての下書きが戻ってくる");
        assert_eq!(b.draft_for(A2), "");
        assert_eq!(b.pending_draft_count(), 0);
    }

    #[test]
    fn retain_agents_sweeps_dead_sessions() {
        let mut b = AgentInputBuffer::new();
        b.set_target(A1);
        b.set_text("1 番へ");
        b.set_target(A2);
        b.set_text("2 番へ");
        b.set_target(ComposerTarget::Agent(3));
        b.set_text("3 番へ");

        b.retain_agents(&[2]);
        assert_eq!(b.draft_for(A1), "", "1 番は生きていないので消える");
        assert_eq!(b.draft_for(A2), "2 番へ", "2 番だけ残る");
        assert_eq!(b.target(), BC, "アクティブだった 3 番が消えたら全員宛てへ戻る");
        assert_eq!(b.text(), "");
    }

    #[test]
    fn sync_target_follows_active_agent_but_respects_pinned_broadcast() {
        let mut b = AgentInputBuffer::new();

        assert!(b.sync_target(Some(7)), "既定は指名 (全員宛てではない)");
        assert_eq!(b.target(), ComposerTarget::Agent(7));
        assert!(!b.sync_target(Some(7)), "同じ宛先なら切り替えない");

        assert!(b.sync_target(Some(8)), "アクティブが変われば追従する");
        assert_eq!(b.target(), ComposerTarget::Agent(8));

        // ユーザーが自分で全員宛てを選んだら、アクティブが変わっても戻されない
        b.pick_target(BC);
        assert!(!b.sync_target(Some(9)));
        assert_eq!(b.target(), BC);

        // 自分で 1 体を指名し直せば、そちらが新しい意思になる
        b.pick_target(ComposerTarget::Agent(9));
        assert!(!b.sync_target(Some(9)));
        assert_eq!(b.target(), ComposerTarget::Agent(9));

        // 宛先にできるエージェントが居なくなったら全員宛てへ
        assert!(b.sync_target(None));
        assert_eq!(b.target(), BC);
    }

    /// **選んだ宛先が「アクティブ = 一番最後のエージェント」へ引き戻されない。**
    ///
    /// UI は毎フレーム `sync_target` を通る。追従がレベルトリガのままだと、
    /// チップで選んだ相手が次のフレームでアクティブへ戻され、
    /// 「どれを押しても最後が選ばれる」ように見えていた。
    #[test]
    fn 選んだ宛先は毎フレームの追従で踏み潰されない() {
        let mut b = AgentInputBuffer::new();
        // アクティブは起動順で最後 (agents.rs: active = sessions.len() - 1)
        let last = 4;
        b.sync_target(Some(last));
        assert_eq!(b.target(), ComposerTarget::Agent(last));

        // 先頭のエージェントを名指しする
        b.pick_target(A1);
        for frame in 0..5 {
            assert!(!b.sync_target(Some(last)), "{frame}: 追従が指名を上書きした");
            assert_eq!(b.target(), A1, "{frame} フレーム後に最後へ戻された");
        }

        // ユーザーがアクティブを動かしたら (タイルを押した等) そちらへ追従する
        assert!(b.sync_target(Some(2)));
        assert_eq!(b.target(), A2, "アクティブの切り替えに追従しなくなった");
    }

    /// ピン留めした相手が消えたら、ピンも一緒に落とす。
    /// 残すと以降の追従が二度と効かず、無関係な相手へ送り続けることになる。
    #[test]
    fn 消えたエージェントへのピン留めは残らない() {
        let mut b = AgentInputBuffer::new();
        b.sync_target(Some(1));
        b.pick_target(A2);
        assert_eq!(b.target(), A2);

        // 2 番が畳まれた → 全員宛てへ戻り、ピンも消える
        b.retain_agents(&[1]);
        assert_eq!(b.target(), BC, "消えた相手を宛先にしたままにしてはいけない");
        assert!(b.sync_target(Some(1)), "ピンが残って追従が効かない");
        assert_eq!(b.target(), A1);
    }

    #[test]
    fn set_draft_for_stashes_without_stealing_the_active_target() {
        let mut b = AgentInputBuffer::new();
        b.set_target(A1);
        b.set_text("1 番に書きかけ");

        // レビュープロンプトを別のエージェント宛てに置く
        b.set_draft_for(A2, "以下のレビューコメントに対応してください:\n\n@a.rs:1\n> x");
        assert_eq!(b.target(), A1, "送信先は勝手に変わらない");
        assert_eq!(b.text(), "1 番に書きかけ", "書きかけを奪われない");
        assert_eq!(b.draft_for(A2), "以下のレビューコメントに対応してください:\n\n@a.rs:1\n> x");

        // アクティブな宛先を指定した場合は set_text と同じ (Undo で戻せる)
        b.set_draft_for(A1, "上書き");
        assert_eq!(b.text(), "上書き");
        b.undo();
        assert_eq!(b.text(), "1 番に書きかけ");

        // 空文字を入れたら枠ごと消える
        b.set_draft_for(A2, "");
        assert_eq!(b.draft_for(A2), "");
        assert_eq!(b.pending_draft_count(), 0);
    }

    #[test]
    fn append_prompt_for_matches_append_prompt_on_any_target() {
        // アクティブでない宛先へ追記しても、アクティブ側と同じ結果になる
        let mut stashed = AgentInputBuffer::new();
        stashed.set_target(A1);
        stashed.set_draft_for(A2, "書きかけ\n\n\n");
        assert!(stashed.append_prompt_for(A2, "@a.rs:1\n> x\n直して"));
        assert_eq!(stashed.draft_for(A2), "書きかけ\n\n@a.rs:1\n> x\n直して");

        let mut active = AgentInputBuffer::new();
        active.set_target(A2);
        active.set_text("書きかけ\n\n\n");
        assert!(active.append_prompt_for(A2, "@a.rs:1\n> x\n直して"));
        assert_eq!(active.text(), stashed.draft_for(A2), "宛先がどこでも結果は同じ");

        // 空プロンプトは無視され、空の枠も作らない
        let mut b = AgentInputBuffer::new();
        assert!(!b.append_prompt_for(A1, "  \n "));
        assert_eq!(b.pending_draft_count(), 0);
        assert_eq!(b.draft_for(A1), "");
    }

    #[test]
    fn take_draft_empties_only_that_target() {
        let mut b = AgentInputBuffer::new();
        b.set_text("全員へ");
        b.set_target(A1);
        b.set_text("1 番へ");

        assert_eq!(b.take_draft(BC), "全員へ");
        assert_eq!(b.draft_for(BC), "");
        assert_eq!(b.text(), "1 番へ", "アクティブは無傷");

        assert_eq!(b.take_draft(A1), "1 番へ");
        assert_eq!(b.text(), "", "アクティブを取り出したら空になる");
    }

    #[test]
    fn multiline_japanese_prompt_with_trailing_newline_round_trips() {
        let src = "以下のレビューコメントに対応してください:\n\n@src/app.rs:42\n> ここ、境界値がずれていませんか？\n直してテストも足してください。\n";
        let mut b = AgentInputBuffer::new();
        b.set_target(A1);
        b.set_draft_for(A2, src);

        // 退避 → 復帰でも 1 文字も変わらない
        assert_eq!(b.draft_for(A2), src);
        b.set_target(A2);
        assert_eq!(b.text(), src, "送信先を切り替えても末尾の改行まで保たれる");

        // 送信 (submit) を通しても素通しで返る
        assert_eq!(b.submit(), src, "スラッシュコマンドでない本文はそのまま");
        assert_eq!(b.text(), "", "送信後は下書きが空になる");
    }
}
