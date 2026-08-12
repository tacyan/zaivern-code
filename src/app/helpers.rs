use super::*;

/// ステータスバーのトークン/コストバッジ 1 個の高さ。
pub(super) const TOKEN_BADGE_H: f32 = 16.0;
/// 同・バッジ間の隙間。
pub(super) const TOKEN_BADGE_GAP: f32 = 8.0;
/// 同・行の残り幅のうちバッジ列が使ってよい割合。
///
/// ステータスバーは 1 行に左詰めと右詰めが同居する。`available_width()` は
/// 行の残り全部を返すので、そのまま使うと右詰め側と食い合って見切れる。
pub(super) const TOKEN_BADGE_MAX_FRACTION: f32 = 0.45;

/// 使用量ウィンドウのトークン明細 1 行分。
pub(super) struct TokenRow {
    /// 最も消費しているエージェントか (色を変えて目立たせる)。
    pub(super) top: bool,
    /// 見出し行。
    pub(super) head: String,
    /// ぶら下がる内訳 (種類別 / モデル別)。
    pub(super) subs: Vec<String>,
}

/// ステータスバーへ出すトークン/コストのバッジ材料。
///
/// `compact` は全エージェントの合算 1 個、`detail` はエージェント別。
/// どちらを描くかは幅次第で [`coordinator::quota::token_badge_layout`] が決める。
pub(super) struct TokenBadges {
    /// (表示文字列, ホバーで出す内訳)。
    pub(super) compact: (String, String),
    /// エージェント別 (消費の多い順)。
    pub(super) detail: Vec<(String, String)>,
}

/// バッジ 1 個のおおよその表示幅 (px)。
///
/// 等幅ではないので概算だが、**多めに見積もる**ので「入ると判断したのに
/// 見切れる」は起きない (ASCII 6.5px / それ以外 12.5px + 余白)。
pub(super) fn badge_width_px(text: &str) -> f32 {
    let w: f32 = text
        .chars()
        .map(|c| if c.is_ascii() { 6.5 } else { 12.5 })
        .sum();
    w + 14.0
}

/// 承認の監査ログを画面に出す件数 (末尾から。全部は読まない)。
pub(super) const APPROVAL_AUDIT_TAIL: usize = 200;

/// ツアーの「このタブを開いて」→ サイドバーのタブ。純関数なのでテストできる。
///
/// `tutorial::SidebarTarget` は app.rs の private な [`SidebarTab`] を見られない
/// ので別の型になっている。ここが**唯一の対応表**。
pub(super) fn sidebar_tab_for(t: tutorial::SidebarTarget) -> SidebarTab {
    use tutorial::SidebarTarget as S;
    match t {
        S::Files => SidebarTab::Files,
        S::Search => SidebarTab::Search,
        S::Agents => SidebarTab::Agents,
        S::Sessions => SidebarTab::Sessions,
        S::Plugins => SidebarTab::Plugins,
        S::Git => SidebarTab::Git,
        S::GitHub => SidebarTab::GitHub,
    }
}

/// 複数キャレットへの**一括挿入 1 回分**。`(新しい本文, 新しい選択, 箇所数)`。
///
/// `editor_ops::insert_at_all` が後ろから前へ当てるので位置ずれは起きない。
/// 呼び出し側は返った本文を **1 回だけ** `Buffer::text` へ入れること —
/// egui の取り消しは本文まるごとのスナップショットなので、1 回の差し替えが
/// そのまま「1 段の取り消し」になる (VS Code も複数キャレットの編集を 1 段にする)。
pub(super) fn multi_batch_insert(
    text: &str,
    sel: &editor_ops::MultiSel,
    ins: &str,
) -> (String, editor_ops::MultiSel, usize) {
    let n = sel.len();
    let (out, next) = editor_ops::insert_at_all(text, sel, ins);
    (out, next, n)
}

