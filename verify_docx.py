#!/usr/bin/env python3
"""
Independent verification that dissertation.docx contains every character of
body text from dissertation.md, unchanged, in the same order.

Method: reconstruct the ordered list of "text units" expected from the .md
(front-matter lines, then every non-blank body line, with contiguous
indented-code runs treated as one unit joined by \\n, exactly mirroring the
splitting rule in build_docx.py) and compare it against the ordered list of
non-empty paragraph texts actually found in the .docx (title page + TOC page
paragraphs excluded/handled separately, then every body paragraph).

This script does not know or care about styling (headings, colours, fonts)
- it only checks that the TEXT survived the conversion 1:1.
"""
import sys
from pathlib import Path

from docx import Document

SRC = Path(__file__).parent / "dissertation.md"
DOCX = Path(__file__).parent / "dissertation.docx"


def expected_units():
    raw_lines = SRC.read_text(encoding="utf-8").splitlines()

    front_matter = []
    body_start = 0
    for idx, l in enumerate(raw_lines):
        if l.strip() != "":
            front_matter.append(l)
        if len(front_matter) == 4 and l.strip() != "":
            body_start = idx + 1
            break
    body_lines = raw_lines[body_start:]

    units = list(front_matter)

    i, n = 0, len(body_lines)
    while i < n:
        if body_lines[i].strip() == "":
            i += 1
            continue
        run_start = i
        while i < n and body_lines[i].strip() != "":
            i += 1
        run = body_lines[run_start:i]
        if all(l.startswith("    ") for l in run):
            units.append("\n".join(run))
        else:
            units.extend(run)
    return units


def actual_units():
    doc = Document(DOCX)
    return [p.text for p in doc.paragraphs if p.text.strip() != ""]


def main():
    expected = expected_units()
    actual = actual_units()

    # The actual list has extra front-matter-echo on the title page (title,
    # subtitle, student, university each as their own paragraph) plus the
    # "Table of Contents" heading + TOC field placeholder on the TOC page,
    # ahead of the body. Strip those known, fixed-count prefix paragraphs.
    front_matter = expected[:4]
    assert actual[:4] == front_matter, (
        f"Title page mismatch.\nExpected: {front_matter}\nGot: {actual[:4]}"
    )

    idx = 4
    assert actual[idx] == "Table of Contents", actual[idx]
    idx += 1
    # Skip the TOC field placeholder paragraph (not part of source content).
    assert "Update Field" in actual[idx], actual[idx]
    idx += 1

    body_actual = actual[idx:]
    body_expected = expected[4:]

    if body_actual != body_expected:
        n = min(len(body_actual), len(body_expected))
        for i in range(n):
            if body_actual[i] != body_expected[i]:
                print("MISMATCH at body paragraph", i)
                print("  expected:", repr(body_expected[i])[:300])
                print("  actual:  ", repr(body_actual[i])[:300])
                sys.exit(1)
        print(f"LENGTH MISMATCH: expected {len(body_expected)} paragraphs, "
              f"got {len(body_actual)}")
        if len(body_expected) > n:
            print("First missing expected paragraph:", repr(body_expected[n])[:300])
        if len(body_actual) > n:
            print("First extra actual paragraph:", repr(body_actual[n])[:300])
        sys.exit(1)

    print(f"OK: {len(front_matter)} title-page lines + "
          f"{len(body_expected)} body paragraphs match 1:1 "
          f"({sum(len(u) for u in expected)} characters total).")


if __name__ == "__main__":
    sys.exit(main())
