//! VS Code `.vscode/tasks.json` (JSONC) の読み取り。
//!
//! この module は **純関数だけ** で構成する。副作用を持つのは
//! `load_tasks` (ファイル 1 本の読み取り) のみで、その中身も
//! 「読む → `parse_tasks` に渡す」だけに留める。こうしておくと
//! 解釈の全分岐をテーブルテストで固定でき、環境に依存しない。
//!
//! 対応範囲は意図的に狭い。`tasks[]` の
//! `label` / `type` / `command` / `args` / `options.cwd` / `options.env` /
//! `group` / `presentation.reveal` だけを見る。
//! `problemMatcher` / `dependsOn` / `runOptions` / `isBackground` などの
//! 未対応キーは **黙って無視** し、エラーにはしない (壊れた JSON でないなら
//! 一覧は出す、という方針)。
//!
//! 変数展開も 4 つだけ:
//! * `${workspaceFolder}` … `parse_tasks` の時点で `root` へ展開する。
//! * `${file}` / `${fileBasename}` / `${fileDirname}` … 文字列に残したまま
//!   `needs_file` を立て、実行直前に [`resolve`] で展開する。
//!
//! それ以外の `${...}` が残っているタスクは **一覧から落とさず**
//! `blocked` に理由を入れる。「なぜ実行できないか」が UI に出ないと、
//! ユーザーはタスクが消えた理由を推測するしかなくなるため。

use crate::jsonc::strip_jsonc;
use serde_json::Value;
use std::path::{Path, PathBuf};

// ===========================================================================
// 型
// ===========================================================================

/// tasks.json の `type`。未知/未指定は Shell (VS Code の既定)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TaskType {
    #[default]
    Shell,
    Process,
}

/// tasks.json の `group`。`clean` などは扱わないので None に落とす。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TaskGroup {
    #[default]
    None,
    Build,
    Test,
}

/// 1 つのタスク定義。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskDef {
    pub label: String,
    pub ty: TaskType,
    pub command: String,
    pub args: Vec<String>,
    /// options.cwd を展開・絶対化したもの (無ければ root)
    pub cwd: PathBuf,
    /// options.env。**キー昇順で決定的**に並べる
    pub env: Vec<(String, String)>,
    pub group: TaskGroup,
    pub is_default: bool,
    /// presentation.reveal == "never" なら false
    pub reveal: bool,
    /// `${file}` 系を含む → 実行時にアクティブファイルが要る
    pub needs_file: bool,
    /// 実行不可の理由 (None = 実行できる)
    pub blocked: Option<String>,
}

/// tasks.json 1 ファイル分の解釈結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TasksDoc {
    pub tasks: Vec<TaskDef>,
    /// ファイル全体が読めなかった理由 (None = 正常)
    pub error: Option<String>,
}

impl TasksDoc {
    /// group == Build かつ is_default のもの。無ければ最初の Build タスク。
    pub fn default_build(&self) -> Option<&TaskDef> {
        self.tasks
            .iter()
            .find(|t| t.group == TaskGroup::Build && t.is_default)
            .or_else(|| self.tasks.iter().find(|t| t.group == TaskGroup::Build))
    }
}

// ===========================================================================
// 変数
// ===========================================================================

/// `${workspaceFolder}` のトークン。
const VAR_WORKSPACE: &str = "${workspaceFolder}";

/// 実行時 (= [`resolve`]) に展開する変数名。ここに載っていない `${...}` は
/// 未対応として `blocked` に回す。
const FILE_VARS: [&str; 3] = ["file", "fileBasename", "fileDirname"];

/// 文字列中の `${...}` を走査して、
/// * ファイル変数を含むか (`needs_file`)
/// * 未対応の変数トークン (`${env:FOO}` 等、重複は 1 回だけ)
///
/// を集める。マルチバイト安全のため境界が確実な ASCII 位置だけで切る。
fn scan_vars(s: &str, needs_file: &mut bool, unsupported: &mut Vec<String>) {
    let b = s.as_bytes();
    let mut i = 0usize;
    while i + 1 < b.len() {
        if b[i] != b'$' || b[i + 1] != b'{' {
            i += 1;
            continue;
        }
        // `${` の直後から最初の `}` まで。閉じていなければ変数ではない。
        let Some(rel) = s[i + 2..].find('}') else {
            return;
        };
        let end = i + 2 + rel; // `}` の位置 (ASCII なので境界は安全)
        let name = &s[i + 2..end];
        if name == "workspaceFolder" {
            // parse_tasks で展開済み扱い。何もしない。
        } else if FILE_VARS.contains(&name) {
            *needs_file = true;
        } else {
            let token = s[i..=end].to_string();
            if !unsupported.contains(&token) {
                unsupported.push(token);
            }
        }
        i = end + 1;
    }
}

