#!/usr/bin/env python3
"""Measure duplicate Read(file_path) calls after the latest compaction summary.

Usage:
  python3 benchmarks/scripts/post_compact_rereads.py path/to/session.json

Expects a SessionFile-like JSON with a top-level "messages" array (wyj-code
session dump). Prints:
  compact_generation=<n or unknown>
  duplicate_reads_after_compact=<count>

Exit 0 always when the file parses; use the count in harness comparisons.
This is the cheap T4 fidelity signal from the long-session plan: after
compaction, repeated Read of the same path is "amnesia labor".
"""

from __future__ import annotations

import json
import sys
from collections import Counter


SUMMARY_PREFIX = "[Conversation summary —"


def is_summary(msg: dict) -> bool:
    if msg.get("role") != "user":
        return False
    for block in msg.get("content") or []:
        if not isinstance(block, dict):
            continue
        text = block.get("text") or ""
        if text.startswith(SUMMARY_PREFIX):
            return True
    return False


def generation_of(msg: dict) -> int | None:
    for block in msg.get("content") or []:
        if not isinstance(block, dict):
            continue
        text = block.get("text") or ""
        if "generation " not in text[:600]:
            continue
        after = text.split("generation ", 1)[1]
        digits = "".join(ch for ch in after if ch.isdigit())
        if digits:
            return int(digits)
        return 1
    return None


def read_paths(msg: dict) -> list[str]:
    paths: list[str] = []
    for block in msg.get("content") or []:
        if not isinstance(block, dict):
            continue
        if block.get("type") not in (None, "tool_use") and "name" not in block:
            # Anthropic-style ContentBlock::ToolUse serializes as object with name
            pass
        name = block.get("name")
        if name != "Read":
            continue
        inp = block.get("input") or {}
        path = (inp.get("file_path") or inp.get("path") or "").strip()
        if path:
            paths.append(path)
    return paths


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    with open(sys.argv[1], encoding="utf-8") as f:
        data = json.load(f)
    messages = data.get("messages") or []
    start = None
    gen = None
    for i, msg in enumerate(messages):
        if is_summary(msg):
            start = i
            gen = generation_of(msg) or gen
    if start is None:
        print("compact_generation=none")
        print("duplicate_reads_after_compact=0")
        return 0
    counts: Counter[str] = Counter()
    dupes = 0
    for msg in messages[start + 1 :]:
        for path in read_paths(msg):
            counts[path] += 1
            if counts[path] >= 2:
                dupes += 1
    print(f"compact_generation={gen if gen is not None else 'unknown'}")
    print(f"duplicate_reads_after_compact={dupes}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
