#!/usr/bin/env python3
"""Render captured terminal output (with ANSI colours) as a standalone dark-terminal SVG.

    ansi_to_svg.py INPUT.ansi OUTPUT.svg [--cols 120] [--title TEXT]

Standard library only. It is a deliberately small terminal emulator: printable text, SGR colours
(16 colours, bold, dim), carriage return (so a progress line redrawn with \\r shows its final
frame), line feed, and soft wrapping at --cols as a real terminal wraps. Every other escape
sequence is dropped. The SVG carries its own opaque terminal background, so it reads the same on
GitHub's light and dark themes.
"""
import argparse
import re
import sys
from xml.sax.saxutils import escape

PALETTE = {
    30: "#484f58", 31: "#ff7b72", 32: "#3fb950", 33: "#d29922",
    34: "#58a6ff", 35: "#bc8cff", 36: "#39c5cf", 37: "#b1bac4",
    90: "#6e7681", 91: "#ffa198", 92: "#56d364", 93: "#e3b341",
    94: "#79c0ff", 95: "#d2a8ff", 96: "#56d4dd", 97: "#f0f6fc",
}
DEFAULT_FG = "#c9d1d9"
BACKGROUND = "#0d1117"
TITLEBAR = "#161b22"
BORDER = "#30363d"

CSI = re.compile(r"\x1b\[([0-9;?]*)([ -/]*[@-~])")
OTHER_ESC = re.compile(r"\x1b[\]PX^_].*?(?:\x07|\x1b\\)|\x1b[@-Z\\-_]")


def parse(text, cols):
    """Return rows: each a list of (char, fg, bold, dim) cells."""
    rows = [[]]
    row_i = 0
    col = 0
    fg, bold, dim = None, False, False

    def put(ch):
        nonlocal rows, row_i, col
        if col >= cols:  # soft wrap, as a terminal does
            rows.append([])
            row_i += 1
            col = 0
        row = rows[row_i]
        cell = (ch, fg, bold, dim)
        if col < len(row):
            row[col] = cell
        else:
            row.extend([(" ", None, False, False)] * (col - len(row)))
            row.append(cell)
        col += 1

    i = 0
    while i < len(text):
        ch = text[i]
        if ch == "\x1b":
            m = CSI.match(text, i)
            if m:
                if m.group(2) == "m":
                    params = [int(p) if p else 0 for p in m.group(1).split(";")] or [0]
                    for p in params:
                        if p == 0:
                            fg, bold, dim = None, False, False
                        elif p == 1:
                            bold = True
                        elif p == 2:
                            dim = True
                        elif p == 22:
                            bold = dim = False
                        elif p == 39:
                            fg = None
                        elif p in PALETTE:
                            fg = PALETTE[p]
                i = m.end()
                continue
            m = OTHER_ESC.match(text, i)
            i = m.end() if m else i + 1
            continue
        if ch == "\r":
            col = 0
        elif ch == "\n":
            rows.append([])
            row_i += 1
            col = 0
        elif ch == "\t":
            for _ in range(8 - col % 8):
                put(" ")
        elif ch >= " " and ch != "\x7f":
            put(ch)
        i += 1
    while rows and not rows[-1]:
        rows.pop()
    return rows


def runs(row):
    out = []
    for ch, fg, bold, dim in row:
        key = (fg, bold, dim)
        if out and out[-1][0] == key:
            out[-1][1].append(ch)
        else:
            out.append((key, [ch]))
    return [(key, "".join(chars)) for key, chars in out]


def render(rows, cols, title, font_px=12.0):
    cell_w = font_px * 0.6
    line_h = font_px * 1.55
    pad_x, pad_top, pad_bottom, bar_h = 22, 16, 18, 36
    width = round(cols * cell_w + pad_x * 2)
    height = round(bar_h + pad_top + len(rows) * line_h + pad_bottom)
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" role="img" aria-label="{escape(title)}">',
        f"<title>{escape(title)}</title>",
        f'<rect x="0.5" y="0.5" width="{width - 1}" height="{height - 1}" rx="10" fill="{BACKGROUND}" stroke="{BORDER}"/>',
        f'<path d="M0.5 {bar_h} V10.5 a10 10 0 0 1 10 -10 H{width - 10.5} a10 10 0 0 1 10 10 V{bar_h} Z" fill="{TITLEBAR}"/>',
        f'<line x1="0.5" y1="{bar_h}" x2="{width - 0.5}" y2="{bar_h}" stroke="{BORDER}"/>',
        '<circle cx="20" cy="18" r="6" fill="#ff5f56"/>',
        '<circle cx="40" cy="18" r="6" fill="#ffbd2e"/>',
        '<circle cx="60" cy="18" r="6" fill="#27c93f"/>',
        f'<text x="{width / 2:.1f}" y="22" text-anchor="middle" font-family="-apple-system, \'Segoe UI\', Helvetica, Arial, sans-serif" '
        f'font-size="12" fill="#8b949e">{escape(title)}</text>',
        f'<g font-family="\'JetBrains Mono\', \'SFMono-Regular\', Consolas, \'Liberation Mono\', Menlo, \'DejaVu Sans Mono\', monospace" '
        f'font-size="{font_px}" style="white-space:pre" xml:space="preserve">',
    ]
    y0 = bar_h + pad_top + font_px
    for n, row in enumerate(rows):
        if not row:
            continue
        y = y0 + n * line_h
        spans = []
        for (fg, bold, dim), text in runs(row):
            style = f' fill="{fg or DEFAULT_FG}"'
            if bold:
                style += ' font-weight="700"'
            if dim:
                style += ' opacity="0.65"'
            spans.append(f"<tspan{style}>{escape(text)}</tspan>")
        parts.append(f'<text x="{pad_x}" y="{y:.1f}" xml:space="preserve" style="white-space:pre">{"".join(spans)}</text>')
    parts.append("</g></svg>")
    return "\n".join(parts) + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("input")
    ap.add_argument("output")
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--title", default="synology-drive-sync")
    args = ap.parse_args()
    with open(args.input, encoding="utf-8", errors="replace", newline="") as handle:
        text = handle.read()
    rows = parse(text, args.cols)
    # Trailing spaces are invisible; trim them so a redrawn progress line does not widen the picture.
    rows = [[c for c in row][: len("".join(c[0] for c in row).rstrip())] for row in rows]
    if not rows:
        sys.exit("ansi_to_svg: nothing to render")
    with open(args.output, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(render(rows, args.cols, args.title))


if __name__ == "__main__":
    main()