/// `${workspaceFolder}` を `root` に置き換える。
fn expand_workspace(s: &str, root: &Path) -> String {
    if s.contains(VAR_WORKSPACE) {
        s.replace(VAR_WORKSPACE, &root.to_string_lossy())
    } else {
        s.to_string()
    }
}

/// 走査 + `${workspaceFolder}` 展開をまとめて行う。
fn take(s: &str, root: &Path, needs_file: &mut bool, unsupported: &mut Vec<String>) -> String {
    scan_vars(s, needs_file, unsupported);
    expand_workspace(s, root)
}

// ===========================================================================
// パース
// ===========================================================================

/// **純関数**。`root` は `${workspaceFolder}` の展開先。
pub fn parse_tasks(text: &str, root: &Path) -> TasksDoc {
    if text.trim().is_empty() {
        return TasksDoc::default();
    }
    let clean = strip_jsonc(text);
    // コメントだけのファイルは「空」と同じ扱いにする (エラーではない)。
    if clean.trim().is_empty() {
        return TasksDoc::default();
    }
    let root_val: Value = match serde_json::from_str(&clean) {
        Ok(v) => v,
        Err(e) => return fail(format!("tasks.json を解釈できません: {e}")),
    };
    // top-level `version` は読み飛ばす (値で挙動を変えない)。
    let arr = match root_val.get("tasks") {
        Some(Value::Array(a)) => a,
        Some(_) => return fail("tasks.json の tasks が配列ではありません".to_string()),
        None => return fail("tasks.json に tasks 配列がありません".to_string()),
    };

    let mut tasks: Vec<TaskDef> = Vec::new();
    for item in arr {
        let Some(t) = parse_one(item, root) else {
            continue;
        };
        // label 重複は最初の 1 件だけ残す (一覧で見分けが付かないため)。
        if tasks.iter().any(|e| e.label == t.label) {
            continue;
        }
        tasks.push(t);
    }
    TasksDoc { tasks, error: None }
}

fn fail(msg: String) -> TasksDoc {
    TasksDoc {
        tasks: Vec::new(),
        error: Some(msg),
    }
}

/// 1 要素を TaskDef へ。label が無い/空なら None (= 一覧に出せないので落とす)。
fn parse_one(v: &Value, root: &Path) -> Option<TaskDef> {
    let label = v.get("label").and_then(Value::as_str).unwrap_or("").trim();
    if label.is_empty() {
        return None;
    }

    let mut needs_file = false;
    let mut unsupported: Vec<String> = Vec::new();

    let ty = match v.get("type").and_then(Value::as_str) {
        Some("process") => TaskType::Process,
        _ => TaskType::Shell,
    };

    let command = v
        .get("command")
        .and_then(Value::as_str)
        .map(|s| take(s, root, &mut needs_file, &mut unsupported))
        .unwrap_or_default();

    let args: Vec<String> = match v.get("args") {
        // 文字列は 1 要素の配列として扱う。
        Some(Value::String(s)) => vec![take(s, root, &mut needs_file, &mut unsupported)],
        Some(Value::Array(a)) => a
            .iter()
            // 配列内の非文字列要素は捨てる。
            .filter_map(Value::as_str)
            .map(|s| take(s, root, &mut needs_file, &mut unsupported))
            .collect(),
        _ => Vec::new(),
    };

    let options = v.get("options");
    let cwd = options
        .and_then(|o| o.get("cwd"))
        .and_then(Value::as_str)
        .map(|s| take(s, root, &mut needs_file, &mut unsupported))
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            let p = PathBuf::from(&s);
            if p.is_absolute() {
                p
            } else {
                root.join(p)
            }
        })
        .unwrap_or_else(|| root.to_path_buf());

    let mut env: Vec<(String, String)> = match options.and_then(|o| o.get("env")) {
        Some(Value::Object(m)) => m
            .iter()
            // 値が非文字列の項目だけ捨てる。
            .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s)))
            .map(|(k, s)| (k, take(s, root, &mut needs_file, &mut unsupported)))
            .collect(),
        // 非オブジェクトは無視。
        _ => Vec::new(),
    };
    // 実行コマンドを決定的にするため必ずキー昇順へ揃える。
    env.sort_by(|a, b| a.0.cmp(&b.0));

    let (group, is_default) = parse_group(v.get("group"));

    let reveal = v
        .get("presentation")
        .and_then(|p| p.get("reveal"))
        .and_then(Value::as_str)
        != Some("never");

    let blocked = if command.trim().is_empty() {
        Some("command がありません".to_string())
    } else if !unsupported.is_empty() {
        Some(format!("未対応の変数 {} を含みます", unsupported.join(" ")))
    } else {
        None
    };

    Some(TaskDef {
        label: label.to_string(),
        ty,
        command,
        args,
        cwd,
        env,
        group,
        is_default,
        reveal,
        needs_file,
        blocked,
    })
}

