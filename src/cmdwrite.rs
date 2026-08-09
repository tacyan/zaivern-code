//! シェルのコマンド行から**書き込まれる可能性のあるパス**を抜く純関数。
//!
//! ## なぜ要るか (実際に破られた穴)
//!
//! ファイル所有リース ([`crate::lease`]) の強制は、フックの payload から
//! **パスのキー** (`tool_input.file_path` 等 — [`crate::agents::HOOK_TARGETS`]
//! の `write_path_keys`) を引いて対象を決めている。ところが `Bash` ツールは
//! パスを持たず**コマンド文字列**しか持たないので、そのままでは
//! 判定の入口にすら入れない。
//!
//! 実証: A が `shared.rs` を確保している状態で、B が
//! `PreToolUse{tool_name:"Bash", command:"printf B_WON > shared.rs"}` を投げると
//! **許可され、ファイルは上書きされ、台帳上は A が持ったまま**になった。
//! エージェントは `sed -i` / `tee` / `>>` / リダイレクトで**日常的に**書くので、
//! ここが最大の穴だった。
//!
//! ## 方針: 取りこぼしより過検出 (fail-closed 側)
//!
//! 1 件でも余計に拾うと「本当は書かないコマンドが止まる」が、その場合は
//! **リースの持ち主が自分なら通る**し、他人が持っているファイルへ触る形なら
//! そもそも止めたい。逆に取りこぼすと**黙って上書きされる**ので回復できない。
//! 迷ったら拾う。
//!
//! ## 拾える形
//!
//! - リダイレクト: `> f` / `>> f` / `2> f` / `&> f` / `&>> f` / `>| f` / `>& f`
//!   (`2>&1` のような fd 複製は**ファイルではない**ので拾わない)
//! - パイプ経由: `|& tee f` / `... | tee -a f`
//! - その場編集: `sed -i` (GNU の `-i` / `-i.bak`、BSD の `-i ''`)、
//!   `perl -i` / `perl -pi -e`、`awk -i inplace` (gawk)
//! - ファイル操作: `cp` (宛先)、`mv` (**両側** — 元は消えるため)、`rm`、
//!   `install`、`ln`、`truncate`、`touch`、`dd of=`
//! - 複文・前置き: `;` `&&` `||` `|` `|&` `&` `(` `)` 改行での分割、
//!   `FOO=bar cmd`、`sudo` / `env` / `nohup` / `time` / `timeout` / `nice` /
//!   `xargs` 等の前置き、`bash -c "…"` の中身 (深さ 3 まで再帰)、
//!   `` $(…) `` / `` `…` `` の中身
//! - クォート (`'` `"`)・バックスラッシュエスケープ・`#` コメント
//!   (`grep '>' f` や `echo ">"`、`# rm -rf > f` を**書き込みと誤認しない**)
//!
//! ## 拾えない形 (正直に書く)
//!
//! ここは**構造的に**追えない。追えるふりをしないこと。
//!
//! - **変数展開**: `> $OUT` / `rm "$F"` — 値はフックの時点で判らない。
//!   パスとしては出さず、[`opaque_write`] を立てて呼び出し側に知らせる
//! - **`eval` / `source`**: 実行時に組み立てられる文字列
//! - **ヒアドキュメント**: `cat <<EOF` の本文は「コマンド行」の一部として
//!   渡ってくるため、本文の行を**コマンドとして誤って字句解析する**
//!   (過検出側に倒れるので、そのままにしてある)
//! - **プログラムが自分で開くファイル**: `python -c "open('f','w')"`、
//!   `node -e`、`cargo build` の成果物、`git checkout`、`patch -p1 < d.diff`。
//!   言語処理系の中まで追うのは範囲外 (`patch` / `eval` は opaque として印だけ立てる)
//! - **サブシェルの生成物**やプロセス置換 `>(cmd)`
//!
//! ## 使う側へ
//!
//! 入口は [`crate::agents::hook_write_targets`]。ツール名 → コマンド文字列の
//! キーはエージェント固有なので**カタログ (`agents.rs`) 側**に置いてある。
//! このモジュールはエージェント固有値を 1 つも持たない。

/// 再帰の上限 (`bash -c "bash -c …"` / `$( … )` の入れ子)。
const MAX_DEPTH: usize = 3;

// ═══════════════════════════════════════════════════════════════════════════
//  公開 API
// ═══════════════════════════════════════════════════════════════════════════

/// 抽出結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// 書き込まれる可能性のあるパス (出現順・重複なし)。
    pub targets: Vec<String>,
    /// **書き込みらしいのに対象が特定できなかった**か
    /// (`> $OUT` / `eval "$CMD"` / `xargs rm` など)。
    pub opaque: bool,
}

/// シェルのコマンド行から、**書き込まれる可能性のあるパス**を抜く。
/// 取りこぼしより過検出を選ぶ (fail-closed 側)。
///
/// 拾える形と拾えない形はモジュール doc を参照。判らないものは
/// [`opaque_write`] 側に出る。
pub fn write_targets(cmd: &str) -> Vec<String> {
    scan(cmd).targets
}

/// **書き込みらしい語を含むのに対象が特定できない**か。
///
/// `> $OUT` のように値が実行時にしか決まらないもの、`eval` / `patch` のように
/// 中身を追えないものが該当する。ここで `true` でも**拒否には使わない**
/// (理由は [`crate::agents::hook_write_targets`] の doc)。
pub fn opaque_write(cmd: &str) -> bool {
    scan(cmd).opaque
}

