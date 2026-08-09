//! ブックマーク (JetBrains の Bookmarks 相当) — ニーモニック付きの行印。
//!
//! `editor::Bookmarks` は「いま開いているバッファの行番号の集合」でしかなく、
//! タブを閉じれば消え、外からファイルが書き換わればずれる。ここはその上位で、
//! **プロジェクト全体**の印を持ち、編集・一括置換・ブランチ切替を跨いで
//! 行を追いかけ、`~/.zaivern` へ永続化する。
//!
//! ## 設計の要点 (IntelliJ `platform/bookmarks` から採った形)
//!
//! * **印は不変値オブジェクト。** 「動かす」は差し替え、`None` への差し替えは削除。
//!   キーは (ファイル, 行) で、`Mark` 自身は書き換えずに作り直す。
//! * **アンカーは 2 本。**
//!   1. 行番号 (編集の増減は [`crate::editor::remap_line`] と同じ規約でずらす)
//!   2. [`Mark::expected_text`] — **変更前**に控えた行の本文。永続化もする。
//!   行番号だけでは外部からの書き換え (git checkout・整形) に耐えられない。
//! * **追えなくなった印は捨てずに [`InvalidMark`] として残す。**
//!   捨ててしまうと「ブランチを戻したら消えていた」になる。残しておけば
//!   [`MarkStore::resurrect`] が挿入行から拾い直せる。
//! * **編集経路で本文を走査しない。** 検証は [`VALIDATE_DEBOUNCE_MS`]、
//!   追悼は [`MEMORIAL_DEBOUNCE_MS`] で束ね、重い差分は
//!   [`plan_bulk`] (純粋関数) をバックグラウンドスレッドで回す。
//!   このリポジトリは同期 git を UI スレッドで撃って 6023ms 固めた前科がある。

use crate::i18n::{tr, trf};
use crate::theme::Theme;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

// ===========================================================================
// 1. ニーモニック
// ===========================================================================

/// ニーモニックの総数 — 数字 `0-9` と英字 `A-Z` で 36。
pub const MNEMONIC_COUNT: usize = 36;

/// 印に付ける 1 文字の見出し。**プロジェクト内で一意**。
///
/// 小文字は大文字へ畳んでから持つので、`a` と `A` は同じニーモニック。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(try_from = "char", into = "char")]
pub struct Mnemonic(char);

impl Mnemonic {
    /// `0-9` / `A-Z` (小文字も可) から作る。それ以外は `None`。
    pub fn new(c: char) -> Option<Self> {
        let c = c.to_ascii_uppercase();
        (c.is_ascii_digit() || c.is_ascii_uppercase()).then_some(Self(c))
    }

    /// 表示に使う 1 文字。
    pub fn ch(self) -> char {
        self.0
    }

    /// 0〜35 の通し番号 (数字が先、英字が後)。
    pub fn index(self) -> usize {
        if self.0.is_ascii_digit() {
            self.0 as usize - '0' as usize
        } else {
            10 + (self.0 as usize - 'A' as usize)
        }
    }

    /// 数字ニーモニックなら 0〜9。英字なら `None`。
    pub fn digit(self) -> Option<u8> {
        self.0.is_ascii_digit().then(|| self.0 as u8 - b'0')
    }
}

impl std::fmt::Display for Mnemonic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<char> for Mnemonic {
    type Error = String;
    fn try_from(c: char) -> Result<Self, String> {
        Self::new(c).ok_or_else(|| format!("ニーモニックにできない文字: {c:?}"))
    }
}

impl From<Mnemonic> for char {
    fn from(m: Mnemonic) -> char {
        m.0
    }
}

// ===========================================================================
// 2. 印そのもの (不変値オブジェクト)
// ===========================================================================

fn default_group() -> String {
    "default".to_string()
}

/// 行ブックマーク 1 件。**書き換えずに作り直す**のが約束。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    /// 対象ファイル (絶対パス)。
    pub file: PathBuf,
    /// 0 始まりの行番号 (アンカー 1)。
    pub line: usize,
    /// 印を付けた時点の行の本文 (アンカー 2)。**永続化する**。
    #[serde(default)]
    pub expected_text: String,
    /// ニーモニック。無印なら `None`。
    #[serde(default)]
    pub mnemonic: Option<Mnemonic>,
    /// 説明。切替時に選択範囲があればそれが入る。
    #[serde(default)]
    pub description: String,
    /// 所属グループ。
    #[serde(default = "default_group")]
    pub group: String,
}

/// 追えなくなった印。**一覧から黙って消さない** — 挿入行から復活しうる。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvalidMark {
    pub file: PathBuf,
    /// 見失った時点の行 (0 始まり)。
    pub line: usize,
    pub expected_text: String,
    #[serde(default)]
    pub mnemonic: Option<Mnemonic>,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_group")]
    pub group: String,
}

impl Mark {
    fn invalidate(&self) -> InvalidMark {
        InvalidMark {
            file: self.file.clone(),
            line: self.line,
            expected_text: self.expected_text.clone(),
            mnemonic: self.mnemonic,
            description: self.description.clone(),
            group: self.group.clone(),
        }
    }
}

impl InvalidMark {
    fn revive(&self, line: usize) -> Mark {
        Mark {
            file: self.file.clone(),
            line,
            expected_text: self.expected_text.clone(),
            mnemonic: self.mnemonic,
            description: self.description.clone(),
            group: self.group.clone(),
        }
    }
}

/// 説明欄に入れられる長さの上限 (選択範囲をそのまま入れると際限が無い)。
const DESC_CAP: usize = 160;

/// 選択文字列を説明として使える形へ畳む。空白だけなら `None`。
pub fn description_from_selection(sel: &str) -> Option<String> {
    let one: String = sel
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let t = one.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        return None;
    }
    Some(t.chars().take(DESC_CAP).collect())
}

/// `text` の `line` 行目 (0 始まり)。範囲外は `None`。
pub fn line_text(text: &str, line: usize) -> Option<&str> {
    text.split('\n').nth(line)
}

// ===========================================================================
// 3. 保管庫と切替の意味論
// ===========================================================================

/// 切替 1 回の要求。
#[derive(Debug, Clone)]
pub struct ToggleRequest<'a> {
    pub file: &'a Path,
    /// 0 始まりの行。
    pub line: usize,
    /// 付けたいニーモニック。無印の印なら `None`。
    pub mnemonic: Option<Mnemonic>,
    /// その行の本文 (`expected_text` の元)。
    pub line_text: &'a str,
    /// エディタの選択文字列。空白のみなら説明に使わない。
    pub selection: Option<&'a str>,
    /// 既に他所で使われているニーモニックを黙って奪ってよいか
    /// (「次回から確認しない」がオンのときに真)。
    pub overwrite: bool,
}

/// 切替の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToggleOutcome {
    /// 新しく付いた。
    Added,
    /// 同じ種類だったので外した。
    Removed,
    /// 種類が変わったので付け替えた。
    Reassigned { from: Option<Mnemonic> },
    /// そのニーモニックは別の行が持っている。上書き確認が要る。
    NeedsConfirm {
        holder_file: PathBuf,
        holder_line: usize,
    },
}

/// 更新 1 回の内訳 (トーストと테스트のための数え上げ)。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UpdateReport {
    pub moved: usize,
    pub invalidated: usize,
    pub collapsed: usize,
    pub revived: usize,
    pub unchanged: usize,
}

impl UpdateReport {
    /// 何か動いたか (トーストを出すかの判定)。
    pub fn changed(&self) -> bool {
        self.moved + self.invalidated + self.collapsed + self.revived > 0
    }
}

/// プロジェクト 1 つぶんの印。
#[derive(Debug, Default, Clone)]
pub struct MarkStore {
    marks: Vec<Mark>,
    invalid: Vec<InvalidMark>,
    /// 追悼を既に出した (ファイル, 旧本文)。**一度きり**の番人。
    memorial_done: HashSet<(PathBuf, String)>,
    dirty: bool,
}

impl MarkStore {
    pub fn marks(&self) -> &[Mark] {
        &self.marks
    }

    pub fn invalid(&self) -> &[InvalidMark] {
        &self.invalid
    }

    pub fn is_empty(&self) -> bool {
        self.marks.is_empty() && self.invalid.is_empty()
    }

    /// 保存が要るか。保存側が読んだら [`Self::clear_dirty`] で落とす。
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    pub fn at(&self, file: &Path, line: usize) -> Option<&Mark> {
        self.marks.iter().find(|m| m.file == file && m.line == line)
    }

    /// ニーモニックの持ち主。**プロジェクト内で高々 1 件**。
    pub fn by_mnemonic(&self, m: Mnemonic) -> Option<&Mark> {
        self.marks.iter().find(|x| x.mnemonic == Some(m))
    }

    /// 使用中のニーモニック一覧 (選択ポップアップの「もう埋まっている」表示用)。
    pub fn assigned(&self) -> HashSet<Mnemonic> {
        self.marks.iter().filter_map(|m| m.mnemonic).collect()
    }

    /// 1 ファイルぶんの (行 → 表示文字) 表。ガターが引く。
    /// 無印の印は `◆`、ニーモニック付きはその文字。
    pub fn glyphs(&self, file: &Path) -> HashMap<usize, char> {
        self.marks
            .iter()
            .filter(|m| m.file == file)
            .map(|m| (m.line, m.mnemonic.map(|x| x.ch()).unwrap_or(ANON_GLYPH)))
            .collect()
    }

    /// 印の付け外し。JetBrains と同じ 3 通り:
    /// 印が無ければ**追加**、同じ種類なら**削除**、違う種類なら**付け替え**。
    ///
    /// 選択範囲が空白でなければ、その文字列が説明になる。
    pub fn toggle(&mut self, req: &ToggleRequest<'_>) -> ToggleOutcome {
        let here = self
            .marks
            .iter()
            .position(|m| m.file == req.file && m.line == req.line);

        // ニーモニックの一意性 — 他の行が持っていれば確認するか奪う。
        let holder = req.mnemonic.and_then(|mn| {
            self.marks
                .iter()
                .position(|m| m.mnemonic == Some(mn))
                .filter(|i| Some(*i) != here)
        });
        if let Some(h) = holder {
            if !req.overwrite {
                return ToggleOutcome::NeedsConfirm {
                    holder_file: self.marks[h].file.clone(),
                    holder_line: self.marks[h].line,
                };
            }
        }

        let desc = req.selection.and_then(description_from_selection);

        match here {
            None => {
                if let Some(h) = holder {
                    self.marks[h].mnemonic = None;
                }
                self.marks.push(Mark {
                    file: req.file.to_path_buf(),
                    line: req.line,
                    expected_text: req.line_text.to_string(),
                    mnemonic: req.mnemonic,
                    description: desc.unwrap_or_default(),
                    group: default_group(),
                });
                self.dirty = true;
                ToggleOutcome::Added
            }
            Some(i) if self.marks[i].mnemonic == req.mnemonic => {
                self.marks.remove(i);
                self.dirty = true;
                ToggleOutcome::Removed
            }
            Some(i) => {
                if let Some(h) = holder {
                    self.marks[h].mnemonic = None;
                }
                // 添字は holder を消していないのでずれない (mnemonic を落としただけ)
                let from = self.marks[i].mnemonic;
                self.marks[i].mnemonic = req.mnemonic;
                self.marks[i].expected_text = req.line_text.to_string();
                if let Some(d) = desc {
                    self.marks[i].description = d;
                }
                self.dirty = true;
                ToggleOutcome::Reassigned { from }
            }
        }
    }

    /// 1 件を消す (一覧の × ボタン)。
    pub fn remove_at(&mut self, file: &Path, line: usize) -> bool {
        let n = self.marks.len();
        self.marks.retain(|m| !(m.file == file && m.line == line));
        self.dirty |= self.marks.len() != n;
        self.marks.len() != n
    }

    /// 無効な印を 1 件消す。
    pub fn remove_invalid(&mut self, at: usize) -> bool {
        if at >= self.invalid.len() {
            return false;
        }
        self.invalid.remove(at);
        self.dirty = true;
        true
    }

    /// 全部消す。
    pub fn clear(&mut self) {
        self.dirty |= !self.is_empty();
        self.marks.clear();
        self.invalid.clear();
        self.memorial_done.clear();
    }

    // ── 経路 1: 通常の編集 (行の増減が分かっている) ──────────────────

