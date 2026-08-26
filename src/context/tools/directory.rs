//! ディレクトリの地図。`token-slim-mcp` の `dir_map` にあたる。
//!
//! `ls -R` / `find` の代わり。1 件 1 行・大きさつき・成果物の置き場は
//! 降りない・件数の上限つき。**上限に当たったら必ずそう書く**。

use std::path::{Path, PathBuf};

use super::{Rendered, ToolContext};
use crate::context::walk::should_skip_dir;
use crate::context::ContextError;

/// 地図の指定。
#[derive(Clone, Copy, Debug, Default)]
pub struct MapParams {
    /// 何段まで降りるか。`None` なら [`crate::context::ContextLimits::dir_depth`]。
    pub depth: Option<usize>,
    /// 出す件数の上限。`None` なら [`crate::context::ContextLimits::dir_max_entries`]。
    pub max_entries: Option<usize>,
}

/// 地図を作る。
pub fn run(cx: &ToolContext, root: &Path, params: MapParams) -> Result<Rendered, ContextError> {
    let start = cx.workspace.resolve(root)?;
    if !start.as_path().is_dir() {
        return Err(ContextError::BadRequest(format!(
            "{}: not a directory",
            start.rel()
        )));
    }
    let depth = params.depth.unwrap_or(cx.limits.dir_depth).max(1);
    let max_entries = params
        .max_entries
        .unwrap_or(cx.limits.dir_max_entries)
        .max(1);

    let mut lines: Vec<String> = Vec::new();
    let mut count = 0usize;
    let mut capped = false;
    walk_map(
        start.as_path(),
        0,
        depth,
        max_entries,
        &mut lines,
        &mut count,
        &mut capped,
    );
    Ok(Rendered {
        detail: format!(
            "{} depth<={depth} entries={count}{}",
            start.rel(),
            if capped {
                format!(" [capped at {max_entries}]")
            } else {
                String::new()
            }
        ),
        body: lines.join("\n"),
        // 素直に `ls -R` した場合との比較はできないので、削減 0 として扱う
        // (測っていない削減を主張しない)。
        original_tokens: crate::context::metrics::estimate_tokens(&lines.join("\n")),
        hint: String::new(),
    })
}

fn walk_map(
    dir: &Path,
    level: usize,
    max_depth: usize,
    max_entries: usize,
    lines: &mut Vec<String>,
    count: &mut usize,
    capped: &mut bool,
) {
    if level >= max_depth || *count >= max_entries {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    let mut files: Vec<(String, u64)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(ft) = entry.file_type() else { continue };
        // symlink は辿らない (根の外へ出る最短経路)
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if !should_skip_dir(&name) {
                dirs.push((name, entry.path()));
            }
        } else if ft.is_file() {
            files.push((name, entry.metadata().map(|m| m.len()).unwrap_or(0)));
        }
    }
    // read_dir の順序は OS と FS で変わるので必ず揃える
    dirs.sort();
    files.sort();
    let indent = "  ".repeat(level);
    for (name, path) in &dirs {
        if *count >= max_entries {
            lines.push(format!("{indent}… [entry cap reached]"));
            *capped = true;
            return;
        }
        lines.push(format!("{indent}{name}/"));
        *count += 1;
        walk_map(
            path,
            level + 1,
            max_depth,
            max_entries,
            lines,
            count,
            capped,
        );
    }
    for (name, size) in &files {
        if *count >= max_entries {
            lines.push(format!("{indent}… [entry cap reached]"));
            *capped = true;
            return;
        }
        lines.push(format!("{indent}{name} {}", human_size(*size)));
        *count += 1;
    }
}

/// 人が読む大きさ。
fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::tests_support::Lab;

    #[test]
    fn 地図は深さと並びを守る() {
        let lab = Lab::new("dirmap");
        lab.write("src/a.rs", "x");
        lab.write("src/deep/b.rs", "y");
        lab.write("z.txt", "hello");
        lab.write("target/junk.o", "junk");
        let r = lab.map(MapParams {
            depth: Some(2),
            max_entries: None,
        });
        let body = r.body.clone();
        assert!(body.contains("src/"), "{body}");
        assert!(body.contains("  a.rs 1B"), "{body}");
        assert!(body.contains("  deep/"), "{body}");
        assert!(!body.contains("b.rs"), "深さ 2 を超えて降りた: {body}");
        assert!(!body.contains("target"), "成果物の置き場へ降りた: {body}");
        assert!(body.contains("z.txt 5B"), "{body}");
        // ディレクトリが先、ファイルが後
        assert!(body.find("src/").unwrap() < body.find("z.txt").unwrap());
    }

    #[test]
    fn 件数の上限に当たったら必ず書く() {
        let lab = Lab::new("dirmap-cap");
        for i in 0..20 {
            lab.write(&format!("f{i:02}.txt"), "x");
        }
        let r = lab.map(MapParams {
            depth: None,
            max_entries: Some(5),
        });
        assert!(r.body.contains("entry cap reached"), "{}", r.body);
        assert!(r.detail.contains("[capped at 5]"), "{}", r.detail);
    }

    #[test]
    fn ファイルを指すと断る() {
        let lab = Lab::new("dirmap-file");
        lab.write("a.rs", "x");
        assert!(matches!(
            lab.map_result(Path::new("a.rs"), MapParams::default()),
            Err(ContextError::BadRequest(_))
        ));
    }

    #[test]
    fn 大きさの表記() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(1023), "1023B");
        assert_eq!(human_size(1024), "1.0KB");
        assert_eq!(human_size(1024 * 1024 * 3 / 2), "1.5MB");
    }
}
