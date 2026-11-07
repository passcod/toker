#!/usr/bin/env python3
"""Check every markdown link and heading anchor in the specs resolves.

A deep link into a heading breaks silently when the heading is reworded, so
this is what makes the precision affordable. Offline: it reads the tree only.
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent / ".workhorse" / "specs"
LINK = re.compile(r"\]\(([^)#]*\.md)(?:#([^)]*))?\)")
HEADING = re.compile(r"^#{1,6}\s+(.*)$")


def slugs(path):
    out = set()
    for line in path.read_text().splitlines():
        m = HEADING.match(line)
        if m:
            s = re.sub(r"[^a-z0-9 -]", "", m.group(1).lower()).strip()
            out.add(s.replace(" ", "-"))
    return out


def main():
    if not ROOT.is_dir():
        sys.exit(f"no specs at {ROOT}")
    problems = []
    ids = {}
    for spec in sorted(ROOT.rglob("*.md")):
        text = spec.read_text()
        here = spec.relative_to(ROOT)

        m = re.search(r"^id:\s*(\S+)\s*$", text, re.M)
        if not m:
            problems.append(f"{here}: no id in frontmatter")
        else:
            ids.setdefault(m.group(1), []).append(str(here))

        for target, anchor in LINK.findall(text):
            dest = (spec.parent / target).resolve()
            if not dest.exists():
                problems.append(f"{here}: link to missing {target}")
            elif anchor and anchor not in slugs(dest):
                problems.append(f"{here}: no heading {anchor} in {target}")

    for ident, where in sorted(ids.items()):
        if len(where) > 1:
            problems.append(f"id {ident} used by {', '.join(where)}")

    if problems:
        print("\n".join(problems))
        sys.exit(f"\n{len(problems)} problem(s)")
    print(f"{len(ids)} specs: every link, anchor and id checks out")


if __name__ == "__main__":
    main()