    /// 挿入 / 削除の (位置, 増減) から印を追従させる。
    ///
    /// * 行が消えた → [`InvalidMark`] へ (**捨てない**)
    /// * 2 つが同じ行に重なった → **後ろ側**を消す
    /// * それ以外で動いた → 行を差し替え、`expected_text` を読み直す
    /// * 動いていない → 何もしない (行そのものの書き換えは追悼パスの仕事)
    ///
    /// 挿入があれば、その行から [`InvalidMark`] の復活も試す。
    pub fn on_edit(
        &mut self,
        file: &Path,
        at: usize,
        delta: isize,
        new_text: &str,
    ) -> UpdateReport {
        let lines: Vec<&str> = new_text.split('\n').collect();
        let mut rep = UpdateReport::default();

        let mut idx: Vec<usize> = (0..self.marks.len())
            .filter(|i| self.marks[*i].file == file)
            .collect();
        idx.sort_by_key(|i| self.marks[*i].line);

        let mut taken: HashSet<usize> = HashSet::new();
        let mut drop: Vec<usize> = Vec::new();
        let mut dead: Vec<InvalidMark> = Vec::new();
        for i in idx {
            let old = self.marks[i].line;
            match crate::editor::remap_line(old, at, delta) {
                None => {
                    dead.push(self.marks[i].invalidate());
                    drop.push(i);
                    rep.invalidated += 1;
                }
                Some(nl) if !taken.insert(nl) => {
                    // 先に来た (= 元が前の行だった) 方を残し、後ろ側を消す。
                    //
                    // `remap_line` 自体は単射なので、**まっとうな入力では
                    // ここへ来ない**。来るのは保存ファイルに同じ行が 2 件
                    // 入っていたとき (手で編集された・旧版の取りこぼし) で、
                    // 一覧に同じ行が二重に並ぶのを構造的に防ぐための番人。
                    // 本物の「重なり」は全文置換の経路 ([`plan_bulk`]) が出す。
                    drop.push(i);
                    rep.collapsed += 1;
                }
                Some(nl) if nl == old => rep.unchanged += 1,
                Some(nl) => match lines.get(nl) {
                    Some(t) => {
                        self.marks[i].line = nl;
                        self.marks[i].expected_text = (*t).to_string();
                        rep.moved += 1;
                    }
                    None => {
                        dead.push(self.marks[i].invalidate());
                        drop.push(i);
                        rep.invalidated += 1;
                    }
                },
            }
        }
        drop.sort_unstable();
        for i in drop.into_iter().rev() {
            self.marks.remove(i);
        }
        self.invalid.extend(dead);

        // 挿入された行から復活を試す (ブランチを戻すと印が帰ってくる経路)
        if delta > 0 {
            let ins: Vec<usize> = (at..at + delta as usize).collect();
            rep.revived += self.resurrect(file, &lines, &ins);
        }
        if rep.changed() {
            self.dirty = true;
        }
        rep
    }

    // ── 経路 2: 全文置換 / 一括更新 ──────────────────────────────────

    /// 差分計画 ([`plan_bulk`]) を当てる。計画づくりは重いので別スレッドで、
    /// 適用 (ここ) は軽いので UI スレッドで、という分け方。
    pub fn apply_bulk(&mut self, file: &Path, new_text: &str, plan: &BulkPlan) -> UpdateReport {
        let mut rep = UpdateReport::default();
        for (old, new, text) in &plan.moves {
            if let Some(m) = self
                .marks
                .iter_mut()
                .find(|m| m.file == file && m.line == *old)
            {
                if m.line == *new && m.expected_text == *text {
                    rep.unchanged += 1;
                } else {
                    m.line = *new;
                    m.expected_text = text.clone();
                    rep.moved += 1;
                }
            }
        }
        let mut dead: Vec<InvalidMark> = Vec::new();
        for old in &plan.invalidated {
            if let Some(i) = self
                .marks
                .iter()
                .position(|m| m.file == file && m.line == *old)
            {
                dead.push(self.marks[i].invalidate());
                self.marks.remove(i);
                rep.invalidated += 1;
            }
        }
        for old in &plan.collapsed {
            if let Some(i) = self
                .marks
                .iter()
                .position(|m| m.file == file && m.line == *old)
            {
                self.marks.remove(i);
                rep.collapsed += 1;
            }
        }
        self.invalid.extend(dead);

        // 復活は「今の無効一覧」に対して引き直す (計画時点の添字は当てにしない)
        let lines: Vec<&str> = new_text.split('\n').collect();
        let occupied: HashSet<usize> = self
            .marks
            .iter()
            .filter(|m| m.file == file)
            .map(|m| m.line)
            .collect();
        let ins: Vec<usize> = (0..lines.len()).filter(|l| !occupied.contains(l)).collect();
        rep.revived += self.resurrect(file, &lines, &ins);

        if rep.changed() {
            self.dirty = true;
        }
        rep
    }

    /// 全文置換をこの場で 1 本で処理する。
    ///
    /// 通常は [`plan_bulk`] をバックグラウンドスレッドで回して
    /// [`Self::apply_bulk`] を当てる。ここを直に呼ぶのは
    /// **スレッドを起こせなかったときの後退路**とテストだけ。
    pub fn on_bulk(&mut self, file: &Path, old_text: &str, new_text: &str) -> UpdateReport {
        let mine: Vec<Mark> = self
            .marks
            .iter()
            .filter(|m| m.file == file)
            .cloned()
            .collect();
        let plan = plan_bulk(old_text, new_text, &mine);
        self.apply_bulk(file, new_text, &plan)
    }

    // ── 経路 3: 復活 ────────────────────────────────────────────────

    /// 挿入された行の本文が [`InvalidMark::expected_text`] と一致したら復活させる。
    /// **ブランチを戻すと印が帰ってくる**のはここ。
    fn resurrect(&mut self, file: &Path, lines: &[&str], inserted: &[usize]) -> usize {
        if inserted.is_empty() {
            return 0;
        }
        let mut occupied: HashSet<usize> = self
            .marks
            .iter()
            .filter(|m| m.file == file)
            .map(|m| m.line)
            .collect();
        let used: HashSet<Mnemonic> = self.marks.iter().filter_map(|m| m.mnemonic).collect();
        let mut back: Vec<Mark> = Vec::new();
        let mut gone: Vec<usize> = Vec::new();
        for (k, inv) in self.invalid.iter().enumerate() {
            if inv.file != file || inv.expected_text.is_empty() {
                continue;
            }
            let hit = inserted.iter().copied().find(|l| {
                !occupied.contains(l) && lines.get(*l) == Some(&inv.expected_text.as_str())
            });
            if let Some(l) = hit {
                let mut m = inv.revive(l);
                // ニーモニックの一意性を壊さない (留守中に奪われていたら無印で戻す)
                if m.mnemonic.map(|x| used.contains(&x)).unwrap_or(false) {
                    m.mnemonic = None;
                }
                occupied.insert(l);
                back.push(m);
                gone.push(k);
            }
        }
        for k in gone.iter().rev() {
            self.invalid.remove(*k);
        }
        let n = back.len();
        self.marks.extend(back);
        n
    }

    // ── 追悼 (memorial) ─────────────────────────────────────────────

    /// ブックマークした行そのものが書き換わっていたら、新しい本文で
    /// `expected_text` を作り直しつつ、**旧本文を持つ無効な印**を同じ
    /// グループへ足す。同じ旧本文につき **1 度だけ**。
    ///
    /// 旧本文の側にニーモニックは持たせない — ニーモニックはプロジェクトで
    /// 一意なので、複製すると 2 か所が同じ文字を名乗ることになる。
    pub fn memorial_pass(&mut self, file: &Path, new_text: &str) -> usize {
        let lines: Vec<&str> = new_text.split('\n').collect();
        let mut added: Vec<InvalidMark> = Vec::new();
        for m in self.marks.iter_mut().filter(|m| m.file == file) {
            let Some(now) = lines.get(m.line) else {
                continue;
            };
            if m.expected_text.is_empty() || *now == m.expected_text {
                continue;
            }
            let key = (m.file.clone(), m.expected_text.clone());
            if self.memorial_done.insert(key) {
                added.push(InvalidMark {
                    file: m.file.clone(),
                    line: m.line,
                    expected_text: m.expected_text.clone(),
                    mnemonic: None,
                    description: m.description.clone(),
                    group: m.group.clone(),
                });
            }
            m.expected_text = (*now).to_string();
        }
        let n = added.len();
        self.invalid.extend(added);
        if n > 0 {
            self.dirty = true;
        }
        n
    }
}

/// 無印 (ニーモニック無し) の印をガターに描くときの文字。
pub const ANON_GLYPH: char = '◆';

// ===========================================================================
// 4. 差分による行の写像 (経路 2 の中身。純粋関数)
// ===========================================================================

/// 一括更新の計画。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BulkPlan {
    /// (元の行, 新しい行, 新しい本文)
    pub moves: Vec<(usize, usize, String)>,
    /// 追えなくなった元の行
    pub invalidated: Vec<usize>,
    /// 重なって消える元の行
    pub collapsed: Vec<usize>,
}

/// LCS を諦める閾値 (旧行数 × 新行数)。これを超えたら共通接頭辞 / 接尾辞
/// だけで写し、残りは本文走査に任せる。**UI を止めないための上限**。
const LCS_CELL_CAP: usize = 4_000_000;

/// 本文走査で外側へ広げる最大距離。これを超えたら諦めて無効にする。
const SCAN_RADIUS: usize = 5_000;

