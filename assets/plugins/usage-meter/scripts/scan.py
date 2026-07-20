#!/usr/bin/env python3
"""ローカルに残っているエージェントのセッション記録を走査し、使用量の目安を Markdown で出す。

スキーマは決め打ちにしない。ホーム直下の隠しディレクトリを探索し、セッションらしい
ファイル (*.jsonl / *.json) を見つけたら、その中に現れる利用量らしき数値キーだけを
拾い集める。見つからない項目は必ず「取得不可」と書き、値を作らない。
"""

import json
import os
import sys

# 利用量とみなす数値キー (見つかった分だけ合算する)
TOKEN_KEYS = (
    "input_tokens",
    "output_tokens",
    "total_tokens",
    "prompt_tokens",
    "completion_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
)
COST_KEYS = ("cost", "cost_usd", "total_cost", "total_cost_usd")

SESSION_SUFFIXES = (".jsonl", ".json")
SKIP_DIR_NAMES = {"node_modules", ".git", "cache", "Cache", "tmp", "bin", "lib"}

MAX_FILES_PER_DIR = 400
MAX_PARSE_FILES = 60
MAX_BYTES_PER_FILE = 2 * 1024 * 1024


def human(num_bytes):
    size = float(num_bytes)
    for unit in ("B", "KB", "MB", "GB"):
        if size < 1024.0 or unit == "GB":
            return "%.1f %s" % (size, unit)
        size /= 1024.0
    return "%.1f GB" % size


MAX_DEPTH = 4
MAX_DIRS_PER_ROOT = 600


def walk_sessions(root):
    """セッションらしいファイルの (パス, サイズ, 更新時刻) を集める。
    深さと訪問ディレクトリ数に上限を設け、巨大なキャッシュを掘り続けないようにする。"""
    found = []
    visited = 0
    base_depth = root.rstrip(os.sep).count(os.sep)
    for dirpath, dirnames, filenames in os.walk(root):
        visited += 1
        if visited > MAX_DIRS_PER_ROOT:
            break
        if dirpath.count(os.sep) - base_depth >= MAX_DEPTH:
            dirnames[:] = []
        else:
            dirnames[:] = [d for d in dirnames if d not in SKIP_DIR_NAMES]
        if len(found) >= MAX_FILES_PER_DIR:
            break
        for name in filenames:
            if not name.endswith(SESSION_SUFFIXES):
                continue
            path = os.path.join(dirpath, name)
            try:
                st = os.stat(path)
            except OSError:
                continue
            found.append((path, st.st_size, st.st_mtime))
            if len(found) >= MAX_FILES_PER_DIR:
                break
    return found


def collect_numbers(obj, tokens, costs, depth=0):
    """入れ子の JSON を辿り、利用量らしき数値キーだけを拾う。"""
    if depth > 8:
        return
    if isinstance(obj, dict):
        for key, value in obj.items():
            lowered = key.lower()
            if lowered in TOKEN_KEYS and isinstance(value, (int, float)):
                tokens[lowered] = tokens.get(lowered, 0) + value
            elif lowered in COST_KEYS and isinstance(value, (int, float)):
                costs[lowered] = costs.get(lowered, 0.0) + float(value)
            else:
                collect_numbers(value, tokens, costs, depth + 1)
    elif isinstance(obj, list):
        for item in obj[:200]:
            collect_numbers(item, tokens, costs, depth + 1)


def parse_usage(files):
    tokens = {}
    costs = {}
    parsed = 0
    for path, size, _mtime in sorted(files, key=lambda f: f[2], reverse=True):
        if parsed >= MAX_PARSE_FILES:
            break
        if size > MAX_BYTES_PER_FILE or size == 0:
            continue
        try:
            with open(path, "r", encoding="utf-8", errors="replace") as fh:
                text = fh.read(MAX_BYTES_PER_FILE)
        except OSError:
            continue
        parsed += 1
        stripped = text.lstrip()
        if stripped.startswith("{") and "\n{" not in stripped:
            try:
                collect_numbers(json.loads(text), tokens, costs)
            except json.JSONDecodeError:
                pass
            continue
        for line in text.splitlines():
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                collect_numbers(json.loads(line), tokens, costs)
            except json.JSONDecodeError:
                continue
    return tokens, costs, parsed


