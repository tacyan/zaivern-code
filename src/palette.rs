use std::path::PathBuf;

#[derive(Clone)]
pub enum Cmd {
    Save,
    SaveAs,
    CloseTab,
    NewFile,
    OpenFolder,
    ToggleTerminal,
    ToggleCockpit,
    /// アクティブな Markdown ファイルのレンダリングプレビュー切替
    ToggleMdPreview,
    ToggleSidebar,
    OpenFind,
    NewAgent(usize),
    FocusAgent(usize),
    RestartAgent,
    KillAgent,
    SetTheme(String),
    OpenConfig,
    ReloadConfig,
    FontInc,
    FontDec,
    SendFileToAgent,
    RefreshTree,
    /// 既定の承認モード: "ask"(毎回ユーザー承認) | "auto"(全自動YES) | "agent"(Agent欄優先)
    SetApproval(String),
    TogglePet,
    /// 実行中の対応エージェントの権限モードを切替
    CyclePermissionAll,
    SetPetImage,
    ResetPetImage,
    ResetPetPos,
    /// ペットの見た目バリアント ("blocky"|"crab"|"cat"|"cloud")
    SetPetVariant(String),
    /// ペットの表示スケール
    SetPetScale(f32),
    /// アンカーモード時にうろうろ歩くか
    TogglePetFreeRoam,
    /// 放置時に居眠りするか
    TogglePetSleep,
    /// 完了/承認待ち/エラーの効果音
    TogglePetSounds,
    /// 承認待ちの吹き出し表示
    TogglePetBubbles,
    /// スマホリモートの QR コードウィンドウ表示切替
    ToggleRemote,
    /// 音声入力の録音を開始/停止する。認識テキストは届け先の入力欄へ
    /// 挿入されるだけで、Enter は送られない
    VoiceInput(crate::voice::Target),
    /// 録音を止める (⏹ ボタン)
    VoiceStop,
    /// 音声入力の既定の届け先を変える (アクティブ / ブロードキャスト)
    SetVoiceTarget(crate::voice::Target),
    /// 音声認識エンジン ("auto"|"mac"|"command"|"off")
    SetVoiceEngine(String),
    /// 認識言語 (BCP-47。"ja-JP" など)
    SetVoiceLang(String),
    /// 話すと Enter まで送る合図キーワード (空文字で無効)
    SetVoiceKeyword(String),
    /// 新規プラグインのテンプレートを作成 (名前入力ダイアログを開く)
    NewPlugin,
    /// .zvplug / .zip ファイルを選んでプラグインをインストール
    InstallPlugin,
    /// プラグインを再スキャン
    RescanPlugins,
    /// サイドバーのプラグインタブを開く
    ShowPlugins,
    /// プラグインコマンドを実行 (plugins[i] の commands[j])
    RunPlugin(usize, usize),
}

#[derive(Clone)]
pub enum Action {
    OpenFile(PathBuf),
    Cmd(Cmd),
}

pub struct Item {
    pub icon: String,
    pub label: String,
    pub detail: String,
    pub action: Action,
    pub score: i32,
}

pub struct Palette {
    pub open: bool,
    pub input: String,
    pub selected: usize,
    pub just_opened: bool,
}

impl Palette {
    pub fn new() -> Self {
        Self {
            open: false,
            input: String::new(),
            selected: 0,
            just_opened: false,
        }
    }

    pub fn open_files(&mut self) {
        self.open = true;
        self.input.clear();
        self.selected = 0;
        self.just_opened = true;
    }

    pub fn open_commands(&mut self) {
        self.open = true;
        self.input = ">".into();
        self.selected = 0;
        self.just_opened = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.input.clear();
        self.selected = 0;
    }

    pub fn is_command_mode(&self) -> bool {
        self.input.trim_start().starts_with('>')
    }

    pub fn query(&self) -> &str {
        let t = self.input.trim_start();
        t.strip_prefix('>').map(|s| s.trim_start()).unwrap_or(t)
    }
}