/// 旧行 → 新行の対応表。対応が付かない行は `None`。
///
/// 共通の接頭辞・接尾辞を先に削ってから、残りに LCS を掛ける。
/// 残りが大きすぎるとき ([`LCS_CELL_CAP`]) は LCS を諦め、削った両端だけを返す。
pub fn map_lines(old: &[&str], new: &[&str]) -> Vec<Option<usize>> {
    let mut out = vec![None; old.len()];
    let mut pre = 0usize;
    while pre < old.len() && pre < new.len() && old[pre] == new[pre] {
        out[pre] = Some(pre);
        pre += 1;
    }
    let mut suf = 0usize;
    while suf < old.len() - pre
        && suf < new.len() - pre
        && old[old.len() - 1 - suf] == new[new.len() - 1 - suf]
    {
        out[old.len() - 1 - suf] = Some(new.len() - 1 - suf);
        suf += 1;
    }
    let (a, b) = (&old[pre..old.len() - suf], &new[pre..new.len() - suf]);
    if a.is_empty() || b.is_empty() {
        return out;
    }
    if a.len().saturating_mul(b.len()) > LCS_CELL_CAP {
        return out;
    }
    // 素直な LCS の DP。行単位なので幅は行数、要素は u32 で足りる。
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * (m + 1) + j] = if a[i] == b[j] {
                dp[(i + 1) * (m + 1) + j + 1] + 1
            } else {
                dp[(i + 1) * (m + 1) + j].max(dp[i * (m + 1) + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out[pre + i] = Some(pre + j);
            i += 1;
            j += 1;
        } else if dp[(i + 1) * (m + 1) + j] >= dp[i * (m + 1) + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// `preferred` から距離 1, 2, 3… と**外側へ**広げて `want` と全行一致する行を探す。
/// 同じ距離なら**手前 (`preferred - d`) が先**。
pub fn scan_outward(lines: &[&str], preferred: usize, want: &str) -> Option<usize> {
    if want.is_empty() || lines.is_empty() {
        return None;
    }
    let p = preferred.min(lines.len() - 1);
    if lines[p] == want {
        return Some(p);
    }
    let far = p.max(lines.len() - 1 - p).min(SCAN_RADIUS);
    for d in 1..=far {
        if let Some(k) = p.checked_sub(d) {
            if lines[k] == want {
                return Some(k);
            }
        }
        let k = p + d;
        if k < lines.len() && lines[k] == want {
            return Some(k);
        }
    }
    None
}

/// 全文置換の計画を立てる。**純粋関数** — 重いのはここだけなので、
/// バックグラウンドスレッドから呼ぶ ([`MarksState::poll`] が受け取る)。
pub fn plan_bulk(old_text: &str, new_text: &str, marks: &[Mark]) -> BulkPlan {
    let old: Vec<&str> = old_text.split('\n').collect();
    let new: Vec<&str> = new_text.split('\n').collect();
    let map = map_lines(&old, &new);
    let mut plan = BulkPlan::default();
    let mut order: Vec<&Mark> = marks.iter().collect();
    order.sort_by_key(|m| m.line);
    let mut taken: HashSet<usize> = HashSet::new();
    for m in order {
        let preferred = preferred_line(&map, m.line, new.len());
        let hit = match new.get(preferred) {
            Some(t) if *t == m.expected_text => Some(preferred),
            _ => scan_outward(&new, preferred, &m.expected_text),
        };
        // expected_text が空 (旧形式からの移行直後) なら写像だけを信じる
        let hit = hit.or_else(|| {
            m.expected_text
                .is_empty()
                .then(|| map.get(m.line).copied().flatten())
                .flatten()
        });
        match hit {
            None => plan.invalidated.push(m.line),
            Some(l) if !taken.insert(l) => plan.collapsed.push(m.line),
            Some(l) => plan.moves.push((m.line, l, new[l].to_string())),
        }
    }
    plan
}

/// 写像が付かなかった行の「たぶんこの辺」を出す。
/// 直前の対応行からのずれを引き継ぎ、無ければ後ろの対応行から逆算する。
fn preferred_line(map: &[Option<usize>], line: usize, new_len: usize) -> usize {
    let clamp = |v: isize| v.clamp(0, new_len.saturating_sub(1) as isize) as usize;
    if let Some(Some(v)) = map.get(line) {
        return clamp(*v as isize);
    }
    for k in (0..line.min(map.len())).rev() {
        if let Some(v) = map[k] {
            return clamp(v as isize + (line - k) as isize);
        }
    }
    for (k, m) in map.iter().enumerate().skip(line + 1) {
        if let Some(v) = m {
            return clamp(*v as isize - (k - line) as isize);
        }
    }
    clamp(line as isize)
}

// ===========================================================================
// 5. デバウンス — 「編集経路で走査しない」ための時間定数
// ===========================================================================

/// ファイル変更を見てから**検証**を走らせるまでの待ち時間 (ミリ秒)。
///
/// IntelliJ の `BookmarksManager` と同じ **100ms**。追加のたびにタイマーを
/// 張り直す (restart-on-add) ので、連続した打鍵は 1 回にまとまる。
/// 短くすると打鍵ごとに差分が走り、長くするとガターの印が目に見えて遅れる。
pub const VALIDATE_DEBOUNCE_MS: u64 = 100;

/// **追悼パス**を走らせるまでの待ち時間 (ミリ秒)。
///
/// IntelliJ と同じ **2000ms**。「行を書き換えている最中」の中間状態を
/// 旧本文として残さないための猶予で、検証 (100ms) よりずっと長い。
/// 保存の直前にも 1 回流す (待ち時間の途中で終了しても取りこぼさない)。
pub const MEMORIAL_DEBOUNCE_MS: u64 = 2000;

// ===========================================================================
// 6. 永続化 (バージョン付き + 移行)
// ===========================================================================

/// 保存形式の版。**上げたら [`parse_storage`] に移行を足すこと**。
///
/// * v1: `bookmarks = [{ file, line (1 始まり), mnemonic }]` — 本文の控え無し
/// * v2: `marks` / `invalid`。行は 0 始まり、`expected_text` を持つ
pub const STORAGE_VERSION: u32 = 2;

#[derive(Serialize, Deserialize, Default)]
struct StoredDoc {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    marks: Vec<Mark>,
    #[serde(default)]
    invalid: Vec<InvalidMark>,
}

#[derive(Deserialize)]
struct StoredV1 {
    #[serde(default)]
    bookmarks: Vec<V1Mark>,
}

#[derive(Deserialize)]
struct V1Mark {
    file: PathBuf,
    /// v1 は **1 始まり**だった。
    line: usize,
    #[serde(default)]
    mnemonic: Option<char>,
    #[serde(default)]
    description: String,
}

/// 保存文字列 → 保管庫。壊れていれば空を返す (起動を止めない)。
pub fn parse_storage(text: &str) -> MarkStore {
    let version = toml::from_str::<toml::Value>(text)
        .ok()
        .and_then(|v| v.get("version").and_then(|x| x.as_integer()))
        .unwrap_or(0) as u32;
    if version <= 1 {
        let Ok(v1) = toml::from_str::<StoredV1>(text) else {
            return MarkStore::default();
        };
        return MarkStore {
            marks: v1
                .bookmarks
                .into_iter()
                .map(|b| Mark {
                    file: b.file,
                    // 1 始まり → 0 始まり
                    line: b.line.saturating_sub(1),
                    expected_text: String::new(),
                    mnemonic: b.mnemonic.and_then(Mnemonic::new),
                    description: b.description,
                    group: default_group(),
                })
                .collect(),
            ..MarkStore::default()
        };
    }
    let Ok(doc) = toml::from_str::<StoredDoc>(text) else {
        return MarkStore::default();
    };
    MarkStore {
        marks: doc.marks,
        invalid: doc.invalid,
        ..MarkStore::default()
    }
}

/// 保管庫 → 保存文字列。
pub fn render_storage(store: &MarkStore) -> String {
    let doc = StoredDoc {
        version: STORAGE_VERSION,
        marks: store.marks.clone(),
        invalid: store.invalid.clone(),
    };
    toml::to_string_pretty(&doc).unwrap_or_default()
}

/// 保存先ディレクトリ: `~/.zaivern/bookmarks`。
/// **パスは導出する** — ホームもユーザー名も直書きしない。
pub fn storage_dir() -> PathBuf {
    crate::config::zaivern_dir().join("bookmarks")
}

/// ワークスペース → 安定キー (`session.rs` と同じ流儀)。
fn workspace_key(workspace: &Path) -> String {
    let canonical = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let mut h = DefaultHasher::new();
    canonical.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// `dir` の中のこのワークスペースのファイル。
pub fn storage_file(dir: &Path, workspace: &Path) -> PathBuf {
    dir.join(format!("{}.toml", workspace_key(workspace)))
}

/// 読み込み。無ければ空。
pub fn load_from_dir(dir: &Path, workspace: &Path) -> MarkStore {
    match std::fs::read_to_string(storage_file(dir, workspace)) {
        Ok(t) => parse_storage(&t),
        Err(_) => MarkStore::default(),
    }
}

/// 保存要求 (書き込みスレッドへ渡す形)。
type SaveJob = (PathBuf, String);

/// 書き込み専用のスレッド。**UI スレッドでファイルを書かない**。
struct Saver {
    tx: Sender<SaveJob>,
}

impl Saver {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<SaveJob>();
        std::thread::Builder::new()
            .name("marks-save".into())
            .spawn(move || {
                while let Ok((path, text)) = rx.recv() {
                    // 溜まっていれば最後の 1 通だけを書く (連打を畳む)
                    let (mut path, mut text) = (path, text);
                    while let Ok(next) = rx.try_recv() {
                        path = next.0;
                        text = next.1;
                    }
                    if let Some(p) = path.parent() {
                        let _ = std::fs::create_dir_all(p);
                    }
                    let _ = std::fs::write(&path, text);
                }
            })
            .ok();
        Self { tx }
    }

    fn save(&self, path: PathBuf, text: String) {
        let _ = self.tx.send((path, text));
    }
}

// ===========================================================================
// 7. レイアウト (純粋関数)
// ===========================================================================

/// 分割バーの幅 (px)。
pub const SPLITTER_W: f32 = 6.0;
/// 片側の最小幅 (px)。これを割ったら縦積みへ切り替える。
pub const MIN_PANE_W: f32 = 200.0;

/// ブックマークパネルの分割。左がツリー、右がプレビュー。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelLayout {
    pub tree: egui::Rect,
    pub splitter: egui::Rect,
    pub preview: egui::Rect,
    /// 幅が足りず**上下**に積んだか。
    pub stacked: bool,
}

/// 可用領域と分割比から矩形を決める。
///
/// **どの幅でも見切れない**のが約束: 3 つの矩形は必ず `area` の中に収まり、
/// 互いに重ならない。狭いときは左右をやめて上下に積む (プレビューを
/// 消して空白を作らない)。
pub fn panel_layout(area: egui::Rect, ratio: f32) -> PanelLayout {
    let r = ratio.clamp(0.2, 0.8);
    if area.width() >= MIN_PANE_W * 2.0 + SPLITTER_W {
        let tw = ((area.width() - SPLITTER_W) * r)
            .clamp(MIN_PANE_W, area.width() - SPLITTER_W - MIN_PANE_W);
        let x0 = area.left() + tw;
        PanelLayout {
            tree: egui::Rect::from_min_max(area.min, egui::pos2(x0, area.bottom())),
            splitter: egui::Rect::from_min_max(
                egui::pos2(x0, area.top()),
                egui::pos2(x0 + SPLITTER_W, area.bottom()),
            ),
            preview: egui::Rect::from_min_max(egui::pos2(x0 + SPLITTER_W, area.top()), area.max),
            stacked: false,
        }
    } else {
        let th = ((area.height() - SPLITTER_W) * 0.6).max(0.0);
        let y0 = area.top() + th;
        PanelLayout {
            tree: egui::Rect::from_min_max(area.min, egui::pos2(area.right(), y0)),
            splitter: egui::Rect::from_min_max(
                egui::pos2(area.left(), y0),
                egui::pos2(area.right(), (y0 + SPLITTER_W).min(area.bottom())),
            ),
            preview: egui::Rect::from_min_max(
                egui::pos2(area.left(), (y0 + SPLITTER_W).min(area.bottom())),
                area.max,
            ),
            stacked: true,
        }
    }
}

/// 数字グリッドの並び (`123 / 456 / 789 / 0`)。
pub const DIGIT_ROWS: [&str; 4] = ["123", "456", "789", "0"];
/// 英字グリッドの並び (`A–G / H–N / O–U / V–Z`)。
pub const LETTER_ROWS: [&str; 4] = ["ABCDEFG", "HIJKLMN", "OPQRSTU", "VWXYZ"];

/// 1 マスの最大辺 (px)。
const CELL_MAX: f32 = 34.0;
/// 1 マスの最小辺 (px)。
const CELL_MIN: f32 = 18.0;
/// マスの間隔 (px)。
const CELL_GAP: f32 = 4.0;

/// ニーモニック選択ポップアップの格子。
#[derive(Debug, Clone, PartialEq)]
pub struct ChooserLayout {
    /// (ニーモニック, 行, 列, 矩形)。数字と英字を続けて並べる。
    pub cells: Vec<(Mnemonic, usize, usize, egui::Rect)>,
    /// 数字グリッドの行数 (英字グリッドはこの後ろに続く)。
    pub digit_rows: usize,
    /// 説明の入力欄。
    pub desc: egui::Rect,
}

/// 行ごとの列数 (矢印移動の折り返しに使う)。
pub fn chooser_row_widths() -> Vec<usize> {
    DIGIT_ROWS
        .iter()
        .chain(LETTER_ROWS.iter())
        .map(|r| r.chars().count())
        .collect()
}

/// 格子の矩形を決める。**可用幅に必ず収める** (最長行 = 7 列を基準に縮める)。
pub fn chooser_layout(area: egui::Rect) -> ChooserLayout {
    let cols = LETTER_ROWS
        .iter()
        .map(|r| r.chars().count())
        .max()
        .unwrap_or(7);
    let cell =
        ((area.width() - CELL_GAP * (cols as f32 - 1.0)) / cols as f32).clamp(CELL_MIN, CELL_MAX);
    let mut cells = Vec::with_capacity(MNEMONIC_COUNT);
    let mut y = area.top();
    let rows: Vec<&str> = DIGIT_ROWS
        .iter()
        .chain(LETTER_ROWS.iter())
        .copied()
        .collect();
    for (ri, row) in rows.iter().enumerate() {
        // 数字と英字の間に 1 段ぶんの隙間を入れる
        if ri == DIGIT_ROWS.len() {
            y += CELL_GAP * 2.0;
        }
        for (ci, ch) in row.chars().enumerate() {
            let Some(mn) = Mnemonic::new(ch) else {
                continue;
            };
            let x = area.left() + ci as f32 * (cell + CELL_GAP);
            cells.push((
                mn,
                ri,
                ci,
                egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell, cell)),
            ));
        }
        y += cell + CELL_GAP;
    }
    let desc = egui::Rect::from_min_size(
        egui::pos2(area.left(), y),
        egui::vec2(area.width(), (area.bottom() - y).clamp(0.0, 26.0)),
    );
    ChooserLayout {
        cells,
        digit_rows: DIGIT_ROWS.len(),
        desc,
    }
}