/// 1 回の走査で両方を出す内部の入口。公開しているのは [`write_targets`] と
/// [`opaque_write`] の 2 つだけ — **呼ばれない公開関数を残さない**ため。
fn scan(cmd: &str) -> Scan {
    let mut out = Scan::default();
    scan_into(cmd, 0, &mut out);
    out
}

// ═══════════════════════════════════════════════════════════════════════════
//  字句解析 — クォート・エスケープ・コメント・演算子
// ═══════════════════════════════════════════════════════════════════════════

/// 1 語。`expanded` = 変数展開やコマンド置換を含んでいた (= 値が判らない)。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Word {
    text: String,
    expanded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    Word(Word),
    /// クォートされていない演算子だけがここへ来る。
    Op(String),
}

fn flush_word(out: &mut Vec<Tok>, buf: &mut String, in_word: &mut bool, expanded: &mut bool) {
    if *in_word {
        out.push(Tok::Word(Word {
            text: std::mem::take(buf),
            expanded: *expanded,
        }));
    }
    buf.clear();
    *in_word = false;
    *expanded = false;
}

/// 演算子を最長一致で取る。戻り値は (演算子, 進める文字数)。
fn lex_op(cs: &[char], i: usize) -> (String, usize) {
    let at = |k: usize| cs.get(i + k).copied();
    let (a, b, c) = (at(0), at(1), at(2));
    let three = |x: char, y: char, z: char| a == Some(x) && b == Some(y) && c == Some(z);
    let two = |x: char, y: char| a == Some(x) && b == Some(y);
    if three('<', '<', '<') {
        return ("<<<".to_string(), 3);
    }
    if three('&', '>', '>') {
        return ("&>>".to_string(), 3);
    }
    for (x, y) in [
        ('&', '>'),
        ('&', '&'),
        ('|', '|'),
        ('|', '&'),
        ('>', '>'),
        ('>', '|'),
        ('>', '&'),
        ('<', '<'),
        ('<', '&'),
        (';', ';'),
    ] {
        if two(x, y) {
            return (format!("{x}{y}"), 2);
        }
    }
    (a.unwrap_or(' ').to_string(), 1)
}

/// コマンド行を字句へ。`subs` にはコマンド置換の中身が積まれる (後で再帰する)。
fn lex(s: &str, subs: &mut Vec<String>) -> Vec<Tok> {
    let cs: Vec<char> = s.chars().collect();
    let n = cs.len();
    let mut out: Vec<Tok> = Vec::new();
    let mut buf = String::new();
    let mut in_word = false;
    let mut expanded = false;
    // 語がクォートもエスケープも通っていない = `2>` の fd 指定になり得る。
    let mut plain = true;
    let mut i = 0usize;
    while i < n {
        let ch = cs[i];
        match ch {
            ' ' | '\t' | '\r' => {
                flush_word(&mut out, &mut buf, &mut in_word, &mut expanded);
                plain = true;
                i += 1;
            }
            '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => {
                // `2> f` のような fd 前置きは、演算子側へ畳む。
                let fd_prefix = in_word
                    && plain
                    && !buf.is_empty()
                    && (ch == '<' || ch == '>')
                    && buf.chars().all(|c| c.is_ascii_digit());
                let mut op = String::new();
                if fd_prefix {
                    op.push_str(&buf);
                    buf.clear();
                    in_word = false;
                    expanded = false;
                } else {
                    flush_word(&mut out, &mut buf, &mut in_word, &mut expanded);
                }
                plain = true;
                let (o, adv) = lex_op(&cs, i);
                op.push_str(&o);
                out.push(Tok::Op(op));
                i += adv;
            }
            // 語の頭に来た `#` だけがコメント (`foo#bar` は語のまま)。
            '#' if !in_word => {
                while i < n && cs[i] != '\n' {
                    i += 1;
                }
            }
            '\'' => {
                in_word = true;
                plain = false;
                i += 1;
                while i < n && cs[i] != '\'' {
                    buf.push(cs[i]);
                    i += 1;
                }
                i = (i + 1).min(n);
            }
            '"' => {
                in_word = true;
                plain = false;
                i += 1;
                while i < n && cs[i] != '"' {
                    if cs[i] == '\\' && i + 1 < n {
                        i += 1;
                        buf.push(cs[i]);
                        i += 1;
                        continue;
                    }
                    if cs[i] == '$' || cs[i] == '`' {
                        expanded = true;
                    }
                    buf.push(cs[i]);
                    i += 1;
                }
                i = (i + 1).min(n);
            }
            '\\' => {
                plain = false;
                i += 1;
                if i < n {
                    // 行継続は語を作らない。
                    if cs[i] != '\n' {
                        in_word = true;
                        buf.push(cs[i]);
                    }
                    i += 1;
                }
            }
            '$' | '`' => {
                in_word = true;
                plain = false;
                expanded = true;
                i = lex_expansion(&cs, i, &mut buf, subs);
            }
            _ => {
                in_word = true;
                buf.push(ch);
                i += 1;
            }
        }
    }
    flush_word(&mut out, &mut buf, &mut in_word, &mut expanded);
    out
}