# 会話・セッション記録が置かれがちなディレクトリ名。製品固有の名前は含めない。
SESSION_DIR_HINTS = (
    "sessions", "session", "history", "conversations", "conversation",
    "chats", "chat", "projects", "transcripts", "threads", "usage",
)


def looks_like_session_store(path):
    """直下に会話記録らしいディレクトリがあるかどうかで、走査対象かを判断する。"""
    try:
        entries = os.listdir(path)
    except OSError:
        return []
    hits = [
        os.path.join(path, name)
        for name in entries
        if name.lower() in SESSION_DIR_HINTS and os.path.isdir(os.path.join(path, name))
    ]
    return hits


def discover_roots(home, extra):
    """記録の置き場所を探す。設定で指定された場所はそのまま、ホーム直下の隠し
    ディレクトリは会話記録らしい構造を持つものだけを対象にする。"""
    roots = []
    for entry in extra:
        entry = os.path.expanduser(entry.strip())
        if entry and os.path.isdir(entry):
            roots.append((os.path.basename(entry.rstrip(os.sep)) or entry, [entry]))
    try:
        names = sorted(os.listdir(home))
    except OSError:
        names = []
    for name in names:
        if not name.startswith("."):
            continue
        path = os.path.join(home, name)
        if not os.path.isdir(path) or os.path.islink(path):
            continue
        hits = looks_like_session_store(path)
        if hits:
            roots.append((name, hits))
    return roots


def main() -> int:
    home = os.path.expanduser("~")
    extra = (sys.argv[1].split(",") if len(sys.argv) > 1 and sys.argv[1] else [])
    reports = []

    for name, dirs in discover_roots(home, extra):
        files = []
        for one in dirs:
            files.extend(walk_sessions(one))
        if not files:
            continue
        tokens, costs, parsed = parse_usage(files)
        if not tokens and not costs and len(files) < 3:
            continue
        reports.append(
            {
                "name": name,
                "path": ", ".join(dirs),
                "sessions": len(files),
                "bytes": sum(f[1] for f in files),
                "tokens": tokens,
                "costs": costs,
                "parsed": parsed,
            }
        )

    out = ["## 使用量の目安", ""]
    if not reports:
        out.append("ローカルにセッション記録が見つかりませんでした: **取得不可**")
        out.append("")
        out.append("設定「追加の探索ディレクトリ」に記録の置き場所を指定すると集計できます。")
        sys.stdout.write("\n".join(out) + "\n")
        return 0

    reports.sort(key=lambda r: r["sessions"], reverse=True)

    out.append("| 記録元 | セッション数 | 記録サイズ | 入力トークン | 出力トークン | 費用 |")
    out.append("| --- | ---: | ---: | ---: | ---: | ---: |")
    for rep in reports[:12]:
        tok = rep["tokens"]
        cost = rep["costs"]
        inp = tok.get("input_tokens", tok.get("prompt_tokens"))
        outp = tok.get("output_tokens", tok.get("completion_tokens"))
        total_cost = sum(cost.values()) if cost else None
        out.append(
            "| %s | %d | %s | %s | %s | %s |"
            % (
                rep["name"],
                rep["sessions"],
                human(rep["bytes"]),
                ("{:,}".format(int(inp)) if inp is not None else "取得不可"),
                ("{:,}".format(int(outp)) if outp is not None else "取得不可"),
                ("$%.2f" % total_cost if total_cost is not None else "取得不可"),
            )
        )

    total_sessions = sum(r["sessions"] for r in reports)
    total_bytes = sum(r["bytes"] for r in reports)
    out += [
        "",
        "合計 %d セッション / %s" % (total_sessions, human(total_bytes)),
        "",
        "### 内訳",
    ]
    for rep in reports[:12]:
        detail = []
        for key in TOKEN_KEYS:
            if key in rep["tokens"]:
                detail.append("%s=%s" % (key, "{:,}".format(int(rep["tokens"][key]))))
        for key, value in sorted(rep["costs"].items()):
            detail.append("%s=%.4f" % (key, value))
        out.append(
            "- **%s** (`%s`) — 解析 %d ファイル: %s"
            % (
                rep["name"],
                rep["path"],
                rep["parsed"],
                ", ".join(detail) if detail else "利用量の数値は 取得不可",
            )
        )

    out += [
        "",
        "※ 数値は記録ファイルに実際に書かれていた値のみを合算しています。",
        "※ 記録が無い項目は「取得不可」と表示し、推定は行いません。",
    ]
    sys.stdout.write("\n".join(out) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
