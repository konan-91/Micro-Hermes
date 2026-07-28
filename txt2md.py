#!/usr/bin/env python3
"""Convert the hard-wrapped dissertation.txt into dissertation.md.

The only transformation is unwrapping: lines that were manually broken to fit a
column width get rejoined into single running paragraphs, so the text pastes
into Word without carrying the fake line breaks. Nothing else is touched, no
headings are marked up, no wording is altered.

Usage: python3 txt2md.py [input.txt] [output.md]
"""

import re
import sys

LIST_ITEM = re.compile(r"^-\s+")
CODE_INDENT = re.compile(r"^ {4,}\S")
NOTE_START = re.compile(r"^\[(FIGURE|TABLE|todo)\b", re.IGNORECASE)
CAPTION = re.compile(r"^Caption:", re.IGNORECASE)


def join(lines):
    """Join wrapped lines into one line.

    A line ending in a hyphen followed by a lowercase word is a hyphenated
    compound split across the wrap (long-lived-\nconnection), so it rejoins
    with no space. Everything else joins with a single space.
    """
    out = ""
    for line in lines:
        piece = line.strip()
        if not piece:
            continue
        if not out:
            out = piece
        elif out.endswith("-") and piece[:1].islower():
            out += piece
        else:
            out += " " + piece
    return out


def split_on(lines, pattern):
    """Split a block into segments, each starting at a line matching pattern."""
    segments = []
    current = []
    for line in lines:
        if pattern.match(line.strip()) and current:
            segments.append(current)
            current = [line]
        else:
            current.append(line)
    if current:
        segments.append(current)
    return segments


def convert_block(lines):
    """Return the output lines for one blank-line-delimited block."""
    # Indented literal blocks (shell commands, env var tables) stay verbatim.
    if all(CODE_INDENT.match(line) for line in lines):
        return list(lines)

    # Bullet lists and the reference list: unwrap each item, keep items apart.
    if LIST_ITEM.match(lines[0].strip()):
        items = split_on(lines, LIST_ITEM)
        return ["- " + join(item).lstrip("- ").strip() for item in items]

    # [FIGURE n HERE ...] / [TABLE n HERE ...] notes: keep the asset path line
    # separate from its caption, unwrap each.
    if NOTE_START.match(lines[0].strip()):
        segments = split_on(lines, CAPTION)
        out = []
        for i, segment in enumerate(segments):
            if i:
                out.append("")
            out.append(join(segment))
        return out

    # Ordinary prose: one running paragraph.
    return [join(lines)]


def convert(text):
    out = []
    block = []
    for line in text.split("\n"):
        if line.strip():
            block.append(line.rstrip())
        else:
            if block:
                out.extend(convert_block(block))
                block = []
            out.append("")
    if block:
        out.extend(convert_block(block))
    return "\n".join(out)


def words(text):
    return re.findall(r"\S+", text.replace("-\n", "-"))


def main():
    src = sys.argv[1] if len(sys.argv) > 1 else "dissertation.txt"
    dst = sys.argv[2] if len(sys.argv) > 2 else "dissertation.md"

    with open(src, encoding="utf-8") as fh:
        original = fh.read()

    result = convert(original)

    with open(dst, "w", encoding="utf-8") as fh:
        fh.write(result)

    before, after = len(words(original)), len(words(result))
    print(f"{src} -> {dst}")
    print(f"lines: {original.count(chr(10))} -> {result.count(chr(10))}")
    print(f"words: {before} -> {after} ({'ok' if before == after else 'MISMATCH'})")


if __name__ == "__main__":
    main()