// ═══ 複数キャレット: 打鍵の横取りとポインタ操作 ═══════════════════
//
// `TextEdit` (egui 0.29) は `CCursorRange` を 1 つしか持たないので、打鍵は
// 主キャレット 1 本にしか入らない。そこで **`TextEdit` を描く前に**
// イベントを抜き取り、`editor_ops` の「全キャレットへ適用」系へ流す。
// ここに置く関数はすべて純関数 (egui の状態に触らない) なのでテストで固定できる。

/// 追加キャレットを描く上限。これを超える集合 (「全ての出現を選択」で数千件) は
/// 先頭から上限ぶんだけ塗る — 画面に出るのは可視行ぶんだけなので実害はなく、
/// 巨大ファイルで毎フレーム数千の矩形を組むほうが害になる。
pub(super) const MULTI_PAINT_MAX: usize = 512;

/// `TextEdit` へ渡す**前**に横取りする打鍵 (複数キャレットのときだけ)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MultiKey {
    /// 確定した文字入力 (`Event::Text`)。IME 変換中 (`Event::Ime`) は含まない。
    Text(String),
    Backspace,
    Delete,
    Enter,
}

/// Alt (⌥ / Alt) 付きのポインタ操作。`TextEdit` の描画中に拾い、外側で反映する
/// (描画中はバッファを可変借用しているので `self` を触れない)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MultiPointer {
    /// Alt+クリック: その位置のキャレットを足す / 既にあれば取り除く。
    Click(usize),
    /// Alt+ドラッグ開始: 矩形選択の始点 (char 添字)。
    DragStart(usize),
    /// Alt+ドラッグ中: 矩形の対角 (char 添字)。
    Drag(usize),
    /// ドラッグ終了: 始点を捨てる。
    DragEnd,
    /// Alt 無しのクリック / ドラッグ: 複数キャレットを解除する (VS Code と同じ)。
    Clear,
}

/// 1 つの入力イベントが「全キャレットへ配る打鍵」かどうか。
///
/// 修飾キー付き (⌘Z / ⌥⌫ / ⌃A など) は**横取りしない** — 取り消しや単語削除は
/// egui と OS の担当で、複数キャレットの意味を持たないため。
pub(super) fn multi_key_of(e: &egui::Event) -> Option<MultiKey> {
    match e {
        egui::Event::Text(t) if !t.is_empty() => Some(MultiKey::Text(t.clone())),
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } if !modifiers.command && !modifiers.ctrl && !modifiers.alt => match key {
            egui::Key::Backspace => Some(MultiKey::Backspace),
            egui::Key::Delete => Some(MultiKey::Delete),
            egui::Key::Enter => Some(MultiKey::Enter),
            _ => None,
        },
        _ => None,
    }
}

/// イベント列から打鍵を**抜き取る** (残りはそのまま `TextEdit` へ流れる)。
///
/// 1 フレームに複数届く (速いタイプ・キーリピート) ので、順番どおり全部返す。
/// 1 つだけ拾って残りを `TextEdit` へ流すと、あふれたぶんが主キャレットにだけ
/// 入って本文がずれる。
pub(super) fn take_multi_keys(events: &mut Vec<egui::Event>) -> Vec<MultiKey> {
    let mut ops = Vec::new();
    events.retain(|e| match multi_key_of(e) {
        Some(op) => {
            ops.push(op);
            false
        }
        None => true,
    });
    ops
}

/// 打鍵 1 つを全キャレットへ当てる。
pub(super) fn apply_multi_key(
    text: &str,
    sel: &editor_ops::MultiSel,
    op: &MultiKey,
) -> (String, editor_ops::MultiSel) {
    match op {
        MultiKey::Text(t) => editor_ops::type_at_all(text, sel, t),
        MultiKey::Backspace => editor_ops::backspace_at_all(text, sel),
        MultiKey::Delete => editor_ops::delete_forward_at_all(text, sel),
        MultiKey::Enter => editor_ops::newline_at_all_detect(text, sel),
    }
}