/// 矢印キーの移動。**端で折り返す**。`widths` は行ごとの列数。
pub fn grid_step(widths: &[usize], cur: (usize, usize), dx: isize, dy: isize) -> (usize, usize) {
    if widths.is_empty() {
        return (0, 0);
    }
    let rows = widths.len() as isize;
    let mut r = (cur.0 as isize + dy).rem_euclid(rows) as usize;
    // 空の行は飛ばす (定義上は無いが、表を書き換えたときに固まらないため)
    let mut guard = 0;
    while widths[r] == 0 && guard < widths.len() {
        r = (r as isize + if dy >= 0 { 1 } else { -1 }).rem_euclid(rows) as usize;
        guard += 1;
    }
    let w = widths[r].max(1) as isize;
    let c = if dy != 0 {
        (cur.1 as isize).min(w - 1)
    } else {
        (cur.1 as isize + dx).rem_euclid(w)
    };
    (r, c.clamp(0, w - 1) as usize)
}

// ===========================================================================
// 8. 並び順 (ディレクトリ → ファイル → 自然順 → 行番号)
// ===========================================================================

/// 数字を数値として比べる自然順比較 (`f2` < `f10`)。
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let na: String =
                        std::iter::from_fn(|| x.next_if(|c| c.is_ascii_digit())).collect();
                    let nb: String =
                        std::iter::from_fn(|| y.next_if(|c| c.is_ascii_digit())).collect();
                    let va = na.trim_start_matches('0');
                    let vb = nb.trim_start_matches('0');
                    let ord = va.len().cmp(&vb.len()).then_with(|| va.cmp(vb));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    let ord = ca
                        .to_ascii_lowercase()
                        .cmp(&cb.to_ascii_lowercase())
                        .then_with(|| ca.cmp(&cb));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    x.next();
                    y.next();
                }
            }
        }
    }
}

/// パスの並び: **同じ階層ではディレクトリが先**、名前は自然順。
pub fn path_cmp(a: &Path, b: &Path) -> Ordering {
    let ac: Vec<String> = a
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let bc: Vec<String> = b
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    for i in 0..ac.len().min(bc.len()) {
        let a_dir = i + 1 < ac.len();
        let b_dir = i + 1 < bc.len();
        if a_dir != b_dir {
            return if a_dir {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        match natural_cmp(&ac[i], &bc[i]) {
            Ordering::Equal => continue,
            o => return o,
        }
    }
    ac.len().cmp(&bc.len())
}

/// 一覧の 1 ファイルぶん。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileGroup {
    pub file: PathBuf,
    /// 表示に使う短い名前 (ワークスペース相対)。
    pub label: String,
    /// `MarkStore::marks()` への添字 (行番号の昇順)。
    pub rows: Vec<usize>,
}

/// 「ファイルでまとめる」表示のための並べ替え。
pub fn group_by_file(marks: &[Mark], root: &Path) -> Vec<FileGroup> {
    let mut by: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, m) in marks.iter().enumerate() {
        by.entry(m.file.clone()).or_default().push(i);
    }
    let mut out: Vec<FileGroup> = by
        .into_iter()
        .map(|(file, mut rows)| {
            rows.sort_by_key(|i| marks[*i].line);
            let label = rel_label(&file, root);
            FileGroup { file, label, rows }
        })
        .collect();
    out.sort_by(|a, b| path_cmp(Path::new(&a.label), Path::new(&b.label)));
    out
}

/// ワークスペース相対の表示名。外なら絶対のまま。
pub fn rel_label(file: &Path, root: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

// ===========================================================================
// 9. キーバインド — 数字ニーモニックへの直行打鍵
// ===========================================================================

/// 数字ニーモニックへ直行する打鍵の修飾キー。
///
/// **導出しない。実測で固定する。** IntelliJ の mac キーマップが
/// `control 0..9` をわざわざ再宣言しているのと同じ理由で、ここは
/// 自動変換に任せてはいけない:
///
/// * `keybinds::canonical_mods` は **非 macOS で ⌃ を ⌘ へ畳む**。
///   素の `⌃ + 数字` にすると Windows / Linux では `⌘ + 数字` =
///   `FocusPane1..3` と**同じ打鍵**になり、片方が永久に効かなくなる
///   (このリポジトリは `⌃⌘F` と `⌘F` で実際にこれを踏み、43,000 行の変更が
///   macOS では全部緑なのに CI の Linux / Windows で落ちた)。
/// * `⌃ + 数字` は **macOS では起動バー** (`QuickLaunch1..9`) が持っている。
/// * `⌃⌥ + 数字` は **非 macOS の起動バー**が持っている。
///
/// 残るのは `⌃⇧ + 数字`。macOS の実測予約表 (`keybinds::MACOS_RESERVED`) に
/// ⌃⇧ + 数字は 1 つも無い (取られているのは ⌃⇧Space)。非 macOS では
/// 畳まれて実体が `Ctrl+Shift+数字` になるが、既定表に ⌘⇧ + 数字は無いので
/// こちらも食い合わない。**両 OS で通る唯一の枠**なので、分岐せず固定する。
pub fn digit_jump_mods() -> egui::Modifiers {
    egui::Modifiers::CTRL.plus(egui::Modifiers::SHIFT)
}

/// 数字 `d` (0〜9) のニーモニックへ直行する打鍵。
pub fn digit_jump_shortcut(d: u8) -> Option<egui::KeyboardShortcut> {
    let key = match d {
        0 => egui::Key::Num0,
        1 => egui::Key::Num1,
        2 => egui::Key::Num2,
        3 => egui::Key::Num3,
        4 => egui::Key::Num4,
        5 => egui::Key::Num5,
        6 => egui::Key::Num6,
        7 => egui::Key::Num7,
        8 => egui::Key::Num8,
        9 => egui::Key::Num9,
        _ => return None,
    };
    Some(egui::KeyboardShortcut::new(digit_jump_mods(), key))
}

// ===========================================================================
// 10. アプリから見た状態 (デバウンス・バックグラウンド差分・UI)
// ===========================================================================

/// アプリ側にやってもらうこと。
#[derive(Debug, Clone, PartialEq)]
pub enum MarkAction {
    /// このファイルの 0 始まりの行へ飛ぶ。
    Goto(PathBuf, usize),
    /// トースト (第 2 引数が真なら成功色)。
    Toast(String, bool),
}

/// 画面に出す打鍵表記。**アプリがキーマップから作って渡す** —
/// このモジュールは打鍵をベタ書きしない。
#[derive(Debug, Clone, Default)]
pub struct Hints {
    /// ニーモニック付き切替の打鍵 (空状態の案内に出す)。
    pub toggle: String,
    /// 一覧を開く打鍵 (パネルのヘッダに出す)。
    pub panel: String,
}

/// 差分計算の結果 (バックグラウンドスレッド → UI スレッド)。
struct BulkDone {
    file: PathBuf,
    text: String,
    plan: BulkPlan,
}

/// 1 ファイルぶんの追跡状態。
struct Tracked {
    /// 最後に差分を取った時点の本文。
    snapshot: String,
    /// 直近に見た本文のハッシュ (毎フレームの比較はこれだけ = アイドル 0 コスト)。
    hash: u64,
    /// 変化を見た時刻 (検証デバウンスの起点)。
    seen: Instant,
    /// 追悼パスのデバウンス起点。
    memorial_at: Option<Instant>,
    /// 検証待ちか。
    pending: bool,
}

/// ニーモニック選択ポップアップの状態。
struct Chooser {
    file: PathBuf,
    line: usize,
    line_text: String,
    selection: Option<String>,
    desc: String,
    cursor: (usize, usize),
    /// 上書き確認の相手。
    confirm: Option<(PathBuf, usize)>,
}

/// アプリが 1 つ持つブックマークの状態。
pub struct MarksState {
    store: MarkStore,
    workspace: PathBuf,
    dir: PathBuf,
    saver: Option<Saver>,
    track: HashMap<PathBuf, Tracked>,
    rx: Option<Receiver<BulkDone>>,
    busy: bool,
    /// 一覧パネルを開いているか。
    pub panel_open: bool,
    /// ジャンプポップアップを開いているか。
    pub jump_open: bool,
    /// ファイルでまとめる (既定で有効)。
    pub group_by_file: bool,
    /// 一覧の分割比。
    split: f32,
    /// 一覧で選んでいる行 (`store.marks()` の添字)。
    selected: Option<usize>,
    chooser: Option<Chooser>,
    /// 「次回から確認しない」。
    pub always_overwrite: bool,
}

impl Default for MarksState {
    fn default() -> Self {
        Self {
            store: MarkStore::default(),
            workspace: PathBuf::new(),
            dir: storage_dir(),
            saver: None,
            track: HashMap::new(),
            rx: None,
            busy: false,
            panel_open: false,
            jump_open: false,
            group_by_file: true,
            split: 0.45,
            selected: None,
            chooser: None,
            always_overwrite: false,
        }
    }
}

impl MarksState {
    /// 参照だけ (ガター・ミニマップ)。
    pub fn store(&self) -> &MarkStore {
        &self.store
    }

    /// ワークスペースを切り替える (前のぶんは書き出す)。
    pub fn set_workspace(&mut self, workspace: &Path) {
        if self.workspace == workspace {
            return;
        }
        self.flush();
        self.workspace = workspace.to_path_buf();
        self.store = load_from_dir(&self.dir, &self.workspace);
        self.track.clear();
        self.selected = None;
    }

    /// テスト用に保存先を差し替える (実 `~/.zaivern` に触れないため)。
    #[cfg(test)]
    fn set_dir(&mut self, dir: &Path) {
        self.dir = dir.to_path_buf();
    }

    /// 溜まっている変更を書き出す。
    pub fn flush(&mut self) {
        if !self.store.dirty() || self.workspace.as_os_str().is_empty() {
            return;
        }
        let text = render_storage(&self.store);
        let path = storage_file(&self.dir, &self.workspace);
        self.saver.get_or_insert_with(Saver::new).save(path, text);
        self.store.clear_dirty();
    }

    /// 印の切替 (パレット / キーバインドから)。ニーモニック選択を開く。
    pub fn begin_toggle(
        &mut self,
        file: &Path,
        line: usize,
        text: &str,
        selection: Option<String>,
    ) {
        self.chooser = Some(Chooser {
            file: file.to_path_buf(),
            line,
            line_text: line_text(text, line).unwrap_or_default().to_string(),
            selection,
            desc: self
                .store
                .at(file, line)
                .map(|m| m.description.clone())
                .unwrap_or_default(),
            cursor: (0, 0),
            confirm: None,
        });
    }

    /// ニーモニック無しでその場で切り替える (ガターのクリック)。
    pub fn quick_toggle(&mut self, file: &Path, line: usize, text: &str) -> ToggleOutcome {
        let lt = line_text(text, line).unwrap_or_default().to_string();
        let out = self.store.toggle(&ToggleRequest {
            file,
            line,
            mnemonic: None,
            line_text: &lt,
            selection: None,
            overwrite: true,
        });
        self.flush();
        out
    }

    /// プロジェクト全体の印を消す。
    pub fn clear_all(&mut self) {
        self.store.clear();
        self.selected = None;
        self.flush();
    }

    /// 保存の直前に**追悼パスを流す**。
    ///
    /// 追悼は [`MEMORIAL_DEBOUNCE_MS`] 待ってから走るので、その途中で保存
    /// (やアプリの終了) が来ると旧本文が残らない。保存は「ここまでを確定する」
    /// 操作なので、待ちを打ち切ってここで 1 回流す。
    pub fn flush_memorial(&mut self, file: &Path, text: &str) {
        if self.store.marks().iter().all(|m| m.file != file) {
            return;
        }
        if let Some(t) = self.track.get_mut(file) {
            t.memorial_at = None;
        }
        if self.store.memorial_pass(file, text) > 0 {
            self.flush();
        }
    }

    /// 数字ニーモニックへ飛ぶ。
    pub fn goto_digit(&self, d: u8) -> Option<MarkAction> {
        let mn = Mnemonic::new((b'0' + d.min(9)) as char)?;
        let m = self.store.by_mnemonic(mn)?;
        Some(MarkAction::Goto(m.file.clone(), m.line))
    }

    /// 毎フレームの安い見張り。**本文は走査しない** — ハッシュを比べるだけ。
    ///
    /// 変化を見つけたら [`VALIDATE_DEBOUNCE_MS`] だけ待ってから、重い差分を
    /// バックグラウンドスレッドへ投げる。
    pub fn tick(&mut self, ctx: &egui::Context, file: &Path, text_hash: u64, text: &str) {
        self.poll();
        if self.store.marks().iter().all(|m| m.file != file)
            && self.store.invalid().iter().all(|m| m.file != file)
        {
            return; // 印の無いファイルは 1 バイトも触らない
        }
        let now = Instant::now();
        let e = self
            .track
            .entry(file.to_path_buf())
            .or_insert_with(|| Tracked {
                snapshot: text.to_string(),
                hash: text_hash,
                seen: now,
                memorial_at: None,
                pending: false,
            });
        if e.hash != text_hash {
            e.hash = text_hash;
            e.seen = now;
            e.pending = true;
            e.memorial_at = Some(now);
        }
        let validate_in = Duration::from_millis(VALIDATE_DEBOUNCE_MS);
        let memorial_in = Duration::from_millis(MEMORIAL_DEBOUNCE_MS);
        if e.pending && now.duration_since(e.seen) >= validate_in && !self.busy {
            let old = e.snapshot.clone();
            e.pending = false;
            self.spawn_bulk(file, old, text.to_string());
        } else if e.pending {
            crate::perf::repaint_after(ctx, validate_in, "marks::tick");
        }
        let due = self
            .track
            .get(file)
            .and_then(|t| t.memorial_at)
            .map(|t| now.duration_since(t) >= memorial_in)
            .unwrap_or(false);
        if due {
            if let Some(t) = self.track.get_mut(file) {
                t.memorial_at = None;
            }
            if self.store.memorial_pass(file, text) > 0 {
                self.flush();
            }
        } else if self.track.get(file).and_then(|t| t.memorial_at).is_some() {
            crate::perf::repaint_after(ctx, memorial_in, "marks::memorial");
        }
    }

    /// 行の増減が分かっている編集 (折りたたみ表示からの差し戻しなど)。
    pub fn note_edit(&mut self, file: &Path, at: usize, delta: isize, new_text: &str) {
        if delta == 0 || self.store.marks().iter().all(|m| m.file != file) {
            return;
        }
        self.store.on_edit(file, at, delta, new_text);
        if let Some(t) = self.track.get_mut(file) {
            t.snapshot = new_text.to_string();
        }
        self.flush();
    }

    /// 重い差分をバックグラウンドへ投げる。
    fn spawn_bulk(&mut self, file: &Path, old: String, new: String) {
        if old == new {
            return;
        }
        let marks: Vec<Mark> = self
            .store
            .marks()
            .iter()
            .filter(|m| m.file == file)
            .cloned()
            .collect();
        let (tx, rx) = std::sync::mpsc::channel::<BulkDone>();
        let f = file.to_path_buf();
        let fallback = (old.clone(), new.clone());
        let spawned = std::thread::Builder::new()
            .name("marks-diff".into())
            .spawn(move || {
                let plan = plan_bulk(&old, &new, &marks);
                let _ = tx.send(BulkDone {
                    file: f,
                    text: new,
                    plan,
                });
            })
            .is_ok();
        if spawned {
            self.rx = Some(rx);
            self.busy = true;
        } else {
            // スレッドが起こせない環境 (fd 枯渇など) では、その場で回す。
            // 止まるより 1 フレーム重い方がまし。
            let (old, new) = fallback;
            self.store.on_bulk(file, &old, &new);
            if let Some(t) = self.track.get_mut(file) {
                t.snapshot = new;
            }
            self.flush();
        }
    }

    /// バックグラウンドの結果を取り込む (チャネルを覗くだけ)。
    fn poll(&mut self) {
        let Some(rx) = self.rx.as_ref() else { return };
        let Ok(done) = rx.try_recv() else { return };
        self.rx = None;
        self.busy = false;
        self.store.apply_bulk(&done.file, &done.text, &done.plan);
        if let Some(t) = self.track.get_mut(&done.file) {
            t.snapshot = done.text;
        }
        self.flush();
    }
}

// ===========================================================================
// 11. 描画
// ===========================================================================

/// ガターに印を 1 つ描く。ニーモニックは**アイコンの 0.75 倍**で、
/// 図形の中央に載せる (JetBrains のガターアイコンと同じ作り)。
pub fn paint_gutter_glyph(
    painter: &egui::Painter,
    top_left: egui::Pos2,
    row_h: f32,
    glyph: char,
    accent: egui::Color32,
    on_accent: egui::Color32,
) {
    let rect = gutter_glyph_rect(top_left, row_h);
    if glyph == ANON_GLYPH {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            ANON_GLYPH,
            egui::FontId::proportional(rect.height()),
            accent,
        );
        return;
    }
    painter.rect_filled(rect, rect.height() * 0.2, accent);
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        // ニーモニックは**アイコンの 0.75 倍**で図形の中央に載せる
        egui::FontId::monospace(rect.height() * GLYPH_SCALE),
        on_accent,
    );
}

