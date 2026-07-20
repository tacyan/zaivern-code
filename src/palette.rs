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
    /// 実行中の全 claude セッションの権限モードを切替(Shift+Tab 送信)
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