/// 打鍵の列を順に当てる。`(新しい本文, 新しいキャレット集合)`。
///
/// 呼び出し側は返った本文を **1 回だけ** `Buffer::text` へ入れること
/// (= egui の取り消しも 1 段。`multi_batch_insert` と同じ約束)。
pub(super) fn apply_multi_keys(
    text: &str,
    sel: &editor_ops::MultiSel,
    ops: &[MultiKey],
) -> (String, editor_ops::MultiSel) {
    let mut text = text.to_string();
    let mut sel = sel.clone();
    for op in ops {
        let (t, s) = apply_multi_key(&text, &sel, op);
        text = t;
        sel = s;
    }
    (text, sel)
}

/// バイト範囲 → char 範囲 (egui のキャレットは char 添字)。
/// 範囲外や文字境界でない値はクランプする (壊れた値でも落ちない)。
pub(super) fn byte_range_to_char_range(text: &str, r: &std::ops::Range<usize>) -> (usize, usize) {
    let clamp = |b: usize| {
        let mut b = b.min(text.len());
        while b > 0 && !text.is_char_boundary(b) {
            b -= 1;
        }
        b
    };
    let s = clamp(r.start);
    let e = clamp(r.end).max(s);
    let cs = text[..s].chars().count();
    (cs, cs + text[s..e].chars().count())
}

/// 複数キャレットの**選択範囲**を視覚行ごとの矩形へ割る (galley ローカル座標)。
///
/// `rows` は選択が跨る視覚行の矩形だけ (先頭 = 選択開始行、末尾 = 終了行)。
/// `x0` は開始行の x、`x1` は終了行の x。行をまたぐぶんは
/// 「開始行は x0 から行末 + `nl_w`」「中間行は行まるごと + `nl_w`」
/// 「終了行は行頭から x1」になる (`nl_w` は改行が選ばれていることを示す幅)。
///
/// 返る矩形は**必ず行の矩形の上下に収まり、互いに重ならない** (行が重ならないため)。
/// 幅 0 に潰れた行は返さない。
pub(super) fn selection_row_rects(
    rows: &[egui::Rect],
    x0: f32,
    x1: f32,
    nl_w: f32,
) -> Vec<egui::Rect> {
    if rows.is_empty() {
        return Vec::new();
    }
    let last = rows.len() - 1;
    let mut out = Vec::with_capacity(rows.len());
    for (i, rr) in rows.iter().enumerate() {
        let left = if i == 0 { x0.max(rr.left()) } else { rr.left() };
        let right = if i == last {
            x1
        } else {
            rr.right() + nl_w.max(0.0)
        };
        let right = right.max(left);
        if right - left <= 0.0 {
            continue;
        }
        out.push(egui::Rect::from_min_max(
            egui::pos2(left, rr.top()),
            egui::pos2(right, rr.bottom()),
        ));
    }
    out
}

/// char 添字 → `(行 0 起点, タブ展開後の表示桁 0 起点)`。
///
/// 矩形選択は「行 × 表示桁」で長方形を作る ([`editor_ops::column_selection`])
/// ので、egui の char キャレットをこの座標系へ移す必要がある。
/// 添字が本文より後ろなら末尾へクランプする (壊れた値でも落ちない)。
pub(super) fn char_index_to_line_col(
    text: &str,
    char_index: usize,
    tab_width: usize,
) -> (usize, usize) {
    let (mut line, mut col) = (0usize, 0usize);
    for (n, ch) in text.chars().enumerate() {
        if n >= char_index {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
            continue;
        }
        // タブ・全角 (CJK)・結合文字の桁送りは `textenc::advance_col` に一本化する。
        // ここだけ「1 文字 = 1 桁」で数えると、日本語の行から始めた矩形選択が
        // `editor_ops::column_selection` の数える桁とずれる (CR は制御文字 = 0 桁
        // なので CRLF の途中に桁を作らない、も同じ表から出てくる)。
        col = crate::textenc::advance_col(col, ch, tab_width);
    }
    (line, col)
}