/// ニーモニック文字とアイコンの大きさの比 (JetBrains のガターアイコンと同じ)。
pub const GLYPH_SCALE: f32 = 0.75;

/// ガターアイコンの矩形 (行の高さに対して中央寄せの正方形)。
pub fn gutter_glyph_rect(top_left: egui::Pos2, row_h: f32) -> egui::Rect {
    let side = (row_h * 0.78).max(8.0);
    egui::Rect::from_min_size(
        egui::pos2(top_left.x, top_left.y + (row_h - side) * 0.5),
        egui::vec2(side, side),
    )
}

/// ガターの「印の列」をクリックした行 (0 始まり)。折りたたみの列
/// (`fold_x` 以降) は対象外 — そちらは既存の折りたたみ判定が持つ。
///
/// `rows` は `(原文行, 行の上端 y)`。
pub fn gutter_click_line(
    rows: &[(usize, f32)],
    row_h: f32,
    mark_x: f32,
    fold_x: f32,
    p: egui::Pos2,
) -> Option<usize> {
    if p.x < mark_x || p.x >= fold_x {
        return None;
    }
    rows.iter()
        .find(|(_, y)| p.y >= *y && p.y < *y + row_h)
        .map(|(l, _)| *l)
}

/// ガターのツールチップ本文。打鍵は**キーマップから**渡ってきたものを使う。
pub fn gutter_tooltip(hint: &str) -> String {
    if hint.is_empty() {
        tr("ブックマークの切り替え")
    } else {
        trf("ブックマークの切り替え ({k})", &[("k", hint.to_string())])
    }
}

/// すべての小窓を描く。返り値はアプリに実行してほしいこと。
pub fn windows_ui(
    ctx: &egui::Context,
    st: &mut MarksState,
    theme: &Theme,
    hints: &Hints,
    root: &Path,
) -> Vec<MarkAction> {
    let mut out = Vec::new();
    chooser_window(ctx, st, theme, &mut out);
    jump_window(ctx, st, theme, hints, &mut out);
    panel_window(ctx, st, theme, hints, root, &mut out);
    out
}

/// ニーモニック選択ポップアップ。
fn chooser_window(
    ctx: &egui::Context,
    st: &mut MarksState,
    theme: &Theme,
    out: &mut Vec<MarkAction>,
) {
    let Some(mut ch) = st.chooser.take() else {
        return;
    };
    let assigned = st.store.assigned();
    let current = st.store.at(&ch.file, ch.line).and_then(|m| m.mnemonic);
    let widths = chooser_row_widths();
    let mut close = false;
    let mut commit: Option<Option<Mnemonic>> = None;

    egui::Window::new(tr("ブックマーク: ニーモニック"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_TOP, [0.0, 90.0])
        .show(ctx, |ui| {
            ui.set_max_width(320.0);
            if let Some((f, l)) = ch.confirm.clone() {
                ui.label(
                    egui::RichText::new(trf(
                        "その文字は {f}:{l} が使っています。奪いますか?",
                        &[
                            (
                                "f",
                                f.file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .into_owned(),
                            ),
                            ("l", (l + 1).to_string()),
                        ],
                    ))
                    .color(theme.warn),
                );
                ui.horizontal(|ui| {
                    if ui.button(tr("奪う")).clicked() {
                        st.always_overwrite = true;
                        ch.confirm = None;
                    }
                    if ui.button(tr("やめる")).clicked() {
                        ch.confirm = None;
                    }
                    ui.checkbox(&mut st.always_overwrite, tr("次回から確認しない"));
                });
                ui.separator();
            }
            let avail = ui.available_width();
            let area = egui::Rect::from_min_size(
                ui.cursor().min,
                egui::vec2(avail, ui.available_height().max(240.0)),
            );
            let lay = chooser_layout(area);
            let painter = ui.painter().clone();
            for (mn, r, c, rect) in &lay.cells {
                let resp = ui.interact(
                    *rect,
                    ui.id().with(("mn", mn.index())),
                    egui::Sense::click(),
                );
                let is_cur = current == Some(*mn);
                let on_cursor = ch.cursor == (*r, *c);
                let used = assigned.contains(mn);
                let bg = if is_cur {
                    theme.accent
                } else if on_cursor {
                    theme.accent_soft
                } else if used {
                    theme.panel_alt
                } else {
                    theme.panel
                };
                painter.rect_filled(*rect, 4.0, bg);
                if on_cursor {
                    painter.rect_stroke(*rect, 4.0, egui::Stroke::new(1.5_f32, theme.accent));
                }
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    mn.ch(),
                    egui::FontId::monospace(rect.height() * 0.6),
                    if is_cur { theme.bg } else { theme.text },
                );
                if resp.clicked() {
                    commit = Some(Some(*mn));
                }
            }
            let bottom = lay
                .cells
                .last()
                .map(|(_, _, _, r)| r.bottom())
                .unwrap_or(area.top());
            ui.allocate_space(egui::vec2(avail, bottom - area.top() + 6.0));
            ui.add(
                egui::TextEdit::singleline(&mut ch.desc)
                    .desired_width(avail)
                    .hint_text(tr("説明 (任意)")),
            );
            ui.horizontal_wrapped(|ui| {
                if ui.button(tr("無印で付ける")).clicked() {
                    commit = Some(None);
                }
                if ui.button(tr("閉じる")).clicked() {
                    close = true;
                }
            });

            // 素のキー入力: 一致するニーモニックは即決、矢印は移動
            if ch.confirm.is_none() {
                let (typed, mv, enter, esc) = ui.input(|i| {
                    let typed = i.events.iter().find_map(|e| match e {
                        egui::Event::Text(t) => t.chars().next().and_then(Mnemonic::new),
                        _ => None,
                    });
                    let mut mv = (0isize, 0isize);
                    for (k, d) in [
                        (egui::Key::ArrowLeft, (-1, 0)),
                        (egui::Key::ArrowRight, (1, 0)),
                        (egui::Key::ArrowUp, (0, -1)),
                        (egui::Key::ArrowDown, (0, 1)),
                    ] {
                        if i.key_pressed(k) {
                            mv = d;
                        }
                    }
                    (
                        typed,
                        mv,
                        i.key_pressed(egui::Key::Enter),
                        i.key_pressed(egui::Key::Escape),
                    )
                });
                if let Some(m) = typed {
                    commit = Some(Some(m));
                } else if mv != (0, 0) {
                    ch.cursor = grid_step(&widths, ch.cursor, mv.0, mv.1);
                } else if enter {
                    let at = lay
                        .cells
                        .iter()
                        .find(|(_, r, c, _)| (*r, *c) == ch.cursor)
                        .map(|(m, ..)| *m);
                    commit = Some(at);
                } else if esc {
                    close = true;
                }
            }
        });

    if let Some(mn) = commit {
        let sel = ch.selection.clone();
        let desc = ch.desc.clone();
        let res = st.store.toggle(&ToggleRequest {
            file: &ch.file,
            line: ch.line,
            mnemonic: mn,
            line_text: &ch.line_text,
            selection: sel.as_deref(),
            overwrite: st.always_overwrite,
        });
        match res {
            ToggleOutcome::NeedsConfirm {
                holder_file,
                holder_line,
            } => {
                ch.confirm = Some((holder_file, holder_line));
                st.chooser = Some(ch);
                return;
            }
            other => {
                if !desc.is_empty() {
                    if let Some(m) = st
                        .store
                        .marks
                        .iter_mut()
                        .find(|m| m.file == ch.file && m.line == ch.line)
                    {
                        m.description = desc;
                    }
                }
                out.push(MarkAction::Toast(toggle_message(&other), true));
                st.flush();
                close = true;
            }
        }
    }
    if !close {
        st.chooser = Some(ch);
    }
}

