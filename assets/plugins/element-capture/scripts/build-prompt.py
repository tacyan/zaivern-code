#!/usr/bin/env python3
"""選択された要素の JSON と切り取り画像のパスから、エージェントへ渡す日本語プロンプトを組み立てる。

使い方: build-prompt.py <pick.json> [screenshot.png]
値が取れなかった項目は「取得不可」と明記し、推測で埋めない。
"""

import json
import sys


def main() -> int:
    if len(sys.argv) < 2:
        sys.stderr.write("引数が足りません\n")
        return 1

    shot = sys.argv[2] if len(sys.argv) > 2 else ""

    try:
        with open(sys.argv[1], "r", encoding="utf-8") as fh:
            pick = json.load(fh)
    except (OSError, json.JSONDecodeError):
        pick = {}

    def get(key, default="取得不可"):
        value = pick.get(key)
        if value in (None, "", [], {}):
            return default
        return value

    rect = pick.get("rect") or {}
    css = pick.get("css") or {}

    lines = ["以下はブラウザ上で選択した UI 要素の情報です。", ""]
    lines.append("- ページ: %s" % get("pageTitle"))
    lines.append("- URL: %s" % get("url"))
    lines.append("- タグ: %s" % get("tag"))
    lines.append("- セレクタ: `%s`" % get("selector"))
    if rect:
        lines.append(
            "- 表示位置/大きさ: x=%s y=%s w=%s h=%s"
            % (rect.get("x", "?"), rect.get("y", "?"), rect.get("w", "?"), rect.get("h", "?"))
        )
    else:
        lines.append("- 表示位置/大きさ: 取得不可")
    lines.append("- 切り取り画像: %s" % (shot if shot else "取得不可 (切り取りを中止しました)"))

    text = pick.get("text")
    if text:
        lines += ["", "## 表示テキスト", "```", text.strip(), "```"]

    lines += ["", "## 主要な CSS"]
    if css:
        lines.append("```css")
        for key in sorted(css):
            lines.append("%s: %s;" % (key, css[key]))
        lines.append("```")
    else:
        lines.append("取得不可")

    lines += ["", "## HTML"]
    html = pick.get("html")
    if html:
        lines.append("```html")
        lines.append(html)
        lines.append("```")
        if pick.get("truncated"):
            lines.append("")
            lines.append("※ HTML は長いため途中で切り詰めています。")
    else:
        lines.append("取得不可")

    lines += ["", "この要素を参考に作業を進めてください。"]
    sys.stdout.write("\n".join(lines) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