/// UNIX エポック秒。時計が壊れている環境でも 0 を返して落ちない。
pub(super) fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `[[approval_policies]]` を 1 件、config.toml の末尾へ書き足す。
///
/// TOML の組み立ては `toml::Value` へ通す (パスやエージェント名に `"` や `\`
/// が入っていても壊れた config.toml を書かないため — `config::append_agent_preset`
/// と同じ理由・同じ流儀)。
pub(super) fn append_approval_policy(p: &config::ApprovalPolicy) -> Result<(), String> {
    let path = config::config_path();
    config::ensure_default();
    let mut raw = std::fs::read_to_string(&path).unwrap_or_default();
    if !raw.is_empty() && !raw.ends_with('\n') {
        raw.push('\n');
    }
    raw.push_str(&render_approval_policy(p));
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, raw).map_err(|e| format!("config.toml を書けません: {e}"))
}

/// `[[approval_policies]]` ブロック 1 件分の TOML テキスト (書き出しは決定的)。
pub(super) fn render_approval_policy(p: &config::ApprovalPolicy) -> String {
    let kv = |k: &str, v: &str| format!("{k} = {}\n", toml::Value::String(v.to_string()));
    let mut s = String::from("\n[[approval_policies]]\n");
    s.push_str(&kv("kind", &p.kind));
    s.push_str(&kv("scope", &p.scope));
    s.push_str(&kv("target", &p.target));
    s.push_str(&kv("decision", &p.decision));
    s
}

/// ルートの表示名(フォルダ名。取れなければフルパス)。
pub(super) fn root_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| root.to_string_lossy().to_string())
}

/// ワークスペース全体の短い表示名。
/// 単一ルートなら従来どおりフォルダ名だけ、複数なら `a, b (+2)` の形。
pub(super) fn roots_label(roots: &[PathBuf]) -> String {
    match roots.len() {
        0 => String::new(),
        1 => root_name(&roots[0]),
        n => {
            let head: Vec<String> = roots.iter().take(2).map(|r| root_name(r)).collect();
            if n > 2 {
                format!("{} (+{})", head.join(", "), n - 2)
            } else {
                head.join(", ")
            }
        }
    }
}

/// ウィンドウタイトル。
pub(super) fn workspace_title(roots: &[PathBuf]) -> String {
    format!("Zaivern Code — {}", roots_label(roots))
}

/// 保存セッションのルート一覧 (`saved`) を復元すべきか判定し、適用する順に並べ替える。
/// 復元しない (現在のルートの方が広い / 別ワークスペース) なら `None`。
///
/// 復元するのは「保存された構成が現在のルートをすべて含み、かつより広い」ときだけ。
/// そのうえで**いま開いたフォルダを先頭 (primary) に戻す**: 保存順のまま適用すると
/// `zai B` で開いたのに A が primary になり、エージェントの起動先も Git パネルも
/// ユーザーが指定していないフォルダを向いてしまう。
pub(super) fn restored_roots(current: &[PathBuf], mut saved: Vec<PathBuf>) -> Option<Vec<PathBuf>> {
    if saved.len() <= current.len() || !current.iter().all(|r| saved.contains(r)) {
        return None;
    }
    if let Some(primary) = current.first() {
        if let Some(pos) = saved.iter().position(|r| r == primary) {
            let head = saved.remove(pos);
            saved.insert(0, head);
        }
    }
    Some(saved)
}

/// エージェント / ターミナルを起動する作業フォルダを決める
/// (`ZaivernApp::agent_cwd` の本体。UI 抜きで検証できるよう切り出してある)。
///
/// `chosen` = 直近にユーザーが開いた・選んだフォルダ。次の 2 条件を満たすときだけ
/// 採用し、外れたら primary ルートへ落とす:
/// - ディレクトリとして実在する (worktree を消した後などを弾く)
/// - いずれかのルート配下にある (ワークスペースから外したフォルダを弾く)
/// セッション記録から「隔離 worktree の割り当て」を復元する (純粋関数)。
///
/// リポジトリ・ブランチ・cwd の 3 つが揃い、**フォルダが実在するときだけ**
/// 隔離として扱う。worktree を手で消した後に記録だけ残っている状態で
/// `git worktree remove` を撃たないための門番でもある。
pub(super) fn restored_worktree(rec: &session::AgentSessionRec) -> Option<worktree::AgentWorktree> {
    if rec.worktree_repo.is_empty() || rec.worktree_branch.is_empty() || rec.cwd.is_empty() {
        return None;
    }
    let dir = PathBuf::from(&rec.cwd);
    if !dir.is_dir() {
        return None;
    }
    Some(worktree::AgentWorktree {
        repo: PathBuf::from(&rec.worktree_repo),
        branch: rec.worktree_branch.clone(),
        dir,
    })
}

