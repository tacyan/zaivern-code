//! 実測の番人 — **実ファイルを本当に書き換えて、本物の git で測る。**
//!
//! ソースの文字列を読むだけの検査では、この層は守れない。「測っている
//! つもりで測れていない」が、まさにここで起きた不具合の形だから。

use std::path::{Path, PathBuf};

use super::changeset::*;

fn lab(name: &str) -> PathBuf {
    crate::test_util::unique_temp_dir("zaivern-team-changeset", name)
}

/// git を起こす (シェル無し)。**cwd を継承させない** — 継承すると、
/// 実験場が git リポジトリでないときに本体のリポジトリを触る。
fn git(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// git リポジトリの実験場を作って、最初のコミットまで済ませる。
fn repo(name: &str) -> Option<PathBuf> {
    let d = lab(name);
    std::fs::create_dir_all(&d).ok()?;
    if !git(&d, &["init", "-q"]) {
        std::fs::remove_dir_all(&d).ok();
        return None;
    }
    // **利用者の設定に頼らない。** CI の runner には user.email が無い。
    git(&d, &["config", "user.email", "t@example.invalid"]);
    git(&d, &["config", "user.name", "t"]);
    git(&d, &["config", "commit.gpgsign", "false"]);
    std::fs::write(d.join("a.rs"), "fn a() {}\n").ok()?;
    std::fs::write(d.join("b.rs"), "fn b() {}\n").ok()?;
    std::fs::create_dir_all(d.join("src")).ok()?;
    std::fs::write(d.join("src").join("keep.rs"), "fn keep() {}\n").ok()?;
    if !git(&d, &["add", "-A"]) || !git(&d, &["commit", "-q", "-m", "init"]) {
        std::fs::remove_dir_all(&d).ok();
        return None;
    }
    Some(d)
}

/// git がこの環境に無ければ降りる (`[skip]` として理由を出す)。
macro_rules! need_repo {
    ($name:literal) => {
        match repo($name) {
            Some(d) => d,
            None => {
                eprintln!("[skip] {} — git を使えません", $name);
                return;
            }
        }
    };
}

fn paths(v: &[MeasuredChange]) -> Vec<&str> {
    v.iter().map(|c| c.path.as_str()).collect()
}

#[test]
fn 新規と変更と削除を見分ける() {
    let d = need_repo!("kinds");
    let base = capture_baseline(&d).expect("基準点");
    assert!(base.usable());
    assert!(base.entries.is_empty(), "汚れていないのに拾った: {base:?}");

    std::fs::write(d.join("a.rs"), "fn a() { changed(); }\n").unwrap();
    std::fs::write(d.join("new.rs"), "fn n() {}\n").unwrap();
    std::fs::remove_file(d.join("b.rs")).unwrap();

    let got = measure(&d, &base).expect("実測");
    let mut m: Vec<(&str, ChangeKind)> = got.iter().map(|c| (c.path.as_str(), c.kind)).collect();
    m.sort();
    assert_eq!(
        m,
        vec![
            ("a.rs", ChangeKind::Modified),
            ("b.rs", ChangeKind::Deleted),
            ("new.rs", ChangeKind::Added),
        ]
    );
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn renameは消えたと増えたの二件になる() {
    // **片方だけが担当範囲、を見逃さない。** 1 件に畳むと、行き先が
    // 担当外でも「元は担当内だった」で通ってしまう。
    let d = need_repo!("rename");
    let base = capture_baseline(&d).expect("基準点");
    std::fs::rename(d.join("a.rs"), d.join("moved.rs")).unwrap();
    let got = measure(&d, &base).expect("実測");
    let mut p = paths(&got);
    p.sort();
    assert_eq!(p, vec!["a.rs", "moved.rs"]);
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn 基準点の時点で汚れていたファイルの再変更も見える() {
    // **状態文字 (`M`) だけでは足りない。** 基準点でもう `M` だった
    // ファイルをもう一度書き換えても `M` のままなので、内容を見ないと
    // 変更が 1 バイトも見えない。
    let d = need_repo!("already-dirty");
    std::fs::write(d.join("a.rs"), "fn a() { first(); }\n").unwrap();
    let base = capture_baseline(&d).expect("基準点");
    assert_eq!(base.entries.len(), 1, "汚れを拾えていない: {base:?}");

    std::fs::write(d.join("a.rs"), "fn a() { second(); }\n").unwrap();
    let got = measure(&d, &base).expect("実測");
    assert_eq!(paths(&got), vec!["a.rs"], "再変更を見落とした");

    // 元へ戻したら「変わっていない」に戻る (差分が消えたことも見える)。
    std::fs::write(d.join("a.rs"), "fn a() { first(); }\n").unwrap();
    assert!(measure(&d, &base).expect("実測").is_empty());
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn 汚れが消えたことも変更として見える() {
    // 基準点では汚れていたものを HEAD の内容へ戻す = そのタスクが
    // **元へ戻した**という変更。見落とすと「何もしていない」になる。
    let d = need_repo!("reverted");
    std::fs::write(d.join("a.rs"), "fn a() { dirty(); }\n").unwrap();
    let base = capture_baseline(&d).expect("基準点");
    std::fs::write(d.join("a.rs"), "fn a() {}\n").unwrap();
    let got = measure(&d, &base).expect("実測");
    assert_eq!(paths(&got), vec!["a.rs"], "戻したことを見落とした: {got:?}");
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn git管理外は測れないと言う() {
    // **保証を偽らない。** 空の結果を返すと「何も変えていない」に見える。
    let d = lab("no-git");
    std::fs::create_dir_all(&d).unwrap();
    assert_eq!(capture_baseline(&d), Err(MeasureError::NotGitRepo));
    let base = FileBaseline::unavailable("git 管理下ではありません");
    assert!(!base.usable());
    assert!(matches!(
        measure(&d, &base),
        Err(MeasureError::NoBaseline(_))
    ));
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn gitが失敗したら安全側へ倒れる() {
    // `.git` を壊す = git は動くがリポジトリとして読めない。
    let d = need_repo!("broken-git");
    std::fs::write(d.join(".git").join("HEAD"), "not a ref\n").unwrap();
    let got = capture_baseline(&d);
    assert!(
        got.is_err(),
        "git が失敗したのに基準点が取れたことにした: {got:?}"
    );
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn 基準点が無ければ実測できないと言う() {
    let d = need_repo!("no-baseline");
    let empty = FileBaseline::default();
    assert!(!empty.usable(), "既定値を「測れる基準点」にしてはいけない");
    assert!(matches!(
        measure(&d, &empty),
        Err(MeasureError::NoBaseline(_))
    ));
    std::fs::remove_dir_all(&d).ok();
}

// ── ワークスペース境界 ───────────────────────────────────────────────

#[test]
fn 外を指すパスを内側と認めない() {
    let d = need_repo!("escape");
    assert!(inside_workspace(&d, "a.rs"));
    assert!(inside_workspace(&d, "src/keep.rs"));
    assert!(inside_workspace(&d, "src/not-yet-created.rs"), "これから作る");
    assert!(!inside_workspace(&d, "../outside.rs"));
    assert!(!inside_workspace(&d, "src/../../outside.rs"));
    assert!(!inside_workspace(&d, "/etc/passwd"));
    assert!(!inside_workspace(&d, ""));
    std::fs::remove_dir_all(&d).ok();
}

#[cfg(unix)]
#[test]
fn symlink越しの外は内側と認めない() {
    // **形だけの検査では足りない。** `link/x` は形の上では相対パスだが、
    // `link` が外を指していれば実体は外にある。
    let d = need_repo!("symlink-escape");
    let outside = lab("symlink-outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "s\n").unwrap();
    std::os::unix::fs::symlink(&outside, d.join("link")).unwrap();

    assert!(
        !inside_workspace(&d, "link/secret.txt"),
        "symlink の先 (workspace の外) を内側と認めた"
    );
    assert!(
        !inside_workspace(&d, "link/not-yet.txt"),
        "まだ無いファイルでも、親が外を指していれば外"
    );
    // リンクそのものは内側 (作業ツリーの中のファイルなので)。
    assert!(inside_workspace(&d, "link"));
    std::fs::remove_dir_all(&d).ok();
    std::fs::remove_dir_all(&outside).ok();
}

#[cfg(unix)]
#[test]
fn symlinkの向き先が変わったことを見る() {
    // リンクを辿って中身をハッシュすると、**向き先を差し替えた**変更が
    // 見えない (先の内容が同じなら同じハッシュになる)。
    let d = need_repo!("symlink-retarget");
    std::os::unix::fs::symlink("a.rs", d.join("link")).unwrap();
    let base = capture_baseline(&d).expect("基準点");
    std::fs::remove_file(d.join("link")).unwrap();
    std::os::unix::fs::symlink("b.rs", d.join("link")).unwrap();
    let got = measure(&d, &base).expect("実測");
    assert!(
        paths(&got).contains(&"link"),
        "リンクの向き先の差し替えを見落とした: {got:?}"
    );
    std::fs::remove_dir_all(&d).ok();
}

// ── 帰属 (並列タスクの切り分け) ──────────────────────────────────────

#[test]
fn 他のタスクが握っている範囲は自分の成果にしない() {
    // **複数のエージェントが同じワークスペースで同時に働く**前提なので、
    // 「作業ツリーと HEAD の差分」をそのまま自分の成果にはできない。
    let measured = vec![
        "src/auth/login.rs".to_string(),
        "src/billing/plan.rs".to_string(),
    ];
    let mine = vec!["src/auth/".to_string()];
    let others = vec!["src/billing/".to_string()];
    let (ours, bad) = attribute(&measured, &mine, &others);
    assert_eq!(ours, vec![&measured[0]], "自分の担当を取りこぼした");
    assert!(bad.is_empty(), "隣のタスクの変更を咎めた: {bad:?}");
}

#[test]
fn 誰も握っていない変更は担当外として扱う() {
    // **自分ではないと言い切れない**ので安全側へ倒す。倒さないと
    // 「誰の範囲でもない場所を書き換えたら素通り」になる。
    let measured = vec!["docs/secret.md".to_string()];
    let mine = vec!["src/auth/".to_string()];
    let others = vec!["src/billing/".to_string()];
    let (ours, bad) = attribute(&measured, &mine, &others);
    assert!(ours.is_empty());
    assert_eq!(bad, vec![&measured[0]], "誰の範囲でもない変更を見逃した");
}

#[test]
fn 帰属の判定は既存のリースの重なり判定を使う() {
    // ディレクトリ指定が配下を覆うこと。**第 2 の競合判定を作らない**
    // ので、ここが `lease::overlaps` と食い違うことはない。
    let measured = vec!["src/auth/login.rs".to_string()];
    let (ours, bad) = attribute(&measured, &["src/auth/".to_string()], &[]);
    assert_eq!(ours.len(), 1, "lease と違う判定になった");
    assert!(bad.is_empty());
    // **末尾の `/` が「配下ぜんぶ」**というのが lease の意味論。
    // 付いていないものは、そのパスちょうどしか指さない。
    assert!(crate::lease::overlaps("src/auth/", "src/auth/login.rs"));
    let (_, strict) = attribute(&measured, &["src/auth".to_string()], &[]);
    assert_eq!(strict.len(), 1, "lease と違う意味論を持ち込んだ");
}

#[test]
fn 測れなかった理由はそのまま人へ出せる() {
    // **一部だけ持って黙る、をしない。** 一部だけ持つと
    // 「担当内しか触っていない」という嘘が台帳に残る。
    for e in [
        MeasureError::TooMany(MAX_TRACKED_PATHS + 1),
        MeasureError::TooLarge,
        MeasureError::NotGitRepo,
        MeasureError::GitFailed("fatal: not a repository".into()),
        MeasureError::NoBaseline(String::new()),
        MeasureError::Escapes("../outside.rs".into()),
    ] {
        let d = e.detail();
        assert!(!d.is_empty(), "{e:?} の理由が空");
        assert!(
            d.contains("測れ") || d.contains("実測") || d.contains("外"),
            "{e:?} → {d} (何が起きたのか伝わらない)"
        );
    }
}


/// 8MB を超える中身を作る (種で内容が変わる)。
///
/// **大きさは意図的に「1 ファイルを丸ごと読む上限」の外側**に取ってある。
/// ここが上限の内側だと、大きいファイルを長さだけで見ていた不具合を
/// この番人は 1 度も通らない。
fn big(seed: u8) -> Vec<u8> {
    const N: usize = 9 * 1024 * 1024;
    let mut v = vec![0u8; N];
    for (i, b) in v.iter_mut().enumerate() {
        *b = ((i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed as u32) >> 13) as u8;
    }
    v
}

#[test]
fn 大きなファイルの新規と変更も内容で見える() {
    // 基準点では綺麗 → その後に汚れる、という素直な経路。
    let d = need_repo!("big-added");
    let base = capture_baseline(&d).expect("基準点");
    assert!(base.entries.is_empty());

    std::fs::write(d.join("big-new.bin"), big(1)).unwrap();
    std::fs::write(d.join("a.rs"), big(2)).unwrap();

    let got = measure(&d, &base).expect("実測");
    let mut m: Vec<(&str, ChangeKind)> = got.iter().map(|c| (c.path.as_str(), c.kind)).collect();
    m.sort();
    assert_eq!(
        m,
        vec![
            ("a.rs", ChangeKind::Modified),
            ("big-new.bin", ChangeKind::Added),
        ]
    );
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn 基準点で汚れていた大きなファイルの同サイズ書き換えも見える() {
    // **この版で直した不具合そのもの。** 大きいものを「長さだけ」で見ると、
    // 同じ長さの別の内容へ書き換えた変更が指紋として等しくなり、
    // `git status` の状態文字も `??` のまま動かないので、**変更が
    // 1 バイトも見えない**。担当外を書き換えたタスクが素通りする形。
    let d = need_repo!("big-already-dirty");
    let first = big(3);
    let second = big(4);
    assert_eq!(first.len(), second.len(), "同じ長さで比べないと意味が無い");
    assert_ne!(first, second);

    // **追跡済みのファイル**を汚しておく (`git status` は最初から最後まで
    // `M` のまま。状態文字は 1 文字も動かない)。
    std::fs::write(d.join("a.rs"), &first).unwrap();
    let base = capture_baseline(&d).expect("基準点");
    assert_eq!(base.entries.len(), 1, "汚れを拾えていない: {base:?}");

    std::fs::write(d.join("a.rs"), &second).unwrap();
    let got = measure(&d, &base).expect("実測");
    assert_eq!(paths(&got), vec!["a.rs"], "同サイズの書き換えを見落とした");
    assert_eq!(got[0].kind, ChangeKind::Modified);
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn 大きなファイルを元へ戻したら変更なしに戻る() {
    // 逆向きも見る。**変更ありへ倒しておけば安全**ではない —
    // いつでも「変わった」と言う実測は、担当範囲の照合を無意味にする。
    let d = need_repo!("big-restored");
    let original = big(5);
    std::fs::write(d.join("big.bin"), &original).unwrap();
    assert!(git(&d, &["add", "-A"]) && git(&d, &["commit", "-q", "-m", "big"]));

    let base = capture_baseline(&d).expect("基準点");
    assert!(base.entries.is_empty(), "commit 済みなのに汚れている: {base:?}");

    std::fs::write(d.join("big.bin"), big(6)).unwrap();
    assert_eq!(
        paths(&measure(&d, &base).expect("実測")),
        vec!["big.bin"],
        "汚した時点で見えていない"
    );

    std::fs::write(d.join("big.bin"), &original).unwrap();
    assert!(
        measure(&d, &base).expect("実測").is_empty(),
        "元へ戻したのに変更として残った"
    );
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn 読む予算を超えたら黙って途中までにせず測れないと言う() {
    // 途中まで読んだ指紋を返すと、**先頭が同じ別の内容**が同じものになる。
    // 予算切れは `TooLarge` (fail-closed) であって、部分一致ではない。
    let d = lab("hash-budget");
    std::fs::create_dir_all(&d).unwrap();
    let f = d.join("x.bin");
    std::fs::write(&f, vec![7u8; 4096]).unwrap();

    // 予算が足りるとき: 中身のハッシュが返る。
    let mut budget: u64 = 4096;
    let got = fingerprint(&f, &mut budget).expect("測れるはず").expect("指紋");
    assert_eq!(got.len, 4096);
    assert_eq!(budget, 0, "読んだぶんだけ引くこと");

    // 1 バイト足りないとき: 途中までの指紋を返さない。
    let mut budget: u64 = 4095;
    assert_eq!(
        fingerprint(&f, &mut budget),
        Err(MeasureError::TooLarge),
        "予算切れを黙って部分ハッシュにした"
    );

    // 予算は複数ファイルで共有する — 2 つ目で尽きても同じ。
    let g = d.join("y.bin");
    std::fs::write(&g, vec![8u8; 4096]).unwrap();
    let mut budget: u64 = 5000;
    assert!(fingerprint(&f, &mut budget).is_ok());
    assert_eq!(fingerprint(&g, &mut budget), Err(MeasureError::TooLarge));
    std::fs::remove_dir_all(&d).ok();
}
