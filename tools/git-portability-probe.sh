#!/usr/bin/env sh
# 「競合ゼロ」が寄りかかっている **git と OS の挙動**を、その場で実測する。
#
# ## なぜ要ったのか
#
# `docs/conflict-zero.md` / `docs/anyrepo-proof.md` / `docs/czero-repo-shapes.md`
# の数字は **macOS・git 2.47.1 の 1 点**でしか測っていなかった。
# だがこの機能が寄りかかっているのは言語機能ではなく、
#
#   * **git の版**    — `merge-tree --write-tree` (2.38+) / 既定戦略 ort (2.34+)
#   * **OS と FS**    — 大文字小文字を畳むか / 改行の既定 (`core.autocrlf`)
#
# であり、どちらも 1 点の測定では 1 バイトも保証されない。CLAUDE.md の
# 「macOS だけで開発していると Linux / Windows でしか出ない不具合が素通りする」
# の直撃領域である。
#
# ## `tools/merge-band-probe.sh` との違い
#
# あちらは**帯の値 (`SAFE_BAND` / `MERGE_ONLY_BAND`) そのもの**を反証しに行く。
# こちらは帯より手前の**前提**を測る。両方を同じ土俵で回すのが
# `tools/git-matrix-prove.sh` (Linux の複数 git 版) と CI の `xplat-probe`。
#
# ## 測るもの
#
#   E   環境の事実         OS / git の版 / 実効 core.autocrlf / FS の大文字小文字 / 既定のマージ戦略
#   G1  merge-tree         `git merge-tree --write-tree` が使えるか (src/conflict.rs の判定と同じ読み方)
#   G2  ort               `git merge` の既定戦略が ort か (2.34 未満は recursive = diff が myers)
#   G3  CRLF 保存          CRLF のファイルへ互いに素な 2 つの編集 → マージが通り、**改行が壊れない**か
#   G4  大文字小文字の畳み  `Foo.txt` と `foo.txt` が**同じファイル**になる FS か
#
# ## 何を落とし、何を出すだけにするか
#
# CLAUDE.md の「絶対時間で線を引かない」と同じ理由で、**OS が違っても同じで
# なければおかしいもの**だけを落とす。時間は 1 つも測らない。
#
#   ✗ 落とす   G3 の「互いに素な編集がマージで衝突した」/「改行が書き換わった」
#              — 競合ゼロの中核の主張そのもの。OS で変わってはいけない
#   △ 出すだけ G1 / G2 / G4 / E — **git の版と FS で当然変わる**。ただし
#              変わった結果どの主張が効かなくなるかは表に必ず出す
#
# G4 は落とさないが、**畳む FS では行域の台帳キーが割れうる**ので警告を出す
# (`Foo.rs#L1-10` と `foo.rs#L1-10` は同じファイルなのに別の鍵になる)。
#
# ## 使い方
#
#   tools/git-portability-probe.sh            人が読む表
#   tools/git-portability-probe.sh --json     JSON は stdout、表は stderr
#
# 終了コード: 0 = 落とす指標が全部緑 / 1 = 1 つ以上赤 / 2 = 前提が足りない
#
# ## 触らないもの
#
#   * 本物の `~/.zaivern` / `~/.gitconfig` / システムの git 設定
#     (`HOME` と `GIT_CONFIG_GLOBAL` を一時ディレクトリへ向け、
#      `GIT_CONFIG_NOSYSTEM=1` を立てる)
#   * このリポジトリの `target/` (cargo を 1 度も呼ばない)
set -eu
# Windows (Git Bash / PowerShell) の既定コードページは UTF-8 ではないので、
# Python が日本語を stdout へ書いた瞬間に
# `UnicodeEncodeError: 'charmap' codec can't encode characters` で落ちる
# (CI の probe (windows-latest) が実際にこれで赤くなった)。
# **どの OS でも同じ出力になるよう UTF-8 を明示する。** 既に設定されていれば尊重する。
export PYTHONUTF8="${PYTHONUTF8:-1}"
export PYTHONIOENCODING="${PYTHONIOENCODING:-utf-8}"