pub(super) fn agent_cwd_from(roots: &[PathBuf], chosen: Option<&Path>) -> PathBuf {
    let primary = || roots.first().cloned().unwrap_or_else(|| PathBuf::from("."));
    match chosen {
        Some(c) if c.is_dir() && roots.iter().any(|r| c.starts_with(r)) => c.to_path_buf(),
        _ => primary(),
    }
}

/// `.git` がファイルのとき、その中身 (`gitdir: <path>`) から実際の git ディレクトリを取り出す。
/// 相対パスは workspace 基準で解決する。
#[allow(dead_code)]
pub(super) fn parse_gitdir_file(contents: &str, workspace: &Path) -> Option<PathBuf> {
    let raw = contents
        .lines()
        .find_map(|l| l.trim().strip_prefix("gitdir:"))?
        .trim();
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(raw);
    Some(if p.is_absolute() {
        p
    } else {
        workspace.join(p)
    })
}

/// ブランチ表示のために読むべき HEAD のパス。
/// 通常のリポジトリは `<ws>/.git/HEAD` だが、linked worktree では `.git` が
/// ディレクトリではなくファイルなので、それが指す git ディレクトリ配下の HEAD を読む。
///
/// 現在のブランチ表示は git.rs (`git branch --show-current`) 経由なので、
/// この関数は呼ばれていない。linked worktree の扱いを自前で解決する必要が出た
/// ときのために、テスト付きで残してある。
#[allow(dead_code)]
pub(super) fn git_head_path(workspace: &Path) -> PathBuf {
    let dot_git = workspace.join(".git");
    if dot_git.is_file() {
        if let Some(dir) = std::fs::read_to_string(&dot_git)
            .ok()
            .and_then(|s| parse_gitdir_file(&s, workspace))
        {
            return dir.join("HEAD");
        }
    }
    dot_git.join("HEAD")
}

/// ペット画像を読み込み egui テクスチャ化する。長辺 256px に縮小する。
/// URL やファイルを OS の既定アプリ (ブラウザ等) で開く。
/// 入力欄に書いてある `old` を `new` にするための編集を求める。
///
/// 返すのは (消す文字数, 書き足す文字列)。端末の入力欄はカーソル位置から
/// Backspace で消すしかないので、**共通する先頭はそのまま残し、そこから後ろを
/// まるごと消して書き直す**。話しながら変換が変わっても、変わった部分だけの
/// やり取りで済む。
pub(super) fn diff_edit(old: &str, new: &str) -> (usize, String) {
    let common = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let del = old.chars().count() - common;
    let add: String = new.chars().skip(common).collect();
    (del, add)
}

/// 音声のひとまとまりを前の続きへ書き足すとき、間に空白が要るか。
///
/// 息継ぎのたびに区切って入力欄へ足していくので、英文は単語がつながらないよう
/// 空白を入れる。日本語は元々分かち書きしないため、入れると逆に読みにくい。
pub(super) fn needs_space(tail: Option<char>, head: Option<char>) -> bool {
    let (Some(a), Some(b)) = (tail, head) else {
        return false;
    };
    if a.is_whitespace() || b.is_whitespace() {
        return false;
    }
    !is_cjk(a) && !is_cjk(b)
}

