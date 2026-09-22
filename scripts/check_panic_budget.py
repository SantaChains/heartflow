#!/usr/bin/env python3
"""Ratcheting budget for panic-prone constructs in production Rust.

Companion to `scripts/bench-gate.sh`: same three-mode shape (save / compare /
fail-over-threshold), except **this baseline belongs in the repo** — it is a
property of the source tree, not of the machine that measured it.

Skeleton derived from jcode (https://github.com/1jehuang/jcode, MIT License,
Copyright (c) 2025 Jeremy Huang): the three-mode shape and the brace-counting
exclusion of inline `#[cfg(test)]` modules are theirs. The `justified` class and
the `panic-ok:` marker are heartflow's addition. See NOTICE.

Why a ratchet instead of a cleanup: the tree already carries panic-prone
idioms that are deliberate (invariant asserts after a `take()` on a stream we
just wired up, `serde_json` on a `Value` that cannot fail to serialize, a
`OnceLock` regex compiled from a string literal). Demanding zero would force
those into contortions that are *worse* than the panic. So the gate is "no
worse than the baseline", and the escape hatch is explicit.

Three classes, in priority order:

  1. `test`       — inside a `#[cfg(test)]` item, or in a test-only file.
                    Never counted. Tests are where panics belong.
  2. `justified`  — carries an explicit `panic-ok: <reason>` marker. Counted,
                    reported, and allowed to grow — but every mark shows up in
                    the diff as a written-down reason, which is exactly the
                    review pressure we want. A bare marker with no reason does
                    NOT count as justified (see REASON_MIN_CHARS).
  3. `debt`       — everything else. Hard-gated.

Marker syntax (either form; the reason is mandatory):

    let value = map.get(key).expect("validated above"); // panic-ok: keys are
    // panic-ok: parsed from a static table, so the lookup cannot miss
    let value = map.get(key).expect("validated above");

How to tell `justified` from `debt` — the only question that matters:

  justified  no non-panicking alternative exists in that API, or the alternative
             is strictly worse: it threads `Result` through many call sites for a
             condition nobody can recover from, or it restructures an invariant
             the type system cannot express.
  debt       a local *and* equally clear non-panicking rewrite exists. Write it.

`Regex::new(<literal>)` is the archetype of the first — Rust ships no infallible
regex constructor, so the panic is the price of the API. `path.parent().expect(…)`
inside a function that already returns `Result` is the archetype of the second:
`ok_or_else(…)?` is the same length and the panic disappears.

Reachability is a *separate, stronger* question, and this script deliberately does
not try to answer it: `debt` means "a local, equally clear rewrite exists", **not**
"this can panic in production". A hit can be `debt` and still be unreachable behind
an invariant (e.g. a `join` chain that always yields a parent). In that case the
rewrite buys durability against future edits, not safety. Do not read `debt` as a
bug list.

`panic-ok` is deliberately spelled without a `hf-` prefix so it greps cleanly
across the whole tree and reads the same in every language's comment syntax.

Usage:
    scripts/check_panic_budget.py --list              # every hit, classified
    scripts/check_panic_budget.py --update            # refresh the baseline
    scripts/check_panic_budget.py                     # gate (exit 1 on regress)
    scripts/check_panic_budget.py --baseline path.json
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any, Iterable

REPO_ROOT = Path(__file__).resolve().parent.parent
BASELINE_FILE = REPO_ROOT / "scripts" / "panic_budget.json"

# Only production source is in scope. `archive/` holds vendored reference
# projects and `target/` holds build output — neither is our code to fix.
SCAN_GLOBS = ("crates/*/src/**/*.rs",)

# Ordered longest-first so `.expect(` is never misread as `.unwrap(`-adjacent.
PATTERNS: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("unimplemented!", re.compile(r"\bunimplemented!\s*\(")),
    ("todo!", re.compile(r"\btodo!\s*\(")),
    ("panic!", re.compile(r"\bpanic!\s*\(")),
    ("expect", re.compile(r"\.expect\s*\(")),
    ("unwrap", re.compile(r"\.unwrap\s*\(")),
)

MARKER = re.compile(r"panic-ok\s*:\s*(?P<reason>.*)$")
# A marker with a shorter reason than this is treated as unmarked: "panic-ok: ok"
# is not a justification, it is a rubber stamp.
REASON_MIN_CHARS = 8

CFG_TEST_ATTR = re.compile(r"#\s*\[\s*cfg\s*\(")
CFG_PREDICATE_TEST = re.compile(r"(?<![\w\"=])test(?![\w\"])")
ITEM_START = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:mod|fn|impl|struct|enum|trait)\b")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--update", action="store_true", help="rewrite the baseline from the current tree")
    parser.add_argument("--list", action="store_true", help="print every hit with its classification")
    parser.add_argument("--baseline", type=Path, default=BASELINE_FILE)
    return parser.parse_args()


# ── source scanning ─────────────────────────────────────────────────────────


def is_test_file(path: Path) -> bool:
    parts = path.relative_to(REPO_ROOT).as_posix().split("/")
    if "tests" in parts:
        return True
    name = path.name
    return name == "tests.rs" or name.startswith("tests_") or name.endswith(("_tests.rs", "_test.rs"))


def production_files() -> list[Path]:
    found: set[Path] = set()
    for pattern in SCAN_GLOBS:
        found.update(p for p in REPO_ROOT.glob(pattern) if p.is_file() and not is_test_file(p))
    return sorted(found)


def blank_comments_and_strings(text: str) -> str:
    """Replace comment and string *content* with spaces, preserving length.

    The budget is a lexer-lite ratchet, not a compiler. But it does have to
    ignore the two places where a panic-shaped token is routinely not a panic:
    a comment that documents one, and a string fixture that embeds one. Both
    would otherwise be counted forever and never be removable — the exact
    failure mode that makes people abandon a budget.

    Length-preserving so callers can keep slicing by line. Handles nested block
    comments, raw strings (`r#"…"#`), byte strings, char literals, and does not
    mistake a lifetime (`'a`) for an unterminated char literal.
    """
    out = list(text)
    i = 0
    n = len(text)
    block_depth = 0
    while i < n:
        ch = text[i]
        if block_depth:
            if text.startswith("/*", i):
                block_depth += 1
                out[i] = out[i + 1] = " "
                i += 2
            elif text.startswith("*/", i):
                block_depth -= 1
                out[i] = out[i + 1] = " "
                i += 2
            else:
                if ch != "\n":
                    out[i] = " "
                i += 1
            continue
        if text.startswith("//", i):
            while i < n and text[i] != "\n":
                out[i] = " "
                i += 1
            continue
        if text.startswith("/*", i):
            block_depth = 1
            out[i] = out[i + 1] = " "
            i += 2
            continue
        # Raw string: r"…", r#"…"#, br#"…"#
        raw = re.match(r'(?:b?r)(?P<hashes>#{0,255})"', text[i:])
        if raw:
            hashes = raw.group("hashes")
            start = i + raw.end()
            closer = '"' + hashes
            end = text.find(closer, start)
            end = n if end == -1 else end + len(closer)
            for j in range(i, end):
                if text[j] != "\n":
                    out[j] = " "
            i = end
            continue
        if ch == '"':
            i += 1
            while i < n and text[i] != '"':
                if text[i] == "\\":
                    out[i] = " "
                    i += 1
                if i < n and text[i] != "\n":
                    out[i] = " "
                i += 1
            i += 1
            continue
        # Char literal or lifetime. A lifetime is `'` + ident with no closing
        # quote immediately after one character.
        if ch == "'":
            lifetime = re.match(r"'(?:[A-Za-z_][A-Za-z0-9_]*)(?!')", text[i:])
            if lifetime:
                i += lifetime.end()
                continue
            i += 1
            while i < n and text[i] != "'":
                if text[i] == "\\":
                    out[i] = " "
                    i += 1
                if i < n and text[i] != "\n":
                    out[i] = " "
                i += 1
            i += 1
            continue
        i += 1
    return "".join(out)


def production_lines(text: str) -> list[tuple[int, str, str]]:
    """Yield `(lineno, raw_line, code_line)` for lines outside `#[cfg(test)]`.

    The cfg-test skip is a brace counter, not a parser, and the docstring says
    so out loud: it is right far more often than it is wrong, and its failure
    direction (counting a test line as debt) inflates the baseline rather than
    letting debt hide.
    """
    raw_lines = text.splitlines()
    code_text = blank_comments_and_strings(text)
    code_lines = code_text.splitlines()
    kept: list[tuple[int, str, str]] = []
    skip_stack: list[int] = []
    pending_cfg_test = False

    for index, raw in enumerate(raw_lines):
        code = code_lines[index] if index < len(code_lines) else ""
        depth = sum(skip_stack)
        if depth == 0:
            if pending_cfg_test and ITEM_START.match(code):
                delta = code.count("{") - code.count("}")
                if delta > 0:
                    skip_stack.append(delta)
                pending_cfg_test = False
                continue
            if pending_cfg_test and code.strip() and not code.strip().startswith("#"):
                pending_cfg_test = False
            if CFG_TEST_ATTR.search(code) and CFG_PREDICATE_TEST.search(code):
                pending_cfg_test = True
                continue
            kept.append((index + 1, raw, code))
        else:
            skip_stack[-1] += code.count("{") - code.count("}")
            if skip_stack[-1] <= 0:
                skip_stack.pop()
    return kept


def marker_reason(raw_line: str, previous_raw: str | None) -> str | None:
    """The reason from a trailing marker, else from an immediately preceding one."""
    for candidate in (raw_line, previous_raw):
        if not candidate:
            continue
        hit = MARKER.search(candidate)
        if hit:
            reason = hit.group("reason").strip().rstrip("*/").strip()
            if len(reason) >= REASON_MIN_CHARS:
                return reason
    return None


def scan() -> list[dict[str, Any]]:
    hits: list[dict[str, Any]] = []
    for path in production_files():
        raw_lines = path.read_text(encoding="utf-8", errors="ignore").splitlines()
        for lineno, raw, code in production_lines(path.read_text(encoding="utf-8", errors="ignore")):
            kinds = [name for name, pattern in PATTERNS if pattern.search(code)]
            if not kinds:
                continue
            previous = raw_lines[lineno - 2] if lineno >= 2 else None
            reason = marker_reason(raw, previous)
            hits.append(
                {
                    "path": path.relative_to(REPO_ROOT).as_posix(),
                    "line": lineno,
                    "kinds": kinds,
                    "justified": reason is not None,
                    "reason": reason or "",
                    "source": raw.strip(),
                }
            )
    return hits


# ── baseline and gate ───────────────────────────────────────────────────────


def summarise(hits: Iterable[dict[str, Any]]) -> tuple[dict[str, int], dict[str, int]]:
    debt: dict[str, int] = {}
    justified: dict[str, int] = {}
    for hit in hits:
        bucket = justified if hit["justified"] else debt
        bucket[hit["path"]] = bucket.get(hit["path"], 0) + 1
    return debt, justified


def load_baseline(path: Path) -> dict[str, Any]:
    if not path.exists():
        raise SystemExit(f"error: no baseline at {path} — create one with --update")
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict) or not isinstance(data.get("debt_files"), dict):
        raise SystemExit(f"error: malformed baseline: {path}")
    return data


def write_baseline(path: Path, debt: dict[str, int], justified: dict[str, int]) -> None:
    payload = {
        "version": 1,
        "note": (
            "Ratcheting panic-prone budget. debt_* is hard-gated (may only shrink). "
            "justified_* counts lines carrying a `panic-ok: <reason>` marker and may grow."
        ),
        "debt_total": sum(debt.values()),
        "debt_files": dict(sorted(debt.items())),
        "justified_total": sum(justified.values()),
        "justified_files": dict(sorted(justified.items())),
    }
    path.write_text(json.dumps(payload, indent=2, sort_keys=False) + "\n", encoding="utf-8")


def main() -> int:
    args = parse_args()
    hits = scan()
    debt, justified = summarise(hits)

    if args.list:
        for hit in hits:
            verdict = "justified" if hit["justified"] else "debt"
            kinds = ",".join(hit["kinds"])
            print(f"{hit['path']}:{hit['line']}: {verdict} [{kinds}] {hit['source']}")
        print(
            f"\ntotal={len(hits)} debt={sum(debt.values())} justified={sum(justified.values())}",
            file=sys.stderr,
        )
        return 0

    if args.update:
        write_baseline(args.baseline, debt, justified)
        print(
            f"baseline updated: debt={sum(debt.values())} justified={sum(justified.values())} "
            f"files={len(debt)}/{len(justified)} -> {args.baseline}"
        )
        return 0

    baseline = load_baseline(args.baseline)
    old_debt: dict[str, int] = baseline["debt_files"]
    old_justified: dict[str, int] = baseline.get("justified_files", {})
    regressions: list[str] = []
    improvements: list[str] = []

    current_total = sum(debt.values())
    baseline_total = baseline.get("debt_total", sum(old_debt.values()))
    if current_total > baseline_total:
        regressions.append(f"debt total grew: {baseline_total} -> {current_total}")
    elif current_total < baseline_total:
        improvements.append(f"debt total shrank: {baseline_total} -> {current_total}")

    for path, count in sorted(debt.items()):
        old = old_debt.get(path)
        if old is None:
            regressions.append(f"new file carrying panic-prone debt: {path} ({count})")
        elif count > old:
            regressions.append(f"debt grew: {path} ({old} -> {count})")
        elif count < old:
            improvements.append(f"debt shrank: {path} ({old} -> {count})")

    for path, old in sorted(old_debt.items()):
        if path not in debt:
            improvements.append(f"debt cleared: {path} ({old} -> 0)")

    # Justified growth is reported, never fatal: marking is a deliberate act
    # that a reviewer sees in the diff. Hiding it would push people to stop
    # marking and start renaming instead.
    new_marks = [
        f"{path} ({old_justified.get(path, 0)} -> {count})"
        for path, count in sorted(justified.items())
        if count > old_justified.get(path, 0)
    ]

    if regressions:
        print("PANIC BUDGET EXCEEDED:", file=sys.stderr)
        for entry in regressions:
            print(f"  - {entry}", file=sys.stderr)
        print(
            "Either remove the construct, or — if it is genuinely deliberate — "
            "annotate it: // panic-ok: <why it cannot fail>",
            file=sys.stderr,
        )
        return 1

    if new_marks:
        print("new justified marks (allowed, but each needs review in the diff):")
        for entry in new_marks:
            print(f"  - {entry}")
    for entry in improvements:
        print(f"  + {entry}")
    print(
        f"panic budget OK: debt={current_total} justified={sum(justified.values())} "
        f"(baseline debt={baseline_total})"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