json=0
for a in "$@"; do
    case "$a" in
    --json) json=1 ;;
    -h | --help)
        sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "不明な引数: $a" >&2
        echo "  tools/git-portability-probe.sh --help で使い方を出します" >&2
        exit 2
        ;;
    esac
done

if ! command -v git >/dev/null 2>&1; then
    echo "git がありません。この検査は素の git だけを測るので git が要ります。" >&2
    exit 2
fi

# Windows の git-bash には `python3` が無く `python` だけのことがある。
# **片方だけ書かない** (CLAUDE.md: OS 差は両側を実装する)。
py=
for cand in python3 python py; do
    if command -v "$cand" >/dev/null 2>&1; then
        # `py` は Windows のランチャ。-3 を付けないと 2 系を拾いうる
        if [ "$cand" = py ]; then
            "$cand" -3 -c 'import sys; sys.exit(0 if sys.version_info[0] == 3 else 1)' 2>/dev/null && py="$cand -3" && break
        else
            "$cand" -c 'import sys; sys.exit(0 if sys.version_info[0] == 3 else 1)' 2>/dev/null && py="$cand" && break
        fi
    fi
done
if [ -z "$py" ]; then
    echo "python3 がありません (python3 / python / py -3 のどれも 3 系ではない)。" >&2
    exit 2
fi

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

# shellcheck disable=SC2086
exec $py - "$json" "$root" <<'PY'
import json as J
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile

WANT_JSON = sys.argv[1] == "1"
ROOT = sys.argv[2]


def w(line=""):
    sys.stderr.write(line + "\n")


# ── `cfg!` の中身を**ソースから読む**。写経しない ──────────────────
#
# ここで `platform.system() in ("Windows", "Darwin")` と書き写すと、
# `src/lease.rs` 側が変わった日に**このプローブだけが黙って古くなる**
# (「実装したのに直っていない」の典型)。実際の式を読んで、想定と合わない
# 形になっていたら `unknown` にして正直に降りる。
#
# CLAUDE.md の「ソースを読む回帰テストは改行を正規化する」に従い、
# Windows のチェックアウト (CRLF) でも外れないようにしてから探す。
# 実物 (`src/lease.rs:240`) はこの形:
#     normalize_path_on(raw, true, cfg!(any(windows, target_os = "macos")))
# 位置引数なので `fold_case =` では引っ掛からない。式そのものを探す。
FOLD_CALL_RE = re.compile(
    r"normalize_path_on\s*\([^)]*?cfg!\s*\(\s*any\s*\(\s*windows\s*,\s*"
    r'target_os\s*=\s*"macos"\s*\)\s*\)',
    re.S,
)


def read_fold_cfg():
    """`src/lease.rs` の畳み判定が今も `windows || macos` かを見る。

    戻り値: True (その式のまま) / False (別の式に変わった) / None (読めない)
    """
    p = os.path.join(ROOT, "src", "lease.rs")
    try:
        with open(p, "r", encoding="utf-8", errors="replace") as fh:
            src = fh.read().replace("\r\n", "\n")
    except OSError:
        return None
    if FOLD_CALL_RE.search(src):
        return True
    # 呼び出し自体が消えていたら「読めない」、居るのに式が違うなら False
    return False if "normalize_path_on" in src else None


# ── git を呼ぶ。**環境を必ず隔離する** ───────────────────────────────
#
# 本物の `~/.gitconfig` とシステム設定を読ませない。読ませると
# 「手元に無い設定が CI にはある」(CLAUDE.md) をこちらで踏む —
# Windows の git インストーラは `core.autocrlf=true` を **global** へ置くので、
# 隔離しないと「Windows だけ結果が違う」の原因が設定なのか git の版なのか
# 永久に切り分かない。**素の既定**と**実環境の実効値**は別々に測る。
ENV_ISOLATED = None