/// 分かち書きしない文字 (かな・漢字・全角記号など)。
pub(super) fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F   // 全角の句読点・記号
        | 0x3040..=0x30FF // ひらがな・カタカナ
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF // 漢字
        | 0xF900..=0xFAFF // 互換漢字
        | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 // 全角英数・記号
    )
}

/// 認識テキストの末尾が合図キーワードなら、それを取り除いた本文を返す。
/// 音声認識は句読点を付けることがあるので、末尾の記号は無視して判定する。
pub(super) fn strip_trailing_keyword(text: &str, keyword: &str) -> Option<String> {
    let trimmed = text.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '。' | '、' | '.' | ',' | '!' | '?' | '！' | '？')
    });
    let rest = trimmed.strip_suffix(keyword)?;
    Some(rest.trim_end().to_string())
}

/// Chrome / Chromium の実行ファイルを探す。
///
/// Web Speech API は Chrome が一番素直に動く。Edge の `webkitSpeechRecognition` は
/// v134 の退行以来あてにならないので、Chrome が居るならそちらを優先する。
pub(super) fn chrome_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    const CANDIDATES: &[&str] = &[
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ];
    #[cfg(target_os = "windows")]
    const CANDIDATES: &[&str] = &[
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    ];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    const CANDIDATES: &[&str] = &[
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ];
    // Windows は管理者権限なしで入れると %LOCALAPPDATA% 側に入る。
    // こちらの方がむしろ普通なので、固定パスより先に見る。
    #[cfg(target_os = "windows")]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local).join(r"Google\Chrome\Application\chrome.exe");
        if p.is_file() {
            return Some(p);
        }
    }
    CANDIDATES.iter().map(PathBuf::from).find(|p| p.is_file())
}

pub(super) fn open_external(target: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(target).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(target).spawn();
    // procx: `cmd` のコンソール窓を一瞬でも出さずにブラウザだけ開く
    #[cfg(target_os = "windows")]
    let _ = crate::procx::hidden_command("cmd")
        .args(["/C", "start", "", target])
        .spawn();
}

pub(super) fn load_pet_texture(ctx: &egui::Context, path: &Path) -> Option<egui::TextureHandle> {
    let bytes = std::fs::read(path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    let mut rgba = img.to_rgba8();
    let (mut w, mut h) = rgba.dimensions();
    let longest = w.max(h);
    if longest > 256 {
        let scale = 256.0 / longest as f32;
        let nw = ((w as f32 * scale) as u32).max(1);
        let nh = ((h as f32 * scale) as u32).max(1);
        rgba = image::imageops::resize(&rgba, nw, nh, image::imageops::FilterType::Triangle);
        w = nw;
        h = nh;
    }
    let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
    Some(ctx.load_texture("zv-pet-image", color, egui::TextureOptions::LINEAR))
}

/// 言語IDに対応する LSP サーバーの起動コマンド。
pub(super) fn lsp_server_for(lang_id: &str) -> Option<&'static str> {
    match lang_id {
        "rust" => Some("rust-analyzer"),
        "typescript" | "javascript" | "typescriptreact" | "javascriptreact" => {
            Some("typescript-language-server --stdio")
        }
        "python" => Some("pyright-langserver --stdio"),
        "go" => Some("gopls"),
        _ => None,
    }
}

/// which() の否定結果を覚えておく時間。
///
/// 3 秒: 60fps なら約 180 フレーム分の spawn が 1 回に減る一方、人が LSP サーバーを
/// インストールし終える時間(cargo install / npm -g で数十秒〜数分)よりずっと短いので、
/// 「起動中に入れたサーバーがいずれ認識される」性質は保たれる。
/// そもそも egui は再描画要求があるときしかフレームを回さないため、再確認の間隔は
/// 元から不定だった(アイドル中は何分でも確認されない)。TTL はその保証を弱めない。
pub(super) const WHICH_MISS_TTL: Duration = Duration::from_secs(3);

/// 記録済みの which 結果がまだ有効か(= which() の再実行を省けるか)。
/// `last_checked` が None(未確認)なら常に再確認する。
pub(super) fn which_result_is_fresh(
    last_checked: Option<Instant>,
    now: Instant,
    ttl: Duration,
) -> bool {
    match last_checked {
        Some(t) => now.saturating_duration_since(t) < ttl,
        None => false,
    }
}