/// `$name` / `${…}` / `$(…)` / `` `…` `` を読み飛ばす。置換の中身は `subs` へ。
fn lex_expansion(cs: &[char], start: usize, buf: &mut String, subs: &mut Vec<String>) -> usize {
    let n = cs.len();
    let mut i = start;
    if cs[i] == '`' {
        i += 1;
        let from = i;
        while i < n && cs[i] != '`' {
            if cs[i] == '\\' {
                i += 1;
            }
            i += 1;
        }
        subs.push(cs[from..i.min(n)].iter().collect());
        return (i + 1).min(n);
    }
    // ここから `$`
    match cs.get(i + 1) {
        Some('(') => {
            let from = i + 2;
            let mut depth = 0usize;
            let mut j = i + 1;
            while j < n {
                match cs[j] {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            subs.push(cs[from.min(n)..j.min(n)].iter().collect());
            buf.push_str("$()");
            (j + 1).min(n)
        }
        Some('{') => {
            let mut j = i + 1;
            while j < n && cs[j] != '}' {
                j += 1;
            }
            buf.push_str("${}");
            (j + 1).min(n)
        }
        _ => {
            buf.push('$');
            i += 1;
            while i < n && (cs[i].is_ascii_alphanumeric() || cs[i] == '_') {
                buf.push(cs[i]);
                i += 1;
            }
            i
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  コマンドのカタログ — リテラルはここだけ
// ═══════════════════════════════════════════════════════════════════════════

/// 非オプション引数の扱い。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// 全部が書き込み先 (`rm` / `touch` / `truncate` / `tee` / `mv`)。
    All,
    /// 最後だけが書き込み先 (`cp` / `install` / `ln` — 手前は読み元)。
    Last,
    /// `key=value` の値 (`dd of=f`)。
    KeyValue(&'static str),
    /// その場編集。フラグが立っているときだけ書く。
    InPlace(InPlace),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InPlace {
    Sed,
    Perl,
    Awk,
}

/// 1 コマンド分の規則。
struct CmdRule {
    /// 実行ファイル名 (basename)。Windows では小文字化して比較する。
    names: &'static [&'static str],
    rule: Rule,
    /// **次の語を値として取る**オプション (その語はファイルではない)。
    val_opts: &'static [&'static str],
    /// **値そのものが書き込み先**になるオプション (`-t DIR`)。
    dest_opts: &'static [&'static str],
}

/// どの OS でも同じ規則で読めるコマンド。
const CMD_RULES: &[CmdRule] = &[
    CmdRule {
        names: &["tee"],
        rule: Rule::All,
        val_opts: &["--output-error"],
        dest_opts: &[],
    },
    CmdRule {
        names: &["rm", "unlink", "shred"],
        rule: Rule::All,
        val_opts: &[],
        dest_opts: &[],
    },
    CmdRule {
        names: &["touch"],
        rule: Rule::All,
        val_opts: &["-d", "-r", "-t", "--date", "--reference", "--time"],
        dest_opts: &[],
    },
    CmdRule {
        names: &["truncate"],
        rule: Rule::All,
        val_opts: &["-s", "-r", "--size", "--reference"],
        dest_opts: &[],
    },
    // `mv a b` は **a も消える** ので両側を書き込み先として扱う。
    CmdRule {
        names: &["mv"],
        rule: Rule::All,
        val_opts: &["-S", "--suffix"],
        dest_opts: &["-t", "--target-directory"],
    },
    CmdRule {
        names: &["cp"],
        rule: Rule::Last,
        val_opts: &["-S", "--suffix"],
        dest_opts: &["-t", "--target-directory"],
    },
    CmdRule {
        names: &["install"],
        rule: Rule::Last,
        val_opts: &[
            "-m", "-o", "-g", "-S", "--mode", "--owner", "--group", "--suffix",
        ],
        dest_opts: &["-t", "--target-directory"],
    },
    CmdRule {
        names: &["ln"],
        rule: Rule::Last,
        val_opts: &["-S", "--suffix"],
        dest_opts: &["-t", "--target-directory"],
    },
    CmdRule {
        names: &["dd"],
        rule: Rule::KeyValue("of="),
        val_opts: &[],
        dest_opts: &[],
    },
    CmdRule {
        names: &["sed", "gsed"],
        rule: Rule::InPlace(InPlace::Sed),
        val_opts: &[],
        dest_opts: &[],
    },
    CmdRule {
        names: &["perl"],
        rule: Rule::InPlace(InPlace::Perl),
        val_opts: &[],
        dest_opts: &[],
    },
    CmdRule {
        names: &["awk", "gawk", "mawk", "nawk"],
        rule: Rule::InPlace(InPlace::Awk),
        val_opts: &[],
        dest_opts: &[],
    },
];

/// `cmd.exe` の内蔵コマンド。**Windows でだけ**引く
/// (POSIX 側の `copy` / `move` は別物なので混ぜない)。
const WINDOWS_CMD_RULES: &[CmdRule] = &[
    CmdRule {
        names: &["copy", "xcopy", "robocopy"],
        rule: Rule::Last,
        val_opts: &[],
        dest_opts: &[],
    },
    CmdRule {
        names: &["move", "ren", "rename"],
        rule: Rule::All,
        val_opts: &[],
        dest_opts: &[],
    },
    CmdRule {
        names: &["del", "erase"],
        rule: Rule::All,
        val_opts: &[],
        dest_opts: &[],
    },
];

/// 「本当のコマンドはこの後ろ」の前置き。
struct Prefix {
    name: &'static str,
    /// 次の語を値として取るオプション。
    val_opts: &'static [&'static str],
    /// 読み飛ばす非オプション引数の数 (`timeout 5s cmd` の `5s`)。
    drop_args: usize,
}

const PREFIXES: &[Prefix] = &[
    Prefix {
        name: "sudo",
        val_opts: &["-u", "-g", "-U", "-p", "-C", "--user", "--group"],
        drop_args: 0,
    },
    Prefix {
        name: "doas",
        val_opts: &["-u", "-C"],
        drop_args: 0,
    },
    Prefix {
        name: "env",
        val_opts: &["-u", "--unset", "-C", "--chdir"],
        drop_args: 0,
    },
    Prefix {
        name: "nohup",
        val_opts: &[],
        drop_args: 0,
    },
    Prefix {
        name: "command",
        val_opts: &[],
        drop_args: 0,
    },
    Prefix {
        name: "builtin",
        val_opts: &[],
        drop_args: 0,
    },
    Prefix {
        name: "exec",
        val_opts: &[],
        drop_args: 0,
    },
    Prefix {
        name: "time",
        val_opts: &["-o", "-f"],
        drop_args: 0,
    },
    Prefix {
        name: "timeout",
        val_opts: &["-s", "--signal", "-k", "--kill-after"],
        drop_args: 1,
    },
    Prefix {
        name: "nice",
        val_opts: &["-n"],
        drop_args: 0,
    },
    Prefix {
        name: "ionice",
        val_opts: &["-c", "-n", "-p"],
        drop_args: 0,
    },
    Prefix {
        name: "stdbuf",
        val_opts: &["-i", "-o", "-e"],
        drop_args: 0,
    },
    // 対象は標準入力から来るので**必ず** opaque になるが、後続のコマンドは見る。
    Prefix {
        name: "xargs",
        val_opts: &["-I", "-i", "-n", "-P", "-d", "-a", "-E", "-L", "-s"],
        drop_args: 0,
    },
];

/// `-c "…"` の中身を再帰で見るシェル。
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "ash", "busybox"];

/// **書き込むと判っているが、対象は追えない**コマンド。
/// 警告 (opaque) を立てるだけで、拒否には使わない。
const OPAQUE_CMDS: &[&str] = &["eval", "source", ".", "patch"];

/// `find` がファイルを触る述語。
const FIND_WRITE_PREDS: &[&str] = &["-delete", "-exec", "-execdir", "-ok", "-okdir"];

// ═══════════════════════════════════════════════════════════════════════════
//  走査
// ═══════════════════════════════════════════════════════════════════════════

fn scan_into(cmd: &str, depth: usize, out: &mut Scan) {
    if depth > MAX_DEPTH || cmd.trim().is_empty() {
        return;
    }
    let mut subs: Vec<String> = Vec::new();
    let toks = lex(cmd, &mut subs);
    for simple in simple_commands(&toks) {
        let (argv, writes) = split_redirects(&simple);
        for w in &writes {
            push_target(w, out);
        }
        command_targets(&argv, depth, out);
    }
    for s in subs {
        scan_into(&s, depth + 1, out);
    }
}

/// 制御演算子で区切って「単純コマンド」の列にする。
fn simple_commands(toks: &[Tok]) -> Vec<Vec<Tok>> {
    const SEPS: &[&str] = &[";", ";;", "&&", "||", "|", "|&", "&", "(", ")", "\n"];
    let mut out: Vec<Vec<Tok>> = Vec::new();
    let mut cur: Vec<Tok> = Vec::new();
    for t in toks {
        match t {
            Tok::Op(o) if SEPS.contains(&o.as_str()) => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(t.clone()),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// リダイレクトの種別。`Some(true)` = 書き込み、`Some(false)` = 読み込み。
fn redirect_kind(op: &str) -> Option<bool> {
    let rest = op.trim_start_matches(|c: char| c.is_ascii_digit());
    match rest {
        ">" | ">>" | ">|" | ">&" | "&>" | "&>>" => Some(true),
        "<" | "<<" | "<<<" | "<&" => Some(false),
        _ => None,
    }
}

/// fd の複製 (`2>&1` の `1`、`>&-` の `-`) は**ファイルではない**。
fn is_fd_word(t: &str) -> bool {
    t == "-" || t.starts_with('&') || (!t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
}

/// 単純コマンドを「引数」と「リダイレクト先」に分ける。
fn split_redirects(toks: &[Tok]) -> (Vec<Word>, Vec<Word>) {
    let mut argv: Vec<Word> = Vec::new();
    let mut writes: Vec<Word> = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        match &toks[i] {
            Tok::Word(w) => {
                argv.push(w.clone());
                i += 1;
            }
            Tok::Op(o) => {
                let next = match toks.get(i + 1) {
                    Some(Tok::Word(w)) => Some(w.clone()),
                    _ => None,
                };
                match redirect_kind(o) {
                    Some(true) => {
                        if let Some(w) = &next {
                            if !is_fd_word(&w.text) {
                                writes.push(w.clone());
                            }
                        }
                        i += if next.is_some() { 2 } else { 1 };
                    }
                    // 読み込み (`< f`) とヒアドキュメントの区切り語は捨てる。
                    Some(false) => i += if next.is_some() { 2 } else { 1 },
                    None => i += 1,
                }
            }
        }
    }
    (argv, writes)
}

/// パスとして記録する。展開を含む語は**値が判らない**ので opaque へ倒す。
fn push_target(w: &Word, out: &mut Scan) {
    if w.expanded {
        out.opaque = true;
        return;
    }
    let t = w.text.trim();
    if !keep_path(t) {
        return;
    }
    if !out.targets.iter().any(|x| x == t) {
        out.targets.push(t.to_string());
    }
}

/// 捨ててよい書き込み先 (実ファイルではない)。
fn keep_path(t: &str) -> bool {
    if t.is_empty() || t == "-" {
        return false;
    }
    if t.starts_with("/dev/") {
        return false;
    }
    // Windows の `NUL` / `CON` は特殊デバイス名。
    if cfg!(windows) && (t.eq_ignore_ascii_case("nul") || t.eq_ignore_ascii_case("con")) {
        return false;
    }
    true
}

/// 実行ファイル名を基底名へ。Windows では区切りと拡張子と大小を吸収する。
fn base_name(cmd: &str) -> String {
    let mut s = cmd;
    if let Some(i) = s.rfind('/') {
        s = &s[i + 1..];
    }
    if cfg!(windows) {
        if let Some(i) = s.rfind('\\') {
            s = &s[i + 1..];
        }
    }
    let mut s = s.to_string();
    if cfg!(windows) {
        s = s.to_ascii_lowercase();
        for ext in [".exe", ".cmd", ".bat", ".com"] {
            if let Some(stem) = s.strip_suffix(ext) {
                if !stem.is_empty() {
                    s = stem.to_string();
                }
                break;
            }
        }
    }
    s
}

fn rule_for(name: &str) -> Option<&'static CmdRule> {
    const EMPTY: &[CmdRule] = &[];
    // `cfg!` で分岐する (`#[cfg]` にすると非 Windows で表が未使用になる)。
    let extra: &'static [CmdRule] = if cfg!(windows) {
        WINDOWS_CMD_RULES
    } else {
        EMPTY
    };
    CMD_RULES
        .iter()
        .chain(extra.iter())
        .find(|r| r.names.contains(&name))
}

/// `NAME=VALUE` の前置き代入か。
fn is_assignment(t: &str) -> bool {
    match t.split_once('=') {
        Some((k, _)) => {
            !k.is_empty()
                && k.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// 前置き (`sudo` 等) と環境変数代入を落として、本体の引数列を返す。
fn strip_prefixes(argv: &[Word]) -> Vec<Word> {
    let mut args = argv.to_vec();
    for _ in 0..MAX_DEPTH * 2 {
        // 環境変数代入の前置き。
        while !args.is_empty() && is_assignment(&args[0].text) {
            args.remove(0);
        }
        let Some(head) = args.first() else {
            return args;
        };
        let name = base_name(&head.text);
        let Some(p) = PREFIXES.iter().find(|p| p.name == name) else {
            return args;
        };
        args.remove(0);
        let mut dropped = 0usize;
        while !args.is_empty() {
            let t = args[0].text.clone();
            if t.starts_with('-') && t.len() > 1 {
                let key = t.split('=').next().unwrap_or(&t).to_string();
                let takes = p.val_opts.contains(&key.as_str());
                args.remove(0);
                // `-I{}` のような**くっついた**形は値を消費しない。
                if takes && !t.contains('=') && key.len() == t.len() && !args.is_empty() {
                    args.remove(0);
                }
                continue;
            }
            if dropped < p.drop_args {
                args.remove(0);
                dropped += 1;
                continue;
            }
            break;
        }
    }
    args
}

/// 単純コマンドの引数列から書き込み先を出す。
fn command_targets(argv: &[Word], depth: usize, out: &mut Scan) {
    let args = strip_prefixes(argv);
    let Some(head) = args.first() else {
        return;
    };
    let name = base_name(&head.text);
    let rest = &args[1..];

    // `bash -c "…"` の中身は本物のコマンド行なので再帰する。
    if SHELLS.contains(&name.as_str()) {
        for (i, a) in rest.iter().enumerate() {
            if a.text == "-c" {
                if let Some(inner) = rest.get(i + 1) {
                    if inner.expanded {
                        out.opaque = true;
                    } else {
                        scan_into(&inner.text, depth + 1, out);
                    }
                }
                return;
            }
        }
        return;
    }
    if OPAQUE_CMDS.contains(&name.as_str()) {
        out.opaque = true;
        return;
    }
    if name == "find"
        && rest
            .iter()
            .any(|a| FIND_WRITE_PREDS.contains(&a.text.as_str()))
    {
        out.opaque = true;
        return;
    }
    let Some(r) = rule_for(&name) else {
        // **既定は通す。** `ls` や `cargo test` まで止めると、ユーザーは
        // この機能ごと切る (切られたら保証はゼロ)。
        return;
    };

    let before = out.targets.len();
    let writes = apply_rule(r, rest, out);
    // 「書くはずのコマンドなのに 1 件も出せなかった」= 追えていない印。
    if writes && out.targets.len() == before && !out.opaque {
        out.opaque = true;
    }
}

/// 規則を当てる。**書き込むはずのコマンドだった**なら `true`。
fn apply_rule(r: &CmdRule, rest: &[Word], out: &mut Scan) -> bool {
    match r.rule {
        Rule::KeyValue(key) => {
            let mut found = false;
            for a in rest {
                if let Some(v) = a.text.strip_prefix(key) {
                    found = true;
                    push_target(
                        &Word {
                            text: v.to_string(),
                            expanded: a.expanded,
                        },
                        out,
                    );
                }
            }
            // `of=` が無ければ標準出力へ書くだけ = ファイルは触らない。
            found
        }
        Rule::All | Rule::Last => {
            let (pos, dest) = classify_args(rest, r);
            for d in &dest {
                push_target(d, out);
            }
            if !dest.is_empty() {
                // `-t DIR` があるときの位置引数は元ファイル。
                // `mv` は元も消えるので拾う。
                if matches!(r.rule, Rule::All) {
                    for p in &pos {
                        push_target(p, out);
                    }
                }
                return true;
            }
            match r.rule {
                Rule::All => {
                    for p in &pos {
                        push_target(p, out);
                    }
                }
                _ => {
                    if let Some(last) = pos.last() {
                        push_target(last, out);
                    }
                }
            }
            true
        }
        Rule::InPlace(kind) => {
            let (files, inplace) = match kind {
                InPlace::Sed => inplace_sed(rest),
                InPlace::Perl => inplace_perl(rest),
                InPlace::Awk => inplace_awk(rest),
            };
            if !inplace {
                return false; // その場編集でなければ標準出力へ書くだけ。
            }
            for f in &files {
                push_target(f, out);
            }
            true
        }
    }
}

/// 位置引数と「値そのものが宛先」のオプション値へ分ける。
fn classify_args(rest: &[Word], r: &CmdRule) -> (Vec<Word>, Vec<Word>) {
    let mut pos: Vec<Word> = Vec::new();
    let mut dest: Vec<Word> = Vec::new();
    let mut i = 0usize;
    let mut only_pos = false;
    while i < rest.len() {
        let a = &rest[i];
        let t = a.text.as_str();
        if only_pos || !(t.starts_with('-') && t.len() > 1) {
            pos.push(a.clone());
            i += 1;
            continue;
        }
        if t == "--" {
            only_pos = true;
            i += 1;
            continue;
        }
        let (key, attached) = match t.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (t.to_string(), None),
        };
        if r.dest_opts.contains(&key.as_str()) {
            match attached {
                Some(v) => dest.push(Word {
                    text: v,
                    expanded: a.expanded,
                }),
                None => {
                    if let Some(n) = rest.get(i + 1) {
                        dest.push(n.clone());
                        i += 1;
                    }
                }
            }
        } else if r.val_opts.contains(&key.as_str()) && attached.is_none() {
            i += 1; // 次の語はこのオプションの値。
        }
        i += 1;
    }
    (pos, dest)
}

/// `sed` — GNU の `-i` / `-i.bak`、BSD の `-i ''` の両方を見る。
fn inplace_sed(rest: &[Word]) -> (Vec<Word>, bool) {
    let mut inplace = false;
    let mut script_taken = false;
    let mut files: Vec<Word> = Vec::new();
    let mut only_pos = false;
    let mut i = 0usize;
    while i < rest.len() {
        let t = rest[i].text.clone();
        if only_pos || !(t.starts_with('-') && t.len() > 1) {
            if !script_taken && !only_pos {
                script_taken = true; // 最初の位置引数はスクリプト本体。
            } else {
                files.push(rest[i].clone());
            }
            i += 1;
            continue;
        }
        if t == "--" {
            only_pos = true;
            i += 1;
            continue;
        }
        if let Some(long) = t.strip_prefix("--") {
            let key = long.split('=').next().unwrap_or(long);
            if key == "in-place" {
                inplace = true;
            } else if matches!(key, "expression" | "file") {
                script_taken = true;
                if !long.contains('=') {
                    i += 1;
                }
            }
            i += 1;
            continue;
        }
        // 短オプションの塊 (`-ni` / `-i.bak` / `-e`)。
        let chars: Vec<char> = t[1..].chars().collect();
        let mut k = 0usize;
        let mut eat_next = false;
        while k < chars.len() {
            match chars[k] {
                'i' => {
                    inplace = true;
                    // GNU は `-i` / `-iSUF`、BSD は `-i ''`。
                    // 次の語が空か `.` 始まりなら BSD の接尾辞として食う。
                    if k + 1 == chars.len() {
                        if let Some(nx) = rest.get(i + 1) {
                            if nx.text.is_empty() || nx.text.starts_with('.') {
                                eat_next = true;
                            }
                        }
                    }
                    k = chars.len();
                }
                'e' | 'f' | 'l' => {
                    script_taken = true;
                    if k + 1 == chars.len() {
                        eat_next = true;
                    }
                    k = chars.len();
                }
                _ => k += 1,
            }
        }
        i += 1;
        if eat_next {
            i += 1;
        }
    }
    (files, inplace)
}

/// `perl -i` / `perl -pi -e '…' f`。
fn inplace_perl(rest: &[Word]) -> (Vec<Word>, bool) {
    /// 引数を取らない短オプション。塊 (`-pi`) を左から舐めるため、
    /// **これ以外の文字が来たらそこから先は値**とみなして打ち切る。
    const FLAGS: &[char] = &[
        'n', 'p', 'l', 'a', 'c', 's', 'w', 'u', 'U', 'W', 'X', 'T', 't',
    ];
    let mut inplace = false;
    let mut script_taken = false;
    let mut files: Vec<Word> = Vec::new();
    let mut only_pos = false;
    let mut i = 0usize;
    while i < rest.len() {
        let t = rest[i].text.clone();
        if only_pos || !(t.starts_with('-') && t.len() > 1) {
            if !script_taken && !only_pos {
                script_taken = true; // スクリプト本体。
            } else {
                files.push(rest[i].clone());
            }
            i += 1;
            continue;
        }
        if t == "--" {
            only_pos = true;
            i += 1;
            continue;
        }
        let chars: Vec<char> = t[1..].chars().collect();
        let mut k = 0usize;
        let mut eat_next = false;
        while k < chars.len() {
            let c = chars[k];
            if c == 'i' {
                inplace = true; // 残りは接尾辞 (`-i.bak`)。
                break;
            }
            if c == 'e' || c == 'E' {
                script_taken = true;
                if k + 1 == chars.len() {
                    eat_next = true;
                }
                break;
            }
            if !FLAGS.contains(&c) {
                // 値を取るオプション (`-I` / `-M` / `-F` …)。残りは値。
                break;
            }
            k += 1;
        }
        i += 1;
        if eat_next {
            i += 1;
        }
    }
    (files, inplace)
}

/// `gawk -i inplace '…' f`。
fn inplace_awk(rest: &[Word]) -> (Vec<Word>, bool) {
    const VAL_OPTS: &[&str] = &[
        "-v",
        "-F",
        "-f",
        "-i",
        "--assign",
        "--field-separator",
        "--file",
        "--include",
        "--source",
    ];
    let mut inplace = false;
    let mut script_taken = false;
    let mut files: Vec<Word> = Vec::new();
    let mut only_pos = false;
    let mut i = 0usize;
    while i < rest.len() {
        let t = rest[i].text.clone();
        if only_pos || !(t.starts_with('-') && t.len() > 1) {
            if !script_taken && !only_pos {
                script_taken = true; // プログラム本体。
            } else {
                files.push(rest[i].clone());
            }
            i += 1;
            continue;
        }
        if t == "--" {
            only_pos = true;
            i += 1;
            continue;
        }
        let (key, attached) = match t.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (t.to_string(), None),
        };
        if key == "-i" || key == "--include" {
            let val = match &attached {
                Some(v) => Some(v.clone()),
                None => rest.get(i + 1).map(|w| w.text.clone()),
            };
            if val.as_deref() == Some("inplace") {
                inplace = true;
            }
        }
        if matches!(key.as_str(), "-f" | "--file" | "--source") {
            script_taken = true;
        }
        if VAL_OPTS.contains(&key.as_str()) && attached.is_none() {
            i += 1;
        }
        i += 1;
    }
    (files, inplace)
}

// ═══════════════════════════════════════════════════════════════════════════
//  テスト — 「このコマンドからこのパスが出る」を表で固定する
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// 表 1 行を検査する。
    fn 一致(cases: &[(&str, &[&str])]) {
        for (cmd, want) in cases {
            let got = write_targets(cmd);
            let want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
            assert_eq!(got, want, "コマンド: {cmd}");
        }
    }

    #[test]
    fn リダイレクトの書き込み先を拾う() {
        一致(&[
            ("printf B_WON > shared.rs", &["shared.rs"]),
            ("echo x >> a.txt", &["a.txt"]),
            ("cargo build 2> err.log", &["err.log"]),
            ("cargo build &> all.log", &["all.log"]),
            ("cargo build &>> all.log", &["all.log"]),
            ("cargo build > out.txt 2>&1", &["out.txt"]),
            ("cat x >| clobber.txt", &["clobber.txt"]),
            ("cat x >& both.log", &["both.log"]),
            ("echo x>compact.txt", &["compact.txt"]),
            ("echo hi > /dev/null", &[]),
            ("cat < in.txt", &[]),
            ("sort < in.txt > out.txt", &["out.txt"]),
        ]);
    }

    #[test]
    fn teeとパイプ経由の書き込みを拾う() {
        一致(&[
            ("make |& tee build.log", &["build.log"]),
            ("cargo test | tee -a t.log", &["t.log"]),
            ("echo x | tee a.txt b.txt", &["a.txt", "b.txt"]),
            ("echo x | sudo tee /etc/hosts", &["/etc/hosts"]),
        ]);
    }

    #[test]
    fn その場編集を拾う() {
        一致(&[
            // GNU
            ("sed -i 's/a/b/' src/x.rs", &["src/x.rs"]),
            ("sed -i.bak s/a/b/ f.txt", &["f.txt"]),
            ("sed -n -i -e 's/a/b/' a.rs b.rs", &["a.rs", "b.rs"]),
            // BSD (`-i ''`)
            ("sed -i '' -e 's/a/b/' src/x.rs", &["src/x.rs"]),
            ("sed -i '' 's/a/b/' src/x.rs", &["src/x.rs"]),
            // その場編集でなければ書かない
            ("sed 's/a/b/' src/x.rs", &[]),
            ("sed -n '1,5p' src/x.rs", &[]),
            // perl / awk
            ("perl -pi -e 's/a/b/' f.rs", &["f.rs"]),
            ("perl -i.bak -pe 's/a/b/' f.rs", &["f.rs"]),
            ("perl -pe 's/a/b/' f.rs", &[]),
            ("gawk -i inplace '{print}' f.txt", &["f.txt"]),
            ("awk '{print}' f.txt", &[]),
        ]);
    }

    #[test]
    fn ファイル操作の書き込み先を拾う() {
        一致(&[
            ("cp a.rs b.rs", &["b.rs"]),
            ("cp -r src dst", &["dst"]),
            ("cp -t out/ a.rs b.rs", &["out/"]),
            // mv は**元も消える**ので両側
            ("mv a.rs b.rs", &["a.rs", "b.rs"]),
            ("rm -f x.rs y.rs", &["x.rs", "y.rs"]),
            ("rm -rf build", &["build"]),
            (
                "install -m 755 zai /usr/local/bin/zai",
                &["/usr/local/bin/zai"],
            ),
            ("truncate -s 0 big.log", &["big.log"]),
            ("dd if=/dev/zero of=disk.img bs=1M", &["disk.img"]),
            ("touch new.rs", &["new.rs"]),
            ("touch -r ref.rs new.rs", &["new.rs"]),
            ("ln -s a.rs link.rs", &["link.rs"]),
        ]);
    }

    #[test]
    fn 複文とクォートと前置きを解く() {
        一致(&[
            ("cd sub && printf hi > y.txt", &["y.txt"]),
            ("mkdir -p d; echo x > d/f.txt", &["d/f.txt"]),
            ("false || echo x > fallback.txt", &["fallback.txt"]),
            ("RUST_LOG=debug cargo run > out.log", &["out.log"]),
            ("env FOO=1 rm x.rs", &["x.rs"]),
            ("timeout 5s rm x.rs", &["x.rs"]),
            ("nohup sudo -u me rm x.rs", &["x.rs"]),
            ("echo x > 'my file.txt'", &["my file.txt"]),
            (r#"echo x > "sp ace.rs""#, &["sp ace.rs"]),
            ("echo x > a\\ b.txt", &["a b.txt"]),
            (r#"bash -c "echo x > inner.txt""#, &["inner.txt"]),
            ("/usr/bin/tee out.txt", &["out.txt"]),
            ("(cd sub && rm x.rs)", &["x.rs"]),
            (
                "echo a > one.txt\necho b > two.txt",
                &["one.txt", "two.txt"],
            ),
        ]);
    }

    /// **読むだけなのに `>` を含む**形を書き込みと誤認しない。
    #[test]
    fn 読むだけのコマンドを誤検出しない() {
        一致(&[
            ("grep '>' f.rs", &[]),
            (r#"grep ">" f.rs"#, &[]),
            (r#"echo ">""#, &[]),
            (r#"echo "a > b""#, &[]),
            ("echo '2> not-a-file'", &[]),
            ("grep -r 'a -> b' src/", &[]),
            ("# rm -rf > danger.txt", &[]),
            ("ls -la  # > danger.txt", &[]),
            ("ls -la", &[]),
            ("cargo test --bin zai", &[]),
            ("git status --porcelain", &[]),
            ("cat src/lease.rs", &[]),
            ("echo x \\> literal", &[]),
        ]);
    }

    /// 対象が判らない書き込みは**パスを出さず** opaque を立てる。
    #[test]
    fn 対象が判らない書き込みはopaqueになる() {
        let cases: &[&str] = &[
            "echo x > $OUT",
            "echo x > \"$OUT/f.txt\"",
            "sed -i 's/a/b/' $F",
            "rm -f ${TARGET}",
            "eval \"$CMD\"",
            "patch -p1 < fix.diff",
            "find . -name '*.tmp' -delete",
            "ls | xargs rm",
            "dd if=a of=$DST",
        ];
        for c in cases {
            let s = scan(c);
            assert!(s.opaque, "opaque を立てそこねた: {c}");
            assert!(
                s.targets.is_empty(),
                "値の判らないパスを出した: {c} -> {:?}",
                s.targets
            );
        }
    }

    /// 通してよいものに opaque を立てない (立てるほど警告が無意味になる)。
    #[test]
    fn 読み取りコマンドにopaqueを立てない() {
        for c in [
            "ls -la",
            "cargo test",
            "grep '>' f.rs",
            "cat a.rs",
            "sed -n '1p' a.rs",
            "echo x > out.txt",
            "cp a.rs b.rs",
        ] {
            assert!(!scan(c).opaque, "余計な opaque: {c}");
        }
    }

    /// コマンド置換の中身も見る (`$(...)` / backtick)。
    #[test]
    fn コマンド置換の中も見る() {
        assert!(write_targets("echo $(rm inner.rs)").contains(&"inner.rs".to_string()));
        assert!(write_targets("echo `rm inner.rs`").contains(&"inner.rs".to_string()));
    }

    /// 同じパスが 2 度出ても 1 件。
    #[test]
    fn 重複は畳む() {
        assert_eq!(
            write_targets("echo a > f.txt; echo b >> f.txt"),
            vec!["f.txt"]
        );
    }

    /// 再帰は有限で止まる (病的な入力で固まらない)。
    #[test]
    fn 深い入れ子でも止まる() {
        let mut s = "rm deep.rs".to_string();
        for _ in 0..12 {
            s = format!("bash -c \"{}\"", s.replace('"', "\\\""));
        }
        let _ = scan(&s); // 落ちない・戻ってくることが検査対象
    }

    /// 空・空白・壊れたクォートで panic しない。
    #[test]
    fn 壊れた入力でも落ちない() {
        for c in ["", "   ", "'", "\"", "$(", "${", "`", ">", "|", "&&", "<<"] {
            let _ = scan(c);
        }
    }
}