def make_env(home):
    e = dict(os.environ)
    e["HOME"] = home
    e["USERPROFILE"] = home                      # Windows 側の HOME 相当
    e["GIT_CONFIG_GLOBAL"] = os.path.join(home, ".gitconfig")
    e["GIT_CONFIG_NOSYSTEM"] = "1"
    e["GIT_AUTHOR_NAME"] = e["GIT_COMMITTER_NAME"] = "probe"
    e["GIT_AUTHOR_EMAIL"] = e["GIT_COMMITTER_EMAIL"] = "probe@example.invalid"
    e["GIT_TERMINAL_PROMPT"] = "0"
    return e


def git(args, cwd, check=True, env=None):
    p = subprocess.run(["git"] + args, cwd=cwd, env=env or ENV_ISOLATED,
                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    out = p.stdout.decode("utf-8", "replace")
    if check and p.returncode != 0:
        raise RuntimeError("git %s -> rc=%d\n%s" % (" ".join(args), p.returncode, out))
    return p.returncode, out


def git_version():
    p = subprocess.run(["git", "--version"], stdout=subprocess.PIPE,
                       stderr=subprocess.STDOUT)
    s = p.stdout.decode("utf-8", "replace").strip()
    m = re.search(r"(\d+)\.(\d+)\.(\d+)", s)
    tup = tuple(int(x) for x in m.groups()) if m else (0, 0, 0)
    return s, tup


def init_repo(d):
    os.makedirs(d, exist_ok=True)
    # `-b main` は 2.28+。それ未満は落ちるので check=False にして
    # 既定ブランチ名のまま続ける (**測りたいのはブランチ名ではない**)。
    rc, _ = git(["init", "-q", "-b", "main", "."], d, check=False)
    if rc != 0:
        git(["init", "-q", "."], d)
    return d


def head_branch(d):
    rc, out = git(["rev-parse", "--abbrev-ref", "HEAD"], d, check=False)
    return out.strip() if rc == 0 else "master"


# ═════════════════════════════════════════════════════════════════════
#  E  環境の事実
# ═════════════════════════════════════════════════════════════════════

def probe_env(work):
    """実環境の**実効値**。隔離した既定ではなく、この機械で実際に効く値。"""
    facts = {}
    facts["platform"] = platform.system()
    facts["machine"] = platform.machine()
    facts["release"] = platform.release()
    facts["os_sep"] = os.sep
    facts["python"] = platform.python_version()

    ver_s, ver_t = git_version()
    facts["git"] = ver_s
    facts["git_tuple"] = list(ver_t)

    # **実環境の** core.autocrlf (隔離しない)。Windows の git インストーラは
    # これを global へ書くので、CI と手元で違う。スコープも一緒に出す
    # (CLAUDE.md: `git config` はスコープを混ぜる)。
    p = subprocess.run(["git", "config", "--show-scope", "--get-all", "core.autocrlf"],
                       cwd=work, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    raw = p.stdout.decode("utf-8", "replace").strip()
    facts["autocrlf_effective"] = raw if raw else "(未設定)"

    return facts


def probe_case_fold(work):
    """G4 この FS は大文字小文字を畳むか。**実際に作って開いて確かめる**。"""
    d = os.path.join(work, "casefold")
    os.makedirs(d, exist_ok=True)
    up = os.path.join(d, "Foo.txt")
    with open(up, "w", encoding="utf-8") as fh:
        fh.write("upper\n")
    low = os.path.join(d, "foo.txt")
    folds = os.path.exists(low)
    same = False
    if folds:
        try:
            with open(low, "r", encoding="utf-8") as fh:
                same = fh.read() == "upper\n"
        except OSError:
            same = False

    # git 側の自動判定も見る (core.ignorecase は init 時に FS を見て決まる)
    r = init_repo(os.path.join(work, "casefold-git"))
    rc, out = git(["config", "--get", "core.ignorecase"], r, check=False)
    ignorecase = out.strip() if rc == 0 else "(未設定)"

    # **`src/lease.rs` は FS を見ていない。** 折り畳むかどうかを
    # `cfg!(any(windows, target_os = "macos"))` = **コンパイル時の OS** で
    # 決めている (`src/lease.rs:240`)。実 FS の挙動と食い違うと:
    #
    #   cfg=畳む / FS=畳まない  → 別物の `Foo.rs` と `foo.rs` を 1 つと見なす
    #                             (**fail-closed**。過剰に断るだけで穴は開かない)
    #   cfg=畳まない / FS=畳む  → **同じファイルに 2 つの鍵ができる**。
    #                             2 人が「互いに素」な行域を同じ実ファイルへ
    #                             持てる = 競合ゼロの主張が破れる
    #
    # 後者は Linux で case-insensitive なマウント (ext4 の casefold /
    # CIFS / exFAT / macOS 由来の共有) を使うと現実に起こる。
    # 式そのものはソースから確認する (下の `read_fold_cfg`)。ここで評価する
    # のは「その式がこの OS で何になるか」だけ。
    src_ok = read_fold_cfg()
    cfg_folds = platform.system() in ("Windows", "Darwin")
    actual = bool(folds and same)
    return {"fs_folds_case": actual,
            "core_ignorecase": ignorecase,
            "cfg_folds_case": cfg_folds,
            # True なら src/lease.rs が想定どおりの式のまま。
            # False = 式が変わった / None = 読めなかった (どちらも判定を弱める)
            "cfg_source_matches": src_ok,
            "mismatch": cfg_folds != actual,
            # 穴が開く向きだけを名指しする。**ソースの確認が取れたときだけ**
            # 落とす (写経した想定で赤にすると必ず嘘の赤になる)。
            "unsound_direction": bool(src_ok) and (not cfg_folds) and actual}


# ═════════════════════════════════════════════════════════════════════
#  G1  merge-tree --write-tree
# ═════════════════════════════════════════════════════════════════════

def probe_merge_tree(work):
    """`src/conflict.rs` と**同じ読み方**で可否を決める。

    版番号では決めない (バックポート版・機能削除版を必ず取り違えるため)。
    引数無しで叩いた usage に `--write-tree` が載るかを見る。
    """
    d = init_repo(os.path.join(work, "mt"))
    p = subprocess.run(["git", "merge-tree", "--write-tree"], cwd=d,
                       env=ENV_ISOLATED, stdout=subprocess.PIPE,
                       stderr=subprocess.STDOUT)
    out = p.stdout.decode("utf-8", "replace")
    # 2.38+ は `usage: git merge-tree [--write-tree] …` を出す。
    # 2.38 未満は option を解析せず `usage: git merge-tree <base-tree> …`。
    avail = "--write-tree" in out
    _, ver = git_version()
    return {"available": avail,
            "expected_by_version": ver >= (2, 38, 0),
            "usage_head": out.strip().splitlines()[0] if out.strip() else ""}


# ═════════════════════════════════════════════════════════════════════
#  G2  既定のマージ戦略 (ort か recursive か)
# ═════════════════════════════════════════════════════════════════════

def probe_strategy(work):
    """`git merge` が名乗る戦略名。

    2.34 以降は ort が既定で、ort は **diff-algorithm=histogram に固定**
    されている (`man git-merge`)。2.34 未満の recursive は myers なので、
    `docs/conflict-zero.md` の帯の考察 (histogram が反復本文でハンクを畳む)
    は**そのままでは当てはまらない**。
    """
    d = init_repo(os.path.join(work, "strat"))
    with open(os.path.join(d, "a.txt"), "w", encoding="utf-8") as fh:
        fh.write("".join("line %d\n" % i for i in range(1, 41)))
    git(["add", "-A"], d)
    git(["commit", "-qm", "base"], d)
    base = head_branch(d)

    git(["checkout", "-q", "-b", "side"], d)
    lines = ["line %d\n" % i for i in range(1, 41)]
    lines[5] = "side change\n"
    with open(os.path.join(d, "a.txt"), "w", encoding="utf-8") as fh:
        fh.write("".join(lines))
    git(["commit", "-qam", "side"], d)

    git(["checkout", "-q", base], d)
    lines = ["line %d\n" % i for i in range(1, 41)]
    lines[30] = "main change\n"
    with open(os.path.join(d, "a.txt"), "w", encoding="utf-8") as fh:
        fh.write("".join(lines))
    git(["commit", "-qam", "main"], d)

    rc, out = git(["merge", "--no-edit", "side"], d, check=False)
    m = re.search(r"Merge made by the '([^']+)' strategy", out)
    name = m.group(1) if m else ("(clean, 戦略名を出さず)" if rc == 0 else "(衝突)")
    return {"strategy": name, "clean": rc == 0}


# ═════════════════════════════════════════════════════════════════════
#  G3  CRLF 保存 — **落とす指標**
# ═════════════════════════════════════════════════════════════════════

def probe_crlf(work, autocrlf):
    """CRLF のファイルへ互いに素な 2 つの編集 → マージが通り、改行が保存されるか。

    CLAUDE.md の実話: `anyrepo-prove.sh` は Python の `open()` を
    `newline=""` 無しで使い、**CRLF のファイルを 1 行残らず LF へ書き換えて**
    いた。当然どの段でも衝突し、実際には守られている Windows 由来の
    リポジトリが「証明できず」になっていた。ここは **git が壊すのか
    道具が壊すのか**を切り分けるための、道具を挟まない素の測定である。

    `autocrlf` は None / "true" / "input" の 3 通りを回す。Windows の
    既定は true なので、**そこだけ結果が違うなら実装の前提が崩れる**。
    """
    tag = autocrlf or "unset"
    d = init_repo(os.path.join(work, "crlf-" + tag))
    if autocrlf:
        git(["config", "core.autocrlf", autocrlf], d)

    path = os.path.join(d, "a.txt")
    # **改行を保存する。** newline="" を外すと Python 側が書き換える
    def write(lines):
        with open(path, "w", encoding="utf-8", newline="") as fh:
            fh.write("".join(lines))

    def read_bytes():
        with open(path, "rb") as fh:
            return fh.read()

    base_lines = ["line %d\r\n" % i for i in range(1, 41)]
    write(base_lines)
    git(["add", "-A"], d)
    git(["commit", "-qm", "base"], d)
    main = head_branch(d)

    git(["checkout", "-q", "-b", "side"], d)
    a = list(base_lines)
    a[5] = "side change\r\n"
    write(a)
    git(["commit", "-qam", "side"], d)

    git(["checkout", "-q", main], d)
    b = list(base_lines)
    b[30] = "main change\r\n"
    write(b)
    git(["commit", "-qam", "main"], d)

    rc, out = git(["merge", "--no-edit", "side"], d, check=False)
    merged = read_bytes()
    crlf = merged.count(b"\r\n")
    lone_lf = merged.count(b"\n") - crlf
    return {
        "autocrlf": tag,
        "clean": rc == 0,
        "crlf_lines": crlf,
        "lone_lf_lines": lone_lf,
        # 作業ツリーの改行は autocrlf の**仕様どおり**変わりうる。
        # 落とすのは「衝突した」ことと「両方の変更が入っていない」ことだけ。
        "both_changes_present": (b"side change" in merged and b"main change" in merged),
    }


# ═════════════════════════════════════════════════════════════════════
#  実行
# ═════════════════════════════════════════════════════════════════════

def main():
    work = tempfile.mkdtemp(prefix="zai-gitprobe-")
    home = os.path.join(work, "home")
    os.makedirs(home, exist_ok=True)
    global ENV_ISOLATED
    ENV_ISOLATED = make_env(home)

    res = {}
    fails = []
    try:
        res["env"] = probe_env(work)
        res["case"] = probe_case_fold(work)
        res["merge_tree"] = probe_merge_tree(work)
        res["strategy"] = probe_strategy(work)
        res["crlf"] = [probe_crlf(work, x) for x in (None, "true", "input")]
    finally:
        shutil.rmtree(work, ignore_errors=True)

    for c in res["crlf"]:
        if not c["clean"]:
            fails.append("G3 CRLF/autocrlf=%s: 互いに素な編集がマージで衝突した" % c["autocrlf"])
        if not c["both_changes_present"]:
            fails.append("G3 CRLF/autocrlf=%s: 両側の変更が結果に入っていない" % c["autocrlf"])

    # G5 は「環境差」ではなく**穴**なので落とす。cfg! が畳まないと決めている
    # のに FS が畳む向きのときだけ (逆向きは fail-closed なので落とさない)。
    if res["case"]["unsound_direction"]:
        fails.append("G5 台帳の鍵: cfg! は畳まないと決めているのに FS は畳む "
                     "(同じファイルへ 2 つの鍵ができる。src/lease.rs:240)")

    res["verdict"] = "ok" if not fails else "broken"
    res["fails"] = fails

    e = res["env"]
    w("== git / OS 移植性プローブ")
    w("   OS:     %s %s (%s)" % (e["platform"], e["release"], e["machine"]))
    w("   git:    %s" % e["git"])
    w("   python: %s" % e["python"])
    w("   実効 core.autocrlf: %s" % e["autocrlf_effective"])
    w("")
    w("== △ 出すだけ (git の版と FS で当然変わる)")
    mt = res["merge_tree"]
    w("   G1 merge-tree --write-tree : %s (版から予想: %s)"
      % ("あり" if mt["available"] else "**なし**",
         "あり" if mt["expected_by_version"] else "なし"))
    if not mt["available"]:
        w("      → `conflict.rs` は行範囲だけで判定へ縮退する。")
        w("        `coedit` の一撃統合の検算と region:: の三方向テストは skip される。")
    w("   G2 既定のマージ戦略        : %s" % res["strategy"]["strategy"])
    if res["strategy"]["strategy"] not in ("ort",):
        w("      → ort ではないので diff は histogram 固定ではない。")
        w("        docs/conflict-zero.md の「ort は histogram 固定」の考察は当たらない。")
    cf = res["case"]
    w("   G4 FS が大文字小文字を畳む : %s (core.ignorecase = %s)"
      % ("はい" if cf["fs_folds_case"] else "いいえ", cf["core_ignorecase"]))
    src_ok = cf["cfg_source_matches"]
    w("      lease.rs の cfg! の想定  : %s (ソース照合: %s)"
      % ("畳む" if cf["cfg_folds_case"] else "畳まない",
         {True: "一致", False: "**式が変わっている**", None: "読めず"}[src_ok]))
    if src_ok is not True:
        w("      → src/lease.rs の fold_case が想定の式ではないので、")
        w("        この行の判定は当てにしないこと (G5 は落としません)。")
    if cf["unsound_direction"]:
        w("      → **食い違い (穴の開く向き)**: `Foo.rs` と `foo.rs` は")
        w("        同じファイルなのに台帳では別の鍵になる。2 人が「互いに素」な")
        w("        行域を同じ実ファイルへ持てる。src/lease.rs:240 は FS を見ていない。")
    elif cf["mismatch"]:
        w("      → 食い違い (fail-closed 側)。実 FS は畳まないのに台帳は畳むので、")
        w("        本当は別物の `Foo.rs` / `foo.rs` を 1 つと見なして過剰に断る。")
    w("")
    w("== ✗ 落とす指標")
    for c in res["crlf"]:
        w("   G3 CRLF (autocrlf=%-5s) : マージ %s / CRLF 行 %d / 単独 LF 行 %d / 両側の変更 %s"
          % (c["autocrlf"], "clean" if c["clean"] else "**衝突**",
             c["crlf_lines"], c["lone_lf_lines"],
             "あり" if c["both_changes_present"] else "**なし**"))
    w("")
    if fails:
        w("== 判定: broken")
        for f in fails:
            w("   - %s" % f)
    else:
        w("== 判定: ok (落とす指標はすべて緑)")

    if WANT_JSON:
        sys.stdout.write(J.dumps(res, ensure_ascii=False, indent=2) + "\n")
    return 0 if not fails else 1


sys.exit(main())
PY