/// コマンドが PATH 上に存在するか。
///
/// 実体の探索は [`shellenv::which`] に任せる (OS ごとの分岐と、GUI 起動で
/// 痩せた PATH の補い方はあちらに集約してある)。サブプロセスを起こさないので
/// 毎フレーム呼んでも安全だが、TTL キャッシュはそのまま残してある。
pub(super) fn which(bin: &str) -> bool {
    shellenv::has(bin)
}

/// テーマ名を解決する。VS Code テーマJSONへのパスならそれを読み込み、
/// 失敗時・それ以外はビルトインテーマ名として解決する。
pub(super) fn resolve_theme(name: &str) -> Theme {
    if name.ends_with(".json") || name.contains('/') || name.contains('\\') {
        if let Ok(t) = theme_json::load(Path::new(name)) {
            return t;
        }
    }
    theme::by_name(name)
}

/// フォント候補を先頭から試し、最初に読めたものを `name` として登録し、
/// Proportional / Monospace 両方の **主フォントのすぐ後ろ** へ積む。
///
/// 末尾に積んではいけない: egui 同梱の `NotoEmoji-Regular` (`FontTweak.scale`
/// 0.81) と `emoji-icon-font` (0.90) が既に列に居るため、`✓ ✕ ▸` や罫線が
/// **縮小された絵文字フォント**で解決され、本文の中で 1 文字だけ小さく
/// 沈んで見える (= 「文字がガタガタ」の主因のひとつ)。主フォントの直後に
/// 差し込めば、主フォントが持たない字だけを等倍の実フォントが拾う。
/// 読めた候補のパスを返す (呼び出し側が二重読み込みを避けるため)。
pub(super) fn push_fallback_font<'a>(
    fonts: &mut egui::FontDefinitions,
    name: &str,
    candidates: &[&'a str],
) -> Option<&'a str> {
    for p in candidates {
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        fonts
            .font_data
            .insert(name.to_owned(), egui::FontData::from_owned(bytes));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&fam) {
                // 主フォント (index 0) の直後。列が空でも panic しないよう clamp。
                let at = list.len().min(1);
                list.insert(at, name.to_owned());
            }
        }
        return Some(p);
    }
    None
}

/// 読めた候補を **全部** `name0, name1, …` として積む版。
/// 記号はフォントごとに持っている字がバラバラで、しかも OS の版で変わる
/// (macOS 26 では Apple Symbols / Arial Unicode から ❯ U+276F が消えた、実測)。
/// 1 本目で止めるとどこかの環境で豆腐が出るので、和集合を取る。
/// `skip` は既に別名で積んだ実体 (同じ 20MB 級ファイルを二重に読まないため)。
///
/// `start_at` は挿入開始位置 — [`push_fallback_font`] と同じ理由で、egui 同梱の
/// 縮小絵文字フォントより **前** へ入れる。候補どうしの優先順は呼ばれた順のまま。
pub(super) fn push_fallback_fonts_all(
    fonts: &mut egui::FontDefinitions,
    name: &str,
    candidates: &[&str],
    skip: Option<&str>,
    start_at: usize,
) -> usize {
    let mut n = 0;
    for p in candidates {
        if Some(*p) == skip {
            continue;
        }
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        let key = format!("{name}{n}");
        fonts
            .font_data
            .insert(key.clone(), egui::FontData::from_owned(bytes));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&fam) {
                list.insert((start_at + n).min(list.len()), key.clone());
            }
        }
        n += 1;
    }
    n
}