/// `"build"` のような文字列と `{"kind": "build", "isDefault": true}` の両対応。
fn parse_group(v: Option<&Value>) -> (TaskGroup, bool) {
    match v {
        Some(Value::String(s)) => (group_kind(s), false),
        Some(Value::Object(m)) => {
            let kind = m.get("kind").and_then(Value::as_str).unwrap_or("");
            // isDefault が bool 以外 (文字列 "true" 等) なら false。
            let is_default = m.get("isDefault").and_then(Value::as_bool).unwrap_or(false);
            (group_kind(kind), is_default)
        }
        _ => (TaskGroup::None, false),
    }
}

fn group_kind(s: &str) -> TaskGroup {
    match s {
        "build" => TaskGroup::Build,
        "test" => TaskGroup::Test,
        _ => TaskGroup::None,
    }
}

// ===========================================================================
// ファイル
// ===========================================================================

/// `<root>/.vscode/tasks.json`
pub fn tasks_json_path(root: &Path) -> PathBuf {
    root.join(".vscode").join("tasks.json")
}

/// ファイルを読んで parse。読めなければ `TasksDoc::default()` (error=None)。
///
/// 「ファイルが無い」は異常ではない (大半のフォルダに tasks.json は無い) ので、
/// ここでエラーを立てると UI が常時警告を出すことになる。
pub fn load_tasks(root: &Path) -> TasksDoc {
    match std::fs::read_to_string(tasks_json_path(root)) {
        Ok(s) => parse_tasks(&s, root),
        Err(_) => TasksDoc::default(),
    }
}

// ===========================================================================
// 実行行の組み立て
// ===========================================================================

