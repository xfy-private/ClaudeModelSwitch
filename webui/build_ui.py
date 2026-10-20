#!/usr/bin/env python3
"""一键构建 Web UI：编译 Tailwind CSS 并注入 index.html。

用法（项目根）:
    python webui/build_ui.py

流程:
    1. 把 index.html 里 <style id="tw"> 块重置为占位标记（保证 Tailwind 只扫描干净源码）
    2. npx @tailwindcss/cli 编译 webui/tailwind/in.css（@source 指向 ../index.html）
    3. 把编译产物注入 <style id="tw"> 块，覆盖 index.html
    4. cargo build 重新打包 exe（webui.rs include_str! 一起进 exe）

幂等：可重复运行。
"""
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INDEX = ROOT / "webui" / "index.html"
TW_DIR = ROOT / "webui" / "tailwind"
IN_CSS = TW_DIR / "in.css"
OUT_CSS = TW_DIR / "out.css"
MARK = "/*__TAILWIND_CSS__*/"
STYLE_RE = re.compile(r'(<style id="tw">).*?(</style>)', re.S)


def reset_marker(html: str) -> str:
    """把 <style id="tw"> 内容重置为占位标记。

    内容可能是占位标记（首次构建）或已注入的编译产物（重复构建），
    一律重置，保证 Tailwind 扫描干净源码。
    """
    if not STYLE_RE.search(html):
        raise SystemExit('index.html 中找不到 <style id="tw"> 占位块')
    return STYLE_RE.sub(lambda m: m.group(1) + MARK + m.group(2), html)


def compile_css() -> str:
    """运行 Tailwind CLI，返回编译后的 CSS 文本。"""
    npx = "npx.cmd" if sys.platform == "win32" else "npx"
    cmd = [npx, "@tailwindcss/cli", "-i", str(IN_CSS), "-o", str(OUT_CSS), "--minify"]
    print("  run:", " ".join(cmd))
    subprocess.run(cmd, cwd=str(TW_DIR), check=True)
    return OUT_CSS.read_text(encoding="utf-8").strip()


def main() -> int:
    print("== ClaudeRelay Web UI build ==")
    html = INDEX.read_text(encoding="utf-8")

    # 1) 先重置占位块，确保扫描的是干净源码（避免把已注入的编译产物再扫一遍）
    html = reset_marker(html)
    INDEX.write_text(html, encoding="utf-8")

    # 2) 编译
    try:
        css = compile_css()
    except Exception as e:
        print("!! Tailwind 编译失败，index.html 已回到源码态（仅占位块）:", e, file=sys.stderr)
        return 1

    # 3) 注入编译产物
    html = INDEX.read_text(encoding="utf-8")
    html = STYLE_RE.sub(lambda m: m.group(1) + css + m.group(2), html)
    INDEX.write_text(html, encoding="utf-8")

    print(f"OK: injected {len(css)} bytes Tailwind CSS -> {INDEX}")
    print("下一步: cargo build --release 重新打包 exe")
    return 0


if __name__ == "__main__":
    sys.exit(main())