/// 画面全体のズームを egui へ反映する。
///
/// egui は `pixels_per_point = zoom_factor × ネイティブ DPI` で描くので、
/// これ 1 本で UI の全部 — サイドバー・タブ・メニュー・ターミナル・エディタ —
/// が一緒に拡大縮小する。フォントサイズと余白の物理ピクセル丸め直しは
/// `theme::resync_pixel_snapping` のフレーム先頭フックが自動で追随するため、
/// ここでテーマを焼き直す必要はない。
///
/// **egui 内蔵のキーボードズームは切る。** egui は既定で `end_pass` に
/// ⌘+/⌘-/⌘0 を拾って `zoom_factor` を直接書き換えるが、それを残すと
/// 「アプリが `Config::ui_zoom` から入れた値」と「egui が勝手に入れた値」が
/// 毎フレーム押し合いになり、倍率が戻る / 保存されないという形で壊れる。
/// ズームの所有者は `Config::ui_zoom` 1 つに絞る。
///
/// `set_zoom_factor` は値が同じなら再描画も要求しないので、毎フレーム
/// 呼んでもアイドル時のコストはゼロのまま。
pub(super) fn apply_ui_zoom(ctx: &egui::Context, z: f32) {
    ctx.options_mut(|o| o.zoom_with_keyboard = false);
    ctx.set_zoom_factor(zoom::clamp(z));
}

pub(super) fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates: Vec<&str> = if cfg!(target_os = "macos") {
        vec![
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
            "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
        ]
    } else if cfg!(target_os = "windows") {
        vec![
            "C:/Windows/Fonts/YuGothM.ttc",
            "C:/Windows/Fonts/meiryo.ttc",
            "C:/Windows/Fonts/msgothic.ttc",
        ]
    } else {
        vec![
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJKjp-Regular.otf",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/fonts-japanese-gothic.ttf",
        ]
    };
    let cjk_loaded = push_fallback_font(&mut fonts, "cjk", &candidates);

    // 記号フォント。egui 同梱の Ubuntu-Light / NotoEmoji / emoji-icon-font にも
    // 日本語フォントにも無い記号 (✕ ✗ ⌫ ⌥ ⌃ ❯ ▸ ▾ 罫線 点字スピナー など) は、
    // これを積まないと豆腐 (□) になる。「✕ 閉じる」が読めなくなるのが典型例。
    // 1 本では足りない: macOS 26 で Apple Symbols から ❯ が消えた (Menlo にはある)
    // ように、OS 更新でグリフ構成が変わるので、読めた候補は全部積んで和を取る。
    let symbols: Vec<&str> = if cfg!(target_os = "macos") {
        vec![
            "/System/Library/Fonts/Apple Symbols.ttf",
            "/System/Library/Fonts/Menlo.ttc",
            "/System/Library/Fonts/Supplemental/Symbol.ttf",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        ]
    } else if cfg!(target_os = "windows") {
        vec![
            "C:/Windows/Fonts/seguisym.ttf",
            "C:/Windows/Fonts/consola.ttf",
            "C:/Windows/Fonts/segoeui.ttf",
            "C:/Windows/Fonts/arial.ttf",
        ]
    } else {
        vec![
            "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
            "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
        ]
    };
    // 主フォント (0) と、読めていれば CJK (1) の後ろから記号を積む。
    push_fallback_fonts_all(
        &mut fonts,
        "symbols",
        &symbols,
        cjk_loaded,
        1 + usize::from(cjk_loaded.is_some()),
    );

    // ── Windows: 本文の Proportional を OS の日本語フェイスそのものにする ──
    // epaint はフェイスごとに ascent が違う値で行内へ置くため、ラテンと日本語が
    // **別フェイス**だと同じ行に 2 つのベースラインが生まれ、「あ」と「a」が
    // 上下にずれて並ぶ (Windows の Yu Gothic × Ubuntu-Light で顕著)。
    // 日本語フェイスはラテンも持っているので、先頭へ回せば 1 フェイスで揃う。
    // Monospace は触らない — 桁の等幅性のほうが優先される。
    #[cfg(target_os = "windows")]
    if fonts.font_data.contains_key("cjk") {
        if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            list.retain(|n| n != "cjk");
            list.insert(0, "cjk".to_owned());
        }
    }

    ctx.set_fonts(fonts);
}