/// 1 引数をシェルに安全に渡せる形へ引用する。
/// OS 差分は `cfg!` ではなく引数で選ぶ — そうしないと片側しかテストできない。
fn quote(arg: &str, windows: bool) -> String {
    if windows {
        // cmd.exe: `"` で囲み、内側の `"` は `""` に。
        format!("\"{}\"", arg.replace('"', "\"\""))
    } else {
        // POSIX: `'` で囲み、内側の `'` は `'\''` に。
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

fn build_line(ty: TaskType, command: &str, args: &[String], windows: bool) -> String {
    // Shell の command はシェルの生テキストなので触らない。
    let mut out = match ty {
        TaskType::Shell => command.to_string(),
        TaskType::Process => quote(command, windows),
    };
    for a in args {
        out.push(' ');
        out.push_str(&quote(a, windows));
    }
    out
}

/// 実行するシェル 1 行を組み立てる。`windows=true` は cmd.exe 用の引用、
/// false は POSIX シェル用の引用。
pub fn command_line(t: &TaskDef, windows: bool) -> String {
    build_line(t.ty, &t.command, &t.args, windows)
}

/// `${file}` `${fileBasename}` `${fileDirname}` を展開して実行行を返す。
/// blocked なタスクは Err(理由)。needs_file なのに `file` が None なら Err(理由)。
pub fn resolve(t: &TaskDef, file: Option<&Path>, windows: bool) -> Result<String, String> {
    if let Some(why) = &t.blocked {
        return Err(why.clone());
    }
    if !t.needs_file {
        return Ok(command_line(t, windows));
    }
    let Some(f) = file else {
        return Err("${file} を使うタスクにはアクティブなファイルが必要です".to_string());
    };
    let full = f.to_string_lossy().to_string();
    let base = f
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let dir = f
        .parent()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // 引用の前に置換する。そうしないとパス中の `'` / `"` が引用を破る。
    let sub = |s: &str| {
        s.replace("${fileBasename}", &base)
            .replace("${fileDirname}", &dir)
            .replace("${file}", &full)
    };
    let command = sub(&t.command);
    let args: Vec<String> = t.args.iter().map(|a| sub(a)).collect();
    Ok(build_line(t.ty, &command, &args, windows))
}

// ===========================================================================
// テスト
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の workspace root。実在しなくてよい (parse は純関数)。
    /// HOME や CWD に依存させないため temp_dir から導出する。
    fn t_root() -> PathBuf {
        std::env::temp_dir().join("zaivern-tasks-root")
    }

    fn parse(src: &str) -> TasksDoc {
        parse_tasks(src, &t_root())
    }

    fn one(src: &str) -> TaskDef {
        let doc = parse(src);
        assert!(doc.error.is_none(), "予期しないエラー: {:?}", doc.error);
        assert_eq!(doc.tasks.len(), 1, "タスクが 1 件ではない: {doc:?}");
        doc.tasks.into_iter().next().expect("1 件目")
    }

    // ---- 文書全体の受理/拒否 ----

    #[test]
    fn doc_level_table() {
        // (名前, 入力, タスク件数, error が立つか)
        let abs = t_root().join("elsewhere");
        let abs_json = serde_json::to_string(&abs.to_string_lossy().to_string()).expect("json");
        let cases: Vec<(&str, String, usize, bool)> = vec![
            ("空文字列", String::new(), 0, false),
            ("空白だけ", "  \n\t ".to_string(), 0, false),
            ("コメントだけ", "// nothing here\n".to_string(), 0, false),
            (
                "壊れた JSON",
                r#"{"tasks": [ {"label": }"#.to_string(),
                0,
                true,
            ),
            (
                "tasks がオブジェクト",
                r#"{"tasks": {"label": "a"}}"#.to_string(),
                0,
                true,
            ),
            ("tasks が数値", r#"{"tasks": 3}"#.to_string(), 0, true),
            ("tasks 欠落", r#"{"version": "2.0.0"}"#.to_string(), 0, true),
            ("top-level が配列", "[1, 2]".to_string(), 0, true),
            ("tasks が空配列", r#"{"tasks": []}"#.to_string(), 0, false),
            (
                "version あり",
                r#"{"version":"2.0.0","tasks":[{"label":"a","command":"c"}]}"#.to_string(),
                1,
                false,
            ),
            (
                "version なし",
                r#"{"tasks":[{"label":"a","command":"c"}]}"#.to_string(),
                1,
                false,
            ),
            (
                "コメント + 末尾カンマ",
                "{\n  // 行コメント\n  /* ブロック\n     コメント */\n  \"version\": \"2.0.0\",\n  \"tasks\": [\n    { \"label\": \"a\", \"command\": \"c\", },\n  ],\n}"
                    .to_string(),
                1,
                false,
            ),
            (
                "未対応キーは無視",
                r#"{"tasks":[{"label":"a","command":"c","problemMatcher":["$rustc"],"dependsOn":"b","runOptions":{"runOn":"folderOpen"},"isBackground":true}]}"#
                    .to_string(),
                1,
                false,
            ),
            (
                "label 欠落は落とす",
                r#"{"tasks":[{"command":"c"},{"label":"  ","command":"c"},{"label":"ok","command":"c"}]}"#
                    .to_string(),
                1,
                false,
            ),
            (
                "label 重複は先頭のみ",
                r#"{"tasks":[{"label":"a","command":"first"},{"label":"a","command":"second"}]}"#
                    .to_string(),
                1,
                false,
            ),
            (
                "非オブジェクト要素は落とす",
                r#"{"tasks":[1,"x",null,{"label":"a","command":"c"}]}"#.to_string(),
                1,
                false,
            ),
            (
                "options.cwd 絶対",
                format!(r#"{{"tasks":[{{"label":"a","command":"c","options":{{"cwd":{abs_json}}}}}]}}"#),
                1,
                false,
            ),
        ];
        for (name, src, want_len, want_err) in cases {
            let doc = parse(&src);
            assert_eq!(doc.tasks.len(), want_len, "[{name}] 件数: {doc:?}");
            assert_eq!(doc.error.is_some(), want_err, "[{name}] error: {doc:?}");
            assert_eq!(doc.tasks.is_empty(), want_len == 0, "[{name}] 空判定");
        }
    }

    #[test]
    fn duplicate_label_keeps_first() {
        let t =
            one(r#"{"tasks":[{"label":"a","command":"first"},{"label":"a","command":"second"}]}"#);
        assert_eq!(t.command, "first");
    }

    // ---- 個々のフィールド ----

    #[test]
    fn type_table() {
        let cases = [
            (r#""shell""#, TaskType::Shell),
            (r#""process""#, TaskType::Process),
            (r#""unknown""#, TaskType::Shell),
            ("123", TaskType::Shell),
        ];
        for (raw, want) in cases {
            let t = one(&format!(
                r#"{{"tasks":[{{"label":"a","command":"c","type":{raw}}}]}}"#
            ));
            assert_eq!(t.ty, want, "type={raw}");
        }
        // 未指定は Shell
        let t = one(r#"{"tasks":[{"label":"a","command":"c"}]}"#);
        assert_eq!(t.ty, TaskType::Shell);
    }

    #[test]
    fn command_missing_is_blocked_not_dropped() {
        for raw in [
            r#"{"tasks":[{"label":"a"}]}"#,
            r#"{"tasks":[{"label":"a","command":""}]}"#,
            r#"{"tasks":[{"label":"a","command":"   "}]}"#,
            r#"{"tasks":[{"label":"a","command":42}]}"#,
        ] {
            let t = one(raw);
            assert_eq!(
                t.blocked.as_deref(),
                Some("command がありません"),
                "src={raw}"
            );
        }
    }

    #[test]
    fn args_table() {
        let cases: [(&str, Vec<&str>); 5] = [
            (r#""one""#, vec!["one"]),
            (r#"["a","b"]"#, vec!["a", "b"]),
            (r#"["a",1,null,true,{},"b"]"#, vec!["a", "b"]),
            ("[]", vec![]),
            ("42", vec![]),
        ];
        for (raw, want) in cases {
            let t = one(&format!(
                r#"{{"tasks":[{{"label":"a","command":"c","args":{raw}}}]}}"#
            ));
            assert_eq!(t.args, want, "args={raw}");
        }
    }

    #[test]
    fn group_table() {
        // (raw, group, is_default)
        let cases = [
            (r#""build""#, TaskGroup::Build, false),
            (r#""test""#, TaskGroup::Test, false),
            (r#""clean""#, TaskGroup::None, false),
            (
                r#"{"kind":"build","isDefault":true}"#,
                TaskGroup::Build,
                true,
            ),
            (
                r#"{"kind":"build","isDefault":false}"#,
                TaskGroup::Build,
                false,
            ),
            (r#"{"kind":"build"}"#, TaskGroup::Build, false),
            (r#"{"kind":"test","isDefault":true}"#, TaskGroup::Test, true),
            // isDefault が bool 以外なら false
            (
                r#"{"kind":"build","isDefault":"true"}"#,
                TaskGroup::Build,
                false,
            ),
            (r#"{"kind":"build","isDefault":1}"#, TaskGroup::Build, false),
            (r#"{}"#, TaskGroup::None, false),
            ("null", TaskGroup::None, false),
            ("7", TaskGroup::None, false),
        ];
        for (raw, want_group, want_default) in cases {
            let t = one(&format!(
                r#"{{"tasks":[{{"label":"a","command":"c","group":{raw}}}]}}"#
            ));
            assert_eq!(t.group, want_group, "group={raw}");
            assert_eq!(t.is_default, want_default, "isDefault={raw}");
        }
        // group 未指定
        let t = one(r#"{"tasks":[{"label":"a","command":"c"}]}"#);
        assert_eq!(t.group, TaskGroup::None);
        assert!(!t.is_default);
    }

    #[test]
    fn reveal_table() {
        let cases = [
            (r#"{"reveal":"never"}"#, false),
            (r#"{"reveal":"always"}"#, true),
            (r#"{"reveal":"silent"}"#, true),
            (r#"{"reveal":123}"#, true),
            (r#"{}"#, true),
            ("null", true),
        ];
        for (raw, want) in cases {
            let t = one(&format!(
                r#"{{"tasks":[{{"label":"a","command":"c","presentation":{raw}}}]}}"#
            ));
            assert_eq!(t.reveal, want, "presentation={raw}");
        }
        let t = one(r#"{"tasks":[{"label":"a","command":"c"}]}"#);
        assert!(t.reveal, "presentation 未指定は表示する");
    }

    #[test]
    fn cwd_table() {
        let root = t_root();
        let abs = root.join("elsewhere");
        let abs_json = serde_json::to_string(&abs.to_string_lossy().to_string()).expect("json");
        // (raw JSON 値, 期待する cwd)
        let cases: Vec<(String, PathBuf)> = vec![
            (r#""sub/dir""#.to_string(), root.join("sub/dir")),
            (abs_json, abs),
            (
                r#""${workspaceFolder}/nested""#.to_string(),
                PathBuf::from(format!("{}/nested", root.to_string_lossy())),
            ),
            (r#""""#.to_string(), root.clone()),
            (r#""   ""#.to_string(), root.clone()),
            ("null".to_string(), root.clone()),
            ("42".to_string(), root.clone()),
        ];
        for (raw, want) in cases {
            let t = one(&format!(
                r#"{{"tasks":[{{"label":"a","command":"c","options":{{"cwd":{raw}}}}}]}}"#
            ));
            assert_eq!(t.cwd, want, "cwd={raw}");
        }
        // options ごと無い場合
        let t = one(r#"{"tasks":[{"label":"a","command":"c"}]}"#);
        assert_eq!(t.cwd, root);
    }

    #[test]
    fn env_table() {
        // (raw, 期待する env)
        let cases: [(&str, Vec<(&str, &str)>); 5] = [
            // キー昇順で決定的に並ぶ (入力順に依存しない)
            (
                r#"{"ZZZ":"3","AAA":"1","MMM":"2"}"#,
                vec![("AAA", "1"), ("MMM", "2"), ("ZZZ", "3")],
            ),
            // 非文字列の値はその項目だけ捨てる
            (
                r#"{"B":2,"A":"1","C":null,"D":"4"}"#,
                vec![("A", "1"), ("D", "4")],
            ),
            // 非オブジェクトは無視
            (r#""FOO=1""#, vec![]),
            ("null", vec![]),
            ("{}", vec![]),
        ];
        for (raw, want) in cases {
            let t = one(&format!(
                r#"{{"tasks":[{{"label":"a","command":"c","options":{{"env":{raw}}}}}]}}"#
            ));
            let got: Vec<(&str, &str)> = t
                .env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            assert_eq!(got, want, "env={raw}");
        }
    }

    // ---- 変数 ----

    #[test]
    fn workspace_folder_is_expanded_at_parse_time() {
        let root = t_root();
        let rs = root.to_string_lossy().to_string();
        let t = one(
            r#"{"tasks":[{"label":"a","command":"${workspaceFolder}/build.sh","args":["--out","${workspaceFolder}/out"],"options":{"env":{"ROOT":"${workspaceFolder}"}}}]}"#,
        );
        assert_eq!(t.command, format!("{rs}/build.sh"));
        assert_eq!(t.args, vec!["--out".to_string(), format!("{rs}/out")]);
        assert_eq!(t.env, vec![("ROOT".to_string(), rs)]);
        assert!(!t.needs_file);
        assert_eq!(t.blocked, None);
    }

    #[test]
    fn needs_file_table() {
        // (raw タスク本体, needs_file)
        let cases = [
            (r#""label":"a","command":"cat ${file}""#, true),
            (
                r#""label":"a","command":"c","args":["${fileBasename}"]"#,
                true,
            ),
            (
                r#""label":"a","command":"c","options":{"cwd":"${fileDirname}"}"#,
                true,
            ),
            (
                r#""label":"a","command":"c","options":{"env":{"F":"${file}"}}"#,
                true,
            ),
            (r#""label":"a","command":"c","args":["plain"]"#, false),
            // 閉じていない `${` は変数ではない
            (r#""label":"a","command":"echo ${file""#, false),
        ];
        for (body, want) in cases {
            let t = one(&format!(r#"{{"tasks":[{{{body}}}]}}"#));
            assert_eq!(t.needs_file, want, "body={body}");
            assert_eq!(t.blocked, None, "body={body}");
        }
    }

    #[test]
    fn unsupported_vars_block_but_keep_task() {
        // (raw タスク本体, blocked に含まれるべき文字列)
        let cases = [
            (r#""label":"a","command":"echo ${env:FOO}""#, "${env:FOO}"),
            (
                r#""label":"a","command":"c","args":["${config:editor.x}"]"#,
                "${config:editor.x}",
            ),
            (
                r#""label":"a","command":"c","options":{"cwd":"${cwd}"}"#,
                "${cwd}",
            ),
            (
                r#""label":"a","command":"c","options":{"env":{"K":"${lineNumber}"}}"#,
                "${lineNumber}",
            ),
        ];
        for (body, want) in cases {
            let t = one(&format!(r#"{{"tasks":[{{{body}}}]}}"#));
            let why = t.blocked.as_deref().unwrap_or("");
            assert!(why.contains(want), "body={body} blocked={why:?}");
            assert!(why.starts_with("未対応の変数"), "blocked={why:?}");
        }
        // 同じ変数が複数回出ても 1 回だけ挙げる
        let t = one(r#"{"tasks":[{"label":"a","command":"${x} ${x} ${y}"}]}"#);
        assert_eq!(
            t.blocked.as_deref(),
            Some("未対応の変数 ${x} ${y} を含みます")
        );
    }

    // ---- 実行行 ----

    #[test]
    fn command_line_table() {
        // (名前, ty, command, args, windows, 期待)
        let cases: [(&str, TaskType, &str, &[&str], bool, &str); 10] = [
            (
                "shell 引数なし",
                TaskType::Shell,
                "echo hi",
                &[],
                false,
                "echo hi",
            ),
            (
                "shell 引数なし win",
                TaskType::Shell,
                "echo hi",
                &[],
                true,
                "echo hi",
            ),
            (
                "shell + args posix",
                TaskType::Shell,
                "npm run build",
                &["--", "-v"],
                false,
                "npm run build '--' '-v'",
            ),
            (
                "shell + args win",
                TaskType::Shell,
                "npm run build",
                &["--", "-v"],
                true,
                r#"npm run build "--" "-v""#,
            ),
            (
                "process posix",
                TaskType::Process,
                "cargo",
                &["test"],
                false,
                "'cargo' 'test'",
            ),
            (
                "process win",
                TaskType::Process,
                "cargo",
                &["test"],
                true,
                r#""cargo" "test""#,
            ),
            (
                "process 引数なし posix",
                TaskType::Process,
                "my prog",
                &[],
                false,
                "'my prog'",
            ),
            (
                "process 引数なし win",
                TaskType::Process,
                "my prog",
                &[],
                true,
                r#""my prog""#,
            ),
            (
                "posix のシングルクォート",
                TaskType::Process,
                "sh",
                &["it's"],
                false,
                r#"'sh' 'it'\''s'"#,
            ),
            (
                "cmd のダブルクォート",
                TaskType::Process,
                "sh",
                &[r#"say "hi""#],
                true,
                r#""sh" "say ""hi""""#,
            ),
        ];
        for (name, ty, command, args, windows, want) in cases {
            let t = TaskDef {
                label: "t".to_string(),
                ty,
                command: command.to_string(),
                args: args.iter().map(|s| s.to_string()).collect(),
                cwd: t_root(),
                env: Vec::new(),
                group: TaskGroup::None,
                is_default: false,
                reveal: true,
                needs_file: false,
                blocked: None,
            };
            assert_eq!(command_line(&t, windows), want, "[{name}]");
        }
    }

    #[test]
    fn resolve_table() {
        let root = t_root();
        let file = root.join("src").join("main.rs");
        let dir = root.join("src");
        let (fs_, ds) = (file.to_string_lossy(), dir.to_string_lossy());

        // 成功: ${file} 系が展開され、引用は展開後に掛かる
        let t = one(
            r#"{"tasks":[{"label":"a","type":"process","command":"cat","args":["${file}","${fileBasename}","${fileDirname}"]}]}"#,
        );
        assert!(t.needs_file);
        assert_eq!(
            resolve(&t, Some(&file), false),
            Ok(format!("'cat' '{fs_}' 'main.rs' '{ds}'"))
        );
        assert_eq!(
            resolve(&t, Some(&file), true),
            Ok(format!(r#""cat" "{fs_}" "main.rs" "{ds}""#))
        );

        // 成功: needs_file でないタスクは file が None でも通る
        let plain = one(r#"{"tasks":[{"label":"a","command":"echo hi"}]}"#);
        assert_eq!(resolve(&plain, None, false), Ok("echo hi".to_string()));
        assert_eq!(resolve(&plain, None, true), Ok("echo hi".to_string()));

        // 失敗 1: blocked (未対応変数)
        let blocked = one(r#"{"tasks":[{"label":"a","command":"echo ${env:FOO}"}]}"#);
        let err = resolve(&blocked, Some(&file), false).expect_err("blocked なら Err");
        assert!(err.contains("${env:FOO}"), "err={err:?}");

        // 失敗 1b: blocked (command 欠落) は file があっても Err
        let nocmd = one(r#"{"tasks":[{"label":"a"}]}"#);
        assert_eq!(
            resolve(&nocmd, Some(&file), false),
            Err("command がありません".to_string())
        );

        // 失敗 2: needs_file なのに file が None
        let err = resolve(&t, None, false).expect_err("file なしなら Err");
        assert!(err.contains("${file}"), "err={err:?}");
        assert!(resolve(&t, None, true).is_err());

        // Shell 型では command の生テキスト内も展開される
        let sh = one(r#"{"tasks":[{"label":"a","command":"wc -l ${file}"}]}"#);
        assert_eq!(resolve(&sh, Some(&file), false), Ok(format!("wc -l {fs_}")));
    }

    // ---- default_build ----

    #[test]
    fn default_build_prefers_is_default() {
        let doc = parse(
            r#"{"tasks":[
                {"label":"t","command":"c","group":"test"},
                {"label":"b1","command":"c","group":"build"},
                {"label":"b2","command":"c","group":{"kind":"build","isDefault":true}},
                {"label":"b3","command":"c","group":"build"}
            ]}"#,
        );
        assert_eq!(doc.tasks.len(), 4);
        assert_eq!(doc.default_build().map(|t| t.label.as_str()), Some("b2"));

        // isDefault が無ければ最初の Build
        let doc = parse(
            r#"{"tasks":[
                {"label":"t","command":"c","group":"test"},
                {"label":"b1","command":"c","group":"build"},
                {"label":"b3","command":"c","group":"build"}
            ]}"#,
        );
        assert_eq!(doc.default_build().map(|t| t.label.as_str()), Some("b1"));

        // Build が 1 つも無ければ None
        let doc = parse(r#"{"tasks":[{"label":"t","command":"c","group":"test"}]}"#);
        assert!(doc.default_build().is_none());
        assert!(TasksDoc::default().default_build().is_none());
        assert!(TasksDoc::default().tasks.is_empty());
    }

    // ---- ファイル往復 ----

    #[test]
    fn tasks_json_path_is_under_dot_vscode() {
        let root = t_root();
        assert_eq!(
            tasks_json_path(&root),
            root.join(".vscode").join("tasks.json")
        );
    }

    #[test]
    fn load_tasks_roundtrip() {
        let root = crate::test_util::unique_temp_dir("zaivern-tasks-test", "load");

        // ファイルが無いときは既定値 (エラーにしない)
        let doc = load_tasks(&root);
        assert!(doc.tasks.is_empty() && doc.error.is_none(), "{doc:?}");

        let path = tasks_json_path(&root);
        std::fs::create_dir_all(path.parent().expect("親")).expect("mkdir .vscode");
        std::fs::write(
            &path,
            "{\n  // ビルド定義\n  \"version\": \"2.0.0\",\n  \"tasks\": [\n    {\n      \"label\": \"build\",\n      \"type\": \"shell\",\n      \"command\": \"cargo build\",\n      \"args\": [\"--locked\"],\n      \"group\": { \"kind\": \"build\", \"isDefault\": true },\n      \"options\": { \"cwd\": \"${workspaceFolder}\" },\n    },\n  ],\n}\n",
        )
        .expect("write tasks.json");

        let doc = load_tasks(&root);
        assert_eq!(doc.error, None, "{doc:?}");
        assert_eq!(doc.tasks.len(), 1);
        let t = doc.default_build().expect("既定のビルドタスク");
        assert_eq!(t.label, "build");
        assert_eq!(t.command, "cargo build");
        assert_eq!(t.args, vec!["--locked".to_string()]);
        assert_eq!(t.cwd, root);
        assert_eq!(command_line(t, false), "cargo build '--locked'");
        assert_eq!(command_line(t, true), r#"cargo build "--locked""#);

        std::fs::remove_dir_all(&root).expect("後片付け");
    }

    /// ディスクの `tasks.json` → 一覧に出る形 → **実際に走るコマンド行** まで
    /// 1 本で確かめる (この OS のシェルで本当に通る引用かを固定する)。
    ///
    /// 走らせるのは `echo` だけ。`sleep` を書かないのはプロセス残留を作らない
    /// ため (CLAUDE.md「PTY テストのシェルスクリプトに長い sleep を書かない」)。
    #[test]
    fn echo_task_from_disk_runs_on_this_platform() {
        let root = crate::test_util::unique_temp_dir("zaivern-tasks-test", "echo");
        let path = tasks_json_path(&root);
        std::fs::create_dir_all(path.parent().expect("親")).expect("mkdir .vscode");
        std::fs::write(
            &path,
            concat!(
                "{\n",
                "  \"version\": \"2.0.0\",\n",
                "  \"tasks\": [\n",
                "    // 無害な確認用タスク\n",
                "    {\n",
                "      \"label\": \"say hello\",\n",
                "      \"type\": \"shell\",\n",
                "      \"command\": \"echo\",\n",
                "      \"args\": [\"zaivern hello\"],\n",
                "      \"options\": { \"cwd\": \"${workspaceFolder}\" }\n",
                "    }\n",
                "  ]\n",
                "}\n"
            ),
        )
        .expect("write tasks.json");

        // 1. 一覧に出る (ラベル・実行可能・作業フォルダ)
        let doc = load_tasks(&root);
        assert_eq!(doc.error, None, "{doc:?}");
        let t = doc.tasks.first().expect("タスクが 1 件");
        assert_eq!(t.label, "say hello");
        assert_eq!(t.blocked, None, "実行可能として並ぶ");
        assert_eq!(t.cwd, root);

        // 2. その行を実際のシェルへ渡すと通る
        let line = resolve(t, None, cfg!(windows)).expect("実行行が組める");
        let out = if cfg!(windows) {
            std::process::Command::new(
                std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
            )
            .args(["/C", &line])
            .current_dir(&t.cwd)
            .output()
        } else {
            std::process::Command::new("/bin/sh")
                .args(["-c", &line])
                .current_dir(&t.cwd)
                .output()
        }
        .expect("シェルが起動する");
        assert!(out.status.success(), "line={line} out={out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("zaivern hello"),
            "line={line} stdout={stdout}"
        );

        std::fs::remove_dir_all(&root).expect("後片付け");
    }
}