/// 切替結果の日本語。
pub fn toggle_message(o: &ToggleOutcome) -> String {
    match o {
        ToggleOutcome::Added => tr("ブックマークを付けました"),
        ToggleOutcome::Removed => tr("ブックマークを外しました"),
        ToggleOutcome::Reassigned { .. } => tr("ブックマークを付け替えました"),
        ToggleOutcome::NeedsConfirm { .. } => tr("そのニーモニックは使用中です"),
    }
}

/// ジャンプ用ポップアップ。**割り当て済みのニーモニックは素のキーで飛べる**。
fn jump_window(
    ctx: &egui::Context,
    st: &mut MarksState,
    theme: &Theme,
    hints: &Hints,
    out: &mut Vec<MarkAction>,
) {
    if !st.jump_open {
        return;
    }
    let mut close = false;
    let rows: Vec<(Mnemonic, PathBuf, usize, String)> = {
        let mut v: Vec<(Mnemonic, PathBuf, usize, String)> = st
            .store
            .marks()
            .iter()
            .filter_map(|m| {
                m.mnemonic
                    .map(|x| (x, m.file.clone(), m.line, m.description.clone()))
            })
            .collect();
        v.sort_by_key(|(m, ..)| m.index());
        v
    };
    egui::Window::new(tr("ブックマークへジャンプ"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_TOP, [0.0, 90.0])
        .show(ctx, |ui| {
            ui.set_max_width(460.0);
            if rows.is_empty() {
                empty_card(
                    ui,
                    theme,
                    &tr("ニーモニック付きのブックマークがありません"),
                    &hints.toggle,
                );
            }
            for (mn, file, line, desc) in &rows {
                let w = ui.available_width();
                let label = format!(
                    "{}  {}:{}{}",
                    mn.ch(),
                    file.file_name().unwrap_or_default().to_string_lossy(),
                    line + 1,
                    if desc.is_empty() {
                        String::new()
                    } else {
                        format!("  — {desc}")
                    }
                );
                // 省略した行の全文はホバーで出す (どの幅でも見切れない)
                let r = ui
                    .add_sized(
                        [w, 20.0],
                        egui::Button::new(egui::RichText::new(ellipsize(&label, w)).monospace())
                            .frame(false),
                    )
                    .on_hover_text(&label);
                if r.clicked() {
                    out.push(MarkAction::Goto(file.clone(), *line));
                    close = true;
                }
            }
            let typed = ui.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Text(t) => t.chars().next().and_then(Mnemonic::new),
                    _ => None,
                })
            });
            if let Some(m) = typed {
                if let Some((_, f, l, _)) = rows.iter().find(|(x, ..)| *x == m) {
                    out.push(MarkAction::Goto(f.clone(), *l));
                    close = true;
                }
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
        });
    if close {
        st.jump_open = false;
    }
}

/// 一覧パネル (左=ツリー / 右=プレビュー)。
fn panel_window(
    ctx: &egui::Context,
    st: &mut MarksState,
    theme: &Theme,
    hints: &Hints,
    root: &Path,
    out: &mut Vec<MarkAction>,
) {
    if !st.panel_open {
        return;
    }
    let mut open = true;
    let groups = group_by_file(st.store.marks(), root);
    let invalid: Vec<(usize, String, usize)> = st
        .store
        .invalid()
        .iter()
        .enumerate()
        .map(|(i, m)| (i, rel_label(&m.file, root), m.line))
        .collect();
    let mut drop_invalid: Option<usize> = None;
    let mut drop_mark: Option<(PathBuf, usize)> = None;
    let mut split = st.split;
    let mut group_on = st.group_by_file;
    let mut selected = st.selected;

    egui::Window::new(tr("🔖 ブックマーク"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size([720.0, 420.0])
        .show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut group_on, tr("ファイルでまとめる"));
                ui.label(
                    egui::RichText::new(&hints.panel)
                        .color(theme.text_dim)
                        .size(11.0),
                );
            });
            ui.separator();
            let area = ui.available_rect_before_wrap();
            if groups.is_empty() && invalid.is_empty() {
                empty_card(
                    ui,
                    theme,
                    &tr("ブックマークはまだありません"),
                    &hints.toggle,
                );
                return;
            }
            let lay = panel_layout(area, split);
            // 分割バー
            let sr = ui.interact(
                lay.splitter,
                ui.id().with("marks-split"),
                egui::Sense::drag(),
            );
            ui.painter().rect_filled(lay.splitter, 1.0, theme.border);
            if sr.dragged() && !lay.stacked && area.width() > 0.0 {
                split = ((lay.splitter.left() + sr.drag_delta().x - area.left()) / area.width())
                    .clamp(0.2, 0.8);
            }
            if sr.hovered() && !lay.stacked {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }

            // 左: ツリー
            let mut tree_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(lay.tree.shrink(2.0))
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            egui::ScrollArea::vertical()
                .id_salt("marks-tree")
                .auto_shrink([false, false])
                .show(&mut tree_ui, |ui| {
                    for g in &groups {
                        if group_on {
                            ui.label(
                                egui::RichText::new(ellipsize(&g.label, ui.available_width()))
                                    .color(theme.text_dim)
                                    .size(11.5),
                            )
                            .on_hover_text(&g.label);
                        }
                        for i in &g.rows {
                            let m = &st.store.marks()[*i];
                            let w = ui.available_width();
                            let head = m.mnemonic.map(|x| x.ch()).unwrap_or(ANON_GLYPH);
                            let text = format!(
                                "{head} {}:{}  {}",
                                if group_on {
                                    String::new()
                                } else {
                                    g.label.clone()
                                },
                                m.line + 1,
                                if m.description.is_empty() {
                                    m.expected_text.trim().to_string()
                                } else {
                                    m.description.clone()
                                }
                            );
                            let r = ui.add_sized(
                                [w, 19.0],
                                egui::SelectableLabel::new(
                                    selected == Some(*i),
                                    ellipsize(&text, w),
                                ),
                            );
                            if r.clicked() {
                                selected = Some(*i);
                            }
                            if r.double_clicked() {
                                out.push(MarkAction::Goto(m.file.clone(), m.line));
                            }
                            r.context_menu(|ui| {
                                if ui.button(tr("削除")).clicked() {
                                    drop_mark = Some((m.file.clone(), m.line));
                                    ui.close_menu();
                                }
                            });
                        }
                    }
                    if !invalid.is_empty() {
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(tr("追えなくなった印"))
                                .color(theme.warn)
                                .size(11.5),
                        );
                        for (k, label, line) in &invalid {
                            let w = ui.available_width();
                            let text = format!("? {label}:{}", line + 1);
                            let r = ui.add_sized(
                                [w, 19.0],
                                egui::Button::new(
                                    egui::RichText::new(ellipsize(&text, w)).color(theme.text_dim),
                                )
                                .frame(false),
                            );
                            r.clone().on_hover_text(tr(
                                "本文が戻れば復活します (ブランチを戻す・元に戻す)",
                            ));
                            r.context_menu(|ui| {
                                if ui.button(tr("削除")).clicked() {
                                    drop_invalid = Some(*k);
                                    ui.close_menu();
                                }
                            });
                        }
                    }
                });

            // 右: プレビュー
            let mut pv = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(lay.preview.shrink(4.0))
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            match selected.and_then(|i| st.store.marks().get(i)) {
                Some(m) => {
                    let w = pv.available_width();
                    pv.label(egui::RichText::new(ellipsize(&rel_label(&m.file, root), w)).strong())
                        .on_hover_text(m.file.display().to_string());
                    pv.label(
                        egui::RichText::new(trf("{n} 行目", &[("n", (m.line + 1).to_string())]))
                            .color(theme.text_dim)
                            .size(11.5),
                    );
                    pv.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .id_salt("marks-preview")
                        .auto_shrink([false, false])
                        .show(&mut pv, |ui| {
                            ui.label(egui::RichText::new(&m.expected_text).monospace());
                            if !m.description.is_empty() {
                                ui.add_space(6.0);
                                ui.label(egui::RichText::new(&m.description).color(theme.text_dim));
                            }
                        });
                    if pv.button(tr("この行へ移動")).clicked() {
                        out.push(MarkAction::Goto(m.file.clone(), m.line));
                    }
                }
                None => {
                    empty_card(&mut pv, theme, &tr("行を選ぶとここに出ます"), "");
                }
            }
        });

    st.split = split;
    st.group_by_file = group_on;
    st.selected = selected;
    if let Some((f, l)) = drop_mark {
        st.store.remove_at(&f, l);
        st.selected = None;
        st.flush();
    }
    if let Some(k) = drop_invalid {
        st.store.remove_invalid(k);
        st.flush();
    }
    if !open {
        st.panel_open = false;
    }
}

/// 空状態は**可用領域の中央**に 1 枚のカードで出す (下に取り残さない)。
fn empty_card(ui: &mut egui::Ui, theme: &Theme, msg: &str, hint: &str) {
    let rect = ui.available_rect_before_wrap();
    if rect.height() <= 0.0 {
        return;
    }
    ui.allocate_new_ui(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::centered_and_justified(
                egui::Direction::TopDown,
            )),
        |ui| {
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(msg).color(theme.text_dim));
                if !hint.is_empty() {
                    ui.label(egui::RichText::new(hint).color(theme.text_dim).size(11.0));
                }
            });
        },
    );
}

