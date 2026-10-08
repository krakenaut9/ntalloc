#!/usr/bin/env python3
"""Fail if a public name of the crate is not mentioned in README.md.

The README's examples are compiled as doctests, all but those marked `ignore`,
so their code keeps up with the API. Its *coverage* still can fall behind: an
item added to the crate with no README mention is invisible to readers. This
closes that gap.

The public names are the top-level `pub` items of src/lib.rs, the names its
`pub use` lines re-export, the `pub` functions and constants of its `impl`
blocks, the macros it exports, and the features in Cargo.toml. A name counts as
mentioned only inside code, either an inline code span or a code block, so that
prose such as "allocate memory" cannot stand in for the function `allocate`.
Anything the script cannot enumerate, such as a glob re-export, fails the check
instead of passing unexamined.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LIB = ROOT / "src" / "lib.rs"
MANIFEST = ROOT / "Cargo.toml"
README = ROOT / "README.md"

# Qualifiers that may precede an item keyword: `pub const unsafe fn`,
# `pub unsafe extern "C" fn` and the like. `const` is also an item keyword of its
# own; the regex engine backtracks to that reading for `pub const NAME: T`.
QUALIFIERS = r'(?:(?:const|unsafe|async|extern(?:\s+"[^"]*")?)\s+)*'
ITEM = r"(?:fn|const|static(?:\s+mut)?|struct|enum|union|trait|type|mod)"
TOP_LEVEL = re.compile(rf"^pub\s+{QUALIFIERS}{ITEM}\s+([A-Za-z_]\w*)", re.M)
# rustfmt indents the items of a top-level `impl` block by four spaces.
ASSOCIATED = re.compile(rf"^    pub\s+{QUALIFIERS}(?:fn|const)\s+([A-Za-z_]\w*)", re.M)
MACRO = re.compile(
    r"^#\[macro_export\][^\n]*\n(?:[ \t]*#\[[^\n]*\n)*[ \t]*macro_rules!\s+([A-Za-z_]\w*)",
    re.M,
)
REEXPORT = re.compile(r"^pub\s+use\s+([^;]+);", re.M)
FENCE = re.compile(r"^```[^\n]*\n(.*?)^```", re.M | re.S)
SPAN = re.compile(r"`([^`\n]+)`")


class Unlistable(Exception):
    """A public surface the script cannot enumerate, such as a glob re-export."""


def split_top_level(text: str) -> list[str]:
    """Split `a, b::{c, d}, e` at the commas outside any braces."""
    parts, depth, current = [], 0, []
    for ch in text:
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
        if ch == "," and depth == 0:
            parts.append("".join(current))
            current = []
        else:
            current.append(ch)
    parts.append("".join(current))
    return [p.strip() for p in parts if p.strip()]


def use_tree_names(tree: str, prefix: str = "") -> list[str]:
    """The names a `use` tree binds: `a::{b, c::{d as e, self}}` binds b, e and c."""
    tree = " ".join(tree.split())
    group = re.match(r"^(.*?)\{(.*)\}$", tree)
    if group:
        head = prefix + group.group(1)
        names = []
        for part in split_top_level(group.group(2)):
            names += use_tree_names(part, head)
        return names
    if tree.endswith("*"):
        raise Unlistable(f"glob re-export `pub use {prefix}{tree}`: list its names")
    alias = re.search(r"\bas\s+(\w+)$", tree)
    if alias:
        return [alias.group(1)]
    last = tree.split("::")[-1].strip()
    if last == "self":
        return [prefix.rstrip(": ").split("::")[-1]]
    return [last]


def reexported(lib: str) -> list[str]:
    names = []
    for tree in REEXPORT.findall(lib):
        names += use_tree_names(tree)
    return names


def features(manifest: str) -> list[str]:
    section = re.search(r"^\[features\]\n(.*?)(?=^\[|\Z)", manifest, re.M | re.S)
    if not section:
        return []
    return re.findall(r"^([A-Za-z0-9_-]+)\s*=", section.group(1), re.M)


def code_text(readme: str) -> str:
    blocks = FENCE.findall(readme)
    prose = FENCE.sub("", readme)
    return "\n".join(blocks + SPAN.findall(prose))


def public_names(lib: str, manifest: str) -> dict[str, list[str]]:
    def named(names):
        # `_` binds nothing a reader could look up (`pub use Trait as _`).
        return sorted({n for n in names if n != "_"})

    return {
        "item": named(TOP_LEVEL.findall(lib)),
        "re-export": named(reexported(lib)),
        "associated": named(ASSOCIATED.findall(lib)),
        "macro": named(f"{m}!" for m in MACRO.findall(lib)),
        "feature": named(features(manifest)),
    }


def main() -> int:
    lib = LIB.read_text(encoding="utf-8")
    code = code_text(README.read_text(encoding="utf-8"))
    try:
        groups = public_names(lib, MANIFEST.read_text(encoding="utf-8"))
    except Unlistable as e:
        print(f"Cannot check README coverage: {e}", file=sys.stderr)
        return 1

    # A Rust name ends at any character that can continue an identifier; a
    # feature name ends at one that can continue a feature name, so that
    # `allocator-api-markers` does not count as a mention of `allocator-api`.
    edge = {"feature": r"[\w-]"}

    rows, missing = [], []
    for kind, names in groups.items():
        e = edge.get(kind, r"\w")
        for name in names:
            found = re.search(rf"(?<!{e}){re.escape(name)}(?!{e})", code)
            rows.append((kind, name, bool(found)))
            if not found:
                missing.append(name)

    print(f"{len(rows)} public names, {len(missing)} missing from README.md")
    for kind, name, found in rows:
        print(f"  {'ok' if found else 'MISSING':>7}  {kind:<10}  {name}")

    # The crate has always had these; finding none means a pattern stopped
    # matching the sources, not that the API shrank to nothing.
    empty = [k for k in ("item", "re-export", "associated", "feature") if not groups[k]]
    if empty:
        print(f"\nFound no {', no '.join(empty)}: the patterns no longer match "
              "the sources", file=sys.stderr)
        return 1
    if missing:
        print(f"\nNot mentioned in README.md: {', '.join(missing)}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