/// おおよその文字幅で切り詰める (全文はホバーで見せる前提)。
pub fn ellipsize(s: &str, avail_w: f32) -> String {
    let per = 7.0_f32;
    let max = ((avail_w / per).floor() as usize).max(6);
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

// ===========================================================================
// テスト
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    fn p(s: &str) -> PathBuf {
        // 直書きの絶対パスを避ける — どの OS でも成り立つ相対パスで組む
        PathBuf::from("src").join(s)
    }

    fn mark(file: &str, line: usize, text: &str, mn: Option<char>) -> Mark {
        Mark {
            file: p(file),
            line,
            expected_text: text.to_string(),
            mnemonic: mn.and_then(Mnemonic::new),
            description: String::new(),
            group: default_group(),
        }
    }

    // ── ニーモニック ───────────────────────────────────────────────

    #[test]
    fn ニーモニックは36種で小文字を畳む() {
        assert_eq!(Mnemonic::new('a').map(|m| m.ch()), Some('A'));
        assert_eq!(Mnemonic::new('Z').map(|m| m.index()), Some(35));
        assert_eq!(Mnemonic::new('0').map(|m| m.index()), Some(0));
        assert_eq!(Mnemonic::new('9').and_then(|m| m.digit()), Some(9));
        assert_eq!(Mnemonic::new('A').and_then(|m| m.digit()), None);
        assert!(Mnemonic::new('・').is_none());
        assert!(Mnemonic::new(' ').is_none());
        let all: HashSet<usize> = ('0'..='9')
            .chain('A'..='Z')
            .filter_map(Mnemonic::new)
            .map(|m| m.index())
            .collect();
        assert_eq!(all.len(), MNEMONIC_COUNT);
    }

    // ── 切替の意味論 ───────────────────────────────────────────────

    #[test]
    fn 切替は追加削除付け替えの3通り() {
        let mut s = MarkStore::default();
        let f = p("a.rs");
        let req = |mn: Option<char>, over: bool| ToggleRequest {
            file: &f,
            line: 3,
            mnemonic: mn.and_then(Mnemonic::new),
            line_text: "fn main() {",
            selection: None,
            overwrite: over,
        };
        // 表: (要求, 期待, 残る件数)
        assert_eq!(s.toggle(&req(Some('1'), false)), ToggleOutcome::Added);
        assert_eq!(s.marks().len(), 1);
        assert_eq!(
            s.toggle(&req(Some('2'), false)),
            ToggleOutcome::Reassigned {
                from: Mnemonic::new('1')
            }
        );
        assert_eq!(s.marks()[0].mnemonic, Mnemonic::new('2'));
        assert_eq!(s.toggle(&req(Some('2'), false)), ToggleOutcome::Removed);
        assert!(s.marks().is_empty());
        assert_eq!(s.toggle(&req(None, false)), ToggleOutcome::Added);
        assert_eq!(s.toggle(&req(None, false)), ToggleOutcome::Removed);
    }

    #[test]
    fn 選択文字列が説明になる() {
        let mut s = MarkStore::default();
        let f = p("a.rs");
        s.toggle(&ToggleRequest {
            file: &f,
            line: 0,
            mnemonic: None,
            line_text: "let x = 1;",
            selection: Some("  let x  \n = 1;  "),
            overwrite: false,
        });
        assert_eq!(s.marks()[0].description, "let x = 1;");
        // 空白だけの選択は説明にしない
        assert_eq!(description_from_selection("  \n\t "), None);
    }

    #[test]
    fn ニーモニックはプロジェクトで一意() {
        let mut s = MarkStore::default();
        let (a, b) = (p("a.rs"), p("b.rs"));
        s.toggle(&ToggleRequest {
            file: &a,
            line: 1,
            mnemonic: Mnemonic::new('7'),
            line_text: "one",
            selection: None,
            overwrite: false,
        });
        // 別ファイルで同じ 7 → 確認が要る
        let need = s.toggle(&ToggleRequest {
            file: &b,
            line: 2,
            mnemonic: Mnemonic::new('7'),
            line_text: "two",
            selection: None,
            overwrite: false,
        });
        assert_eq!(
            need,
            ToggleOutcome::NeedsConfirm {
                holder_file: a.clone(),
                holder_line: 1
            }
        );
        assert_eq!(s.marks().len(), 1, "確認前は 1 件も足さない");
        // 上書きを許すと前の持ち主から剥がれる
        s.toggle(&ToggleRequest {
            file: &b,
            line: 2,
            mnemonic: Mnemonic::new('7'),
            line_text: "two",
            selection: None,
            overwrite: true,
        });
        assert_eq!(s.marks().len(), 2);
        assert_eq!(s.by_mnemonic(Mnemonic::new('7').unwrap()).unwrap().file, b);
        assert_eq!(s.assigned().len(), 1);
    }

    // ── 経路 1: 通常の編集 ─────────────────────────────────────────

    #[test]
    fn 通常編集は行を追従し消えた行は無効になる() {
        let mut s = MarkStore::default();
        let f = p("a.rs");
        s.marks.push(mark("a.rs", 1, "b", None));
        s.marks.push(mark("a.rs", 3, "d", None));
        // 0 行目に 2 行挿入
        let after = "x\ny\na\nb\nc\nd";
        let rep = s.on_edit(&f, 0, 2, after);
        assert_eq!(rep.moved, 2);
        assert_eq!(s.marks()[0].line, 3);
        assert_eq!(s.marks()[0].expected_text, "b");
        assert_eq!(s.marks()[1].line, 5);

        // b の行 (3) を消す
        let after2 = "x\ny\na\nc\nd";
        let rep2 = s.on_edit(&f, 3, -1, after2);
        assert_eq!(rep2.invalidated, 1);
        assert_eq!(s.marks().len(), 1);
        assert_eq!(s.invalid().len(), 1);
        assert_eq!(s.invalid()[0].expected_text, "b");
    }

    #[test]
    fn 通常編集で同じ行に重なった印は後ろが消える() {
        // `remap_line` は単射なので、通常の編集で 2 件が同じ行に落ちるのは
        // **保存ファイルに同じ行が 2 件入っていた**場合だけ。番人が効くこと。
        let mut s = MarkStore::default();
        let f = p("a.rs");
        s.marks.push(mark("a.rs", 2, "c", Some('1')));
        s.marks.push(mark("a.rs", 2, "c", Some('2')));
        let rep = s.on_edit(&f, 0, 1, "x\na\nb\nc");
        assert_eq!(rep.collapsed, 1, "後から入った方が消える");
        assert_eq!(s.marks().len(), 1);
        assert_eq!(s.marks()[0].mnemonic, Mnemonic::new('1'));
        assert_eq!(s.marks()[0].line, 3);
    }

    #[test]
    fn 削除された行は重なりではなく無効になる() {
        let mut s = MarkStore::default();
        let f = p("a.rs");
        s.marks.push(mark("a.rs", 2, "c", Some('1')));
        s.marks.push(mark("a.rs", 3, "d", Some('2')));
        // 3 行目を消す → 元 3 行目は「消えた行」なので無効化 (重なりではない)
        let rep = s.on_edit(&f, 3, -1, "a\nb\nc\ne");
        assert_eq!(rep.collapsed, 0);
        assert_eq!(rep.invalidated, 1);
        assert_eq!(s.marks().len(), 1);
        assert_eq!(s.invalid()[0].expected_text, "d");
    }

    #[test]
    fn 挿入行から無効な印が復活する() {
        let mut s = MarkStore::default();
        let f = p("a.rs");
        s.invalid.push(InvalidMark {
            file: f.clone(),
            line: 1,
            expected_text: "fn gone()".into(),
            mnemonic: Mnemonic::new('5'),
            description: "戻ってくる".into(),
            group: default_group(),
        });
        let rep = s.on_edit(&f, 1, 1, "a\nfn gone()\nb");
        assert_eq!(rep.revived, 1);
        assert!(s.invalid().is_empty());
        assert_eq!(s.marks()[0].line, 1);
        assert_eq!(s.marks()[0].mnemonic, Mnemonic::new('5'));
        assert_eq!(s.marks()[0].description, "戻ってくる");
    }

    // ── 経路 2: 全文置換 ───────────────────────────────────────────

    #[test]
    fn 外側走査は距離1手前優先で当たる() {
        let lines = ["a", "T", "c", "d", "T", "f"];
        // preferred=3 → 距離 1 で手前 (2) は不一致、後ろ (4) が一致
        assert_eq!(scan_outward(&lines, 3, "T"), Some(4));
        // preferred=2 → 距離 1 の手前 (1) が先に当たる
        assert_eq!(scan_outward(&lines, 2, "T"), Some(1));
        // ちょうどその行にあるなら距離 0
        assert_eq!(scan_outward(&lines, 1, "T"), Some(1));
        assert_eq!(scan_outward(&lines, 0, "zzz"), None);
        assert_eq!(scan_outward(&lines, 0, ""), None);
        assert_eq!(scan_outward(&[], 0, "T"), None);
        // 範囲外の preferred は末尾へ丸める
        assert_eq!(scan_outward(&lines, 99, "f"), Some(5));
    }

    #[test]
    fn 行の写像は共通部分とlcsで付く() {
        let old = ["a", "b", "c", "d"];
        let new = ["a", "x", "b", "c", "d"];
        assert_eq!(
            map_lines(&old, &new),
            vec![Some(0), Some(2), Some(3), Some(4)]
        );
        let old2 = ["a", "b", "c"];
        let new2 = ["a", "c"];
        assert_eq!(map_lines(&old2, &new2), vec![Some(0), None, Some(1)]);
    }

    #[test]
    fn 全文置換は写像と本文走査で追いかける() {
        let mut s = MarkStore::default();
        let f = p("a.rs");
        s.marks.push(mark("a.rs", 2, "target", None));
        let old = "a\nb\ntarget\nd";
        // 先頭に 3 行入り、target は 5 行目へ
        let new = "1\n2\n3\na\nb\ntarget\nd";
        let rep = s.on_bulk(&f, old, new);
        assert_eq!(rep.moved, 1);
        assert_eq!(s.marks()[0].line, 5);

        // 写像が付かない (全部書き換わった) が本文は残っている → 走査で拾う
        let mut s2 = MarkStore::default();
        s2.marks.push(mark("a.rs", 1, "keep", None));
        let rep2 = s2.on_bulk(&f, "x\nkeep\ny", "p\nq\nr\nkeep\ns");
        assert_eq!(rep2.moved, 1);
        assert_eq!(s2.marks()[0].line, 3);

        // 本文ごと消えた → 無効化 (捨てない)
        let mut s3 = MarkStore::default();
        s3.marks.push(mark("a.rs", 1, "keep", Some('3')));
        let rep3 = s3.on_bulk(&f, "x\nkeep\ny", "totally\ndifferent");
        assert_eq!(rep3.invalidated, 1);
        assert_eq!(s3.invalid().len(), 1);
        assert_eq!(s3.invalid()[0].expected_text, "keep");
    }

    #[test]
    fn ブランチを戻すと印が復活する() {
        let mut s = MarkStore::default();
        let f = p("a.rs");
        s.marks.push(mark("a.rs", 1, "fn feature()", Some('F')));
        // チェックアウトで消える
        s.on_bulk(&f, "a\nfn feature()\nb", "a\nb");
        assert_eq!(s.marks().len(), 0);
        assert_eq!(s.invalid().len(), 1);
        // 戻すと帰ってくる
        let rep = s.on_bulk(&f, "a\nb", "a\nfn feature()\nb");
        assert_eq!(rep.revived, 1);
        assert_eq!(s.marks()[0].line, 1);
        assert_eq!(s.marks()[0].mnemonic, Mnemonic::new('F'));
        assert!(s.invalid().is_empty());
    }

    #[test]
    fn 全文置換でも重なりは後ろが消える() {
        let mut s = MarkStore::default();
        let f = p("a.rs");
        s.marks.push(mark("a.rs", 1, "same", None));
        s.marks.push(mark("a.rs", 2, "same", None));
        // 2 行あった "same" が 1 行になる
        let rep = s.on_bulk(&f, "a\nsame\nsame\nb", "a\nsame\nb");
        assert_eq!(rep.collapsed, 1);
        assert_eq!(s.marks().len(), 1);
        assert_eq!(s.marks()[0].line, 1);
    }

    // ── 追悼 ───────────────────────────────────────────────────────

    #[test]
    fn 追悼は一度きりで旧本文を残す() {
        let mut s = MarkStore::default();
        let f = p("a.rs");
        s.marks.push(mark("a.rs", 1, "let x = 1;", Some('1')));
        assert_eq!(s.memorial_pass(&f, "a\nlet x = 2;\nb"), 1);
        assert_eq!(s.invalid().len(), 1);
        assert_eq!(s.invalid()[0].expected_text, "let x = 1;");
        assert_eq!(s.invalid()[0].mnemonic, None, "ニーモニックは複製しない");
        assert_eq!(s.marks()[0].expected_text, "let x = 2;");
        // 同じ旧本文では二度と積まない
        assert_eq!(s.memorial_pass(&f, "a\nlet x = 2;\nb"), 0);
        // さらに書き換えると別の旧本文なので 1 回だけ積む
        assert_eq!(s.memorial_pass(&f, "a\nlet x = 3;\nb"), 1);
        assert_eq!(s.memorial_pass(&f, "a\nlet x = 3;\nb"), 0);
        assert_eq!(s.invalid().len(), 2);
    }

    // ── 保存 ───────────────────────────────────────────────────────

    #[test]
    fn 保存は往復して版を移行できる() {
        let dir = unique_temp_dir("zaivern", "marks-io");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).expect("ワークスペースを作る");
        let mut s = MarkStore::default();
        s.marks.push(mark("a.rs", 4, "hello", Some('A')));
        s.invalid.push(InvalidMark {
            file: p("b.rs"),
            line: 9,
            expected_text: "lost".into(),
            mnemonic: None,
            description: String::new(),
            group: default_group(),
        });
        let text = render_storage(&s);
        assert!(text.contains("version = 2"));
        let back = parse_storage(&text);
        assert_eq!(back.marks(), s.marks());
        assert_eq!(back.invalid(), s.invalid());

        // v1 (1 始まりの行・本文の控え無し) からの移行
        let v1 = r#"
version = 1
[[bookmarks]]
file = "src/old.rs"
line = 12
mnemonic = "b"
description = "旧形式"
"#;
        let m = parse_storage(v1);
        assert_eq!(m.marks().len(), 1);
        assert_eq!(m.marks()[0].line, 11, "1 始まり → 0 始まり");
        assert_eq!(m.marks()[0].mnemonic, Mnemonic::new('B'));
        assert_eq!(m.marks()[0].expected_text, "");
        assert_eq!(m.marks()[0].group, "default");

        // 壊れた入力でも落ちない
        assert!(parse_storage("これは toml ではない [[[").is_empty());

        // ディレクトリ経由の読み書き (実 ~/.zaivern には触らない)
        let path = storage_file(&dir, &ws);
        std::fs::create_dir_all(path.parent().expect("親がある")).expect("ディレクトリ");
        std::fs::write(&path, render_storage(&s)).expect("書き出し");
        let loaded = load_from_dir(&dir, &ws);
        assert_eq!(loaded.marks().len(), 1);
        // 別ワークスペースは別ファイル
        let other = dir.join("other");
        std::fs::create_dir_all(&other).expect("別ワークスペース");
        assert_ne!(storage_file(&dir, &ws), storage_file(&dir, &other));
        assert!(load_from_dir(&dir, &other).is_empty());
    }

    #[test]
    fn 状態は差し替えたディレクトリへ保存する() {
        let dir = unique_temp_dir("zaivern", "marks-state");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).expect("ワークスペース");
        let mut st = MarksState::default();
        st.set_dir(&dir);
        st.set_workspace(&ws);
        let out = st.quick_toggle(&ws.join("a.rs"), 1, "a\nb\nc");
        assert_eq!(out, ToggleOutcome::Added);
        assert_eq!(st.store().marks()[0].expected_text, "b");
        // 書き込みは別スレッドなので、ファイルが出るまで少しだけ待つ
        let path = storage_file(&dir, &ws);
        for _ in 0..200 {
            if path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(path.exists(), "保存ファイルが作られていない: {path:?}");
        assert!(std::fs::read_to_string(&path)
            .expect("読み出し")
            .contains("version = 2"));
    }

    #[test]
    fn 保存の直前に追悼を流せる() {
        let dir = unique_temp_dir("zaivern", "marks-memorial");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).expect("ワークスペース");
        let mut st = MarksState::default();
        st.set_dir(&dir);
        st.set_workspace(&ws);
        let f = ws.join("a.rs");
        st.quick_toggle(&f, 1, "a\nb\nc");
        // 行が書き換わった状態で保存 → 旧本文が追悼として残る
        st.flush_memorial(&f, "a\nB!\nc");
        assert_eq!(st.store().invalid().len(), 1);
        assert_eq!(st.store().invalid()[0].expected_text, "b");
        assert_eq!(st.store().marks()[0].expected_text, "B!");
        // 印の無いファイルでは何もしない
        st.flush_memorial(&ws.join("other.rs"), "x\ny");
        assert_eq!(st.store().invalid().len(), 1);
    }

    // ── ガター ─────────────────────────────────────────────────────

    #[test]
    fn ガターの当たり判定は印の列だけ() {
        let rows = [(0usize, 0.0_f32), (1, 16.0), (2, 32.0)];
        let (mark_x, fold_x, row_h) = (10.0_f32, 24.0_f32, 16.0_f32);
        let at = |x: f32, y: f32| gutter_click_line(&rows, row_h, mark_x, fold_x, egui::pos2(x, y));
        assert_eq!(at(12.0, 20.0), Some(1));
        assert_eq!(at(12.0, 0.0), Some(0));
        // 折りたたみの列は対象外 (そちらは既存の判定が持つ)
        assert_eq!(at(30.0, 20.0), None);
        // 印の列より左も対象外
        assert_eq!(at(2.0, 20.0), None);
        // 行の外
        assert_eq!(at(12.0, 500.0), None);
        // アイコンは行の中に収まり、正方形になる
        let r = gutter_glyph_rect(egui::pos2(10.0, 0.0), row_h);
        assert!(r.height() <= row_h + 0.01 && (r.width() - r.height()).abs() < 0.01);
        assert!(r.top() >= 0.0 && r.bottom() <= row_h + 0.01);
        assert_eq!(GLYPH_SCALE, 0.75);
    }

    // ── デバウンス定数 ─────────────────────────────────────────────

    #[test]
    fn デバウンスは検証100ms追悼2000ms() {
        assert_eq!(VALIDATE_DEBOUNCE_MS, 100);
        assert_eq!(MEMORIAL_DEBOUNCE_MS, 2000);
        assert!(MEMORIAL_DEBOUNCE_MS > VALIDATE_DEBOUNCE_MS);
    }

    // ── 並び順 ─────────────────────────────────────────────────────

    #[test]
    fn 並びはディレクトリ優先で自然順() {
        assert_eq!(natural_cmp("f2", "f10"), Ordering::Less);
        assert_eq!(natural_cmp("a", "B"), Ordering::Less);
        assert_eq!(natural_cmp("x", "x"), Ordering::Equal);
        assert_eq!(
            path_cmp(Path::new("src/a.rs"), Path::new("a.rs")),
            Ordering::Less,
            "同じ階層ではディレクトリが先"
        );
        assert_eq!(
            path_cmp(Path::new("src/f2.rs"), Path::new("src/f10.rs")),
            Ordering::Less
        );
        let marks = vec![
            mark("b.rs", 5, "", None),
            mark("b.rs", 1, "", None),
            mark("a.rs", 2, "", None),
        ];
        let g = group_by_file(&marks, Path::new("src"));
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].label, "a.rs");
        assert_eq!(
            g[1].rows.iter().map(|i| marks[*i].line).collect::<Vec<_>>(),
            vec![1, 5],
            "同じファイルの中は行番号順"
        );
    }

    // ── レイアウト ─────────────────────────────────────────────────

    fn rects_ok(l: &PanelLayout, area: egui::Rect) -> Result<(), String> {
        let rs = [
            ("tree", l.tree),
            ("split", l.splitter),
            ("preview", l.preview),
        ];
        for (n, r) in rs {
            if r.left() < area.left() - 0.01
                || r.right() > area.right() + 0.01
                || r.top() < area.top() - 0.01
                || r.bottom() > area.bottom() + 0.01
            {
                return Err(format!("{n} が領域からはみ出した: {r:?} ⊄ {area:?}"));
            }
        }
        for i in 0..rs.len() {
            for j in i + 1..rs.len() {
                let x = rs[i].1.intersect(rs[j].1);
                if x.width() > 0.01 && x.height() > 0.01 {
                    return Err(format!("{} と {} が重なった: {x:?}", rs[i].0, rs[j].0));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn パネルのレイアウトはどの大きさでも収まる() {
        // (幅, 高さ, 比, 縦積みか)
        let cases: &[(f32, f32, f32, bool)] = &[
            (900.0, 700.0, 0.45, false),
            (1200.0, 300.0, 0.45, false),
            (1200.0, 300.0, 0.05, false),
            (1200.0, 300.0, 0.95, false),
            (900.0, 700.0, 0.5, false),
            (320.0, 700.0, 0.45, true),
            (120.0, 120.0, 0.45, true),
        ];
        for (w, h, r, stacked) in cases.iter().copied() {
            let area = egui::Rect::from_min_size(egui::pos2(11.0, 23.0), egui::vec2(w, h));
            let l = panel_layout(area, r);
            assert_eq!(l.stacked, stacked, "{w}x{h} の積み方");
            rects_ok(&l, area).unwrap_or_else(|e| panic!("{w}x{h} r={r}: {e}"));
        }
    }

    #[test]
    fn ニーモニック格子はどの幅でも収まる() {
        for (w, h) in [(900.0_f32, 700.0_f32), (1200.0, 300.0), (240.0, 400.0)] {
            let area = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h));
            let lay = chooser_layout(area);
            assert_eq!(lay.cells.len(), MNEMONIC_COUNT, "36 マス並ぶ");
            assert_eq!(lay.digit_rows, 4);
            for (mn, _, _, r) in &lay.cells {
                assert!(
                    r.right() <= area.right() + 0.01 && r.left() >= area.left() - 0.01,
                    "{} が幅からはみ出した: {r:?}",
                    mn.ch()
                );
            }
            // 同じ行のマス同士が重ならない
            for a in &lay.cells {
                for b in &lay.cells {
                    if a.0 == b.0 {
                        continue;
                    }
                    let x = a.3.intersect(b.3);
                    assert!(
                        x.width() <= 0.01 || x.height() <= 0.01,
                        "{} と {} が重なった",
                        a.0.ch(),
                        b.0.ch()
                    );
                }
            }
        }
    }

    #[test]
    fn 格子の矢印移動は端で折り返す() {
        let w = chooser_row_widths();
        assert_eq!(w, vec![3, 3, 3, 1, 7, 7, 7, 5]);
        // 右端 → 左端
        assert_eq!(grid_step(&w, (0, 2), 1, 0), (0, 0));
        // 左端 → 右端
        assert_eq!(grid_step(&w, (0, 0), -1, 0), (0, 2));
        // 上端 → 下端
        assert_eq!(grid_step(&w, (0, 1), 0, -1), (7, 1));
        // 短い行へ降りたら列は丸める
        assert_eq!(grid_step(&w, (2, 2), 0, 1), (3, 0));
        assert_eq!(grid_step(&[], (0, 0), 1, 0), (0, 0));
    }

    #[test]
    fn 省略表示は幅に収まる() {
        assert_eq!(ellipsize("abc", 400.0), "abc");
        let s = ellipsize(&"a".repeat(200), 70.0);
        assert!(s.chars().count() <= 10, "{s}");
        assert!(s.ends_with('…'));
    }

    // ── キーバインド (OS ごとに期待値を変える) ──────────────────────

    #[test]
    fn 数字ジャンプの打鍵はosごとに固定されている() {
        use crate::keybinds;
        let sc = digit_jump_shortcut(1).expect("1 は在る");
        // **期待値に cfg! を入れる** — 他 OS で使わない打鍵を咎めないため
        let want = if cfg!(target_os = "macos") {
            "⌃⇧1"
        } else {
            "Ctrl+Shift+1"
        };
        assert_eq!(keybinds::format_shortcut(sc), want);
        assert!(digit_jump_shortcut(10).is_none());
        for d in 0u8..=9 {
            assert!(digit_jump_shortcut(d).is_some(), "{d} が無い");
        }
    }

    #[test]
    fn ブックマークの既定打鍵はosごとに固定されている() {
        use crate::keybinds::{BindAction, Keybinds};
        let keys = Keybinds::from_overrides(&HashMap::new());
        // (アクション, macOS の表記, その他 OS の表記)
        let table: &[(BindAction, &str, &str)] = &[
            (BindAction::MarkToggleMnemonic, "⌥⇧⌘B", "Ctrl+Alt+Shift+B"),
            (BindAction::MarksPanel, "⌥⌘M", "Ctrl+Alt+M"),
            (BindAction::MarkJump, "⌥⌘J", "Ctrl+Alt+J"),
        ];
        for (a, mac, other) in table {
            // **期待値に cfg! を入れる** — 他 OS で使わない打鍵を咎めないため
            let want = if cfg!(target_os = "macos") {
                mac
            } else {
                other
            };
            assert_eq!(&keys.label(*a), want, "{a:?} の既定");
        }
    }

    #[test]
    fn 数字ジャンプはos予約とも既存割り当てとも食い合わない() {
        use crate::keybinds::{self, Binding, Keybinds};
        let keys = Keybinds::from_overrides(&HashMap::new());
        for d in 0u8..=9 {
            let sc = digit_jump_shortcut(d).expect("在る");
            // ① macOS の実測予約表に無い
            assert_eq!(
                keybinds::macos_reservation(sc),
                None,
                "数字 {d} の打鍵が OS 予約と衝突している"
            );
            // ② 既定の割り当てと**畳んだ形で**一致しない。
            //    `canonical_mods` は非 macOS で ⌃ を ⌘ へ畳むので、
            //    ここが Linux / Windows でだけ落ちる罠を捕まえる。
            for a in keybinds::ALL_ACTIONS {
                let b = keys.binding(a);
                let hit = match b {
                    Binding::Single(x) => keybinds::same_stroke(x, sc),
                    Binding::Chord(x, _) => keybinds::same_stroke(x, sc),
                };
                assert!(
                    !hit,
                    "数字 {d} の打鍵が {a:?} ({}) と同じ打鍵になっている",
                    keys.label(a)
                );
            }
        }
    }

    // ── 表示文字列に打鍵をベタ書きしていない ────────────────────────

    #[test]
    fn ガターの案内はキーマップから作る() {
        assert_eq!(
            gutter_tooltip(""),
            tr("ブックマークの切り替え"),
            "打鍵が無いときも文言は出す"
        );
        let s = gutter_tooltip("XYZ");
        assert!(s.contains("XYZ"), "渡された打鍵をそのまま使う: {s}");
        // ソース中に修飾キー記号のリテラルが無いこと (CRLF を正規化してから見る)。
        // **見るのは本体だけ** — テストは OS ごとの期待値として記号を書く。
        let whole = include_str!("marks.rs").replace("\r\n", "\n");
        let src = whole.split("mod tests {").next().expect("本体がある");
        for line in src.lines() {
            if line.contains("assert")
                || line.trim_start().starts_with("///")
                || line.trim_start().starts_with("//")
            {
                continue;
            }
            for g in ['⌘', '⌥', '⌃', '⇧'] {
                assert!(
                    !(line.contains('"') && line.contains(g)),
                    "打鍵記号をベタ書きしている: {line}"
                );
            }
        }
    }
}
