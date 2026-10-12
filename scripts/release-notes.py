#!/usr/bin/env python3
"""The note a release ships with: what CHANGELOG.md has gained since the release before.

The changelog is already written for somebody to read and updated with every change, so the
entry is the note, and nothing else needs keeping in step with it. What a release added is
whatever CHANGELOG.md holds now that it did not hold at the previous release's tag, compared
whole rather than diffed, so that an entry wrapped differently later does not come back as new
and the `### Internal` headings stay visible. Internal entries, the build, the tests, CI, the
deploy, stay in the changelog and out of the note. Each entry is cut to its first sentence,
which is what an entry is written to lead with.

    release-notes.py [previous tag]   the note, on stdout

With no previous tag the newest section stands in. A changelog that gained no entry, or none
at all, gives no note: the release goes out with an empty one, and stderr says why. From
gcloud-ovh-migrate's service template, word for word the same in every service.
"""
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CHANGELOG = "CHANGELOG.md"
INTERNAL = "internal"
# A release of nothing but internals says so: an empty note reads as a mistake.
INTERNALS_ONLY = ("- Internals only — the build, the tests or the deploy. "
                  "Nothing new to notice from outside.")


def entries(text):
    """Every bullet in a changelog, as (text, internal?) in the order written.

    A bullet runs until the next bullet, heading or blank line; its indented lines, nested
    bullets among them, belong to it. It is internal when the nearest `###` heading above it,
    within its section, says so.
    """
    found, holding, internal = [], None, False
    for line in text.splitlines():
        if line.startswith("## "):
            internal = False
        elif line.startswith("### "):
            internal = line[4:].strip().lower() == INTERNAL
        if line.startswith("- "):
            if holding:
                found.append(holding)
            holding = [line[2:].strip(), internal]
        elif holding and line.startswith("  ") and line.strip():
            holding[0] += " " + line.strip()
        elif not line.strip() or line.startswith("#"):
            if holding:
                found.append(holding)
            holding = None
    if holding:
        found.append(holding)
    return [(text, internal) for text, internal in found]


def newest_section(text):
    """The first `## ` section, for when there is no release to compare with."""
    kept, seen = [], False
    for line in text.splitlines():
        if line.startswith("## "):
            if seen:
                break
            seen = True
        if seen:
            kept.append(line)
    return "\n".join(kept)


def headline(text):
    """The first sentence, which is what an entry is written to lead with."""
    text = re.sub(r"\s*\(#\d+\)", "", text)          # the issue it closes
    text = re.sub(r"\*\*(.+?)\*\*", r"\1", text)     # what the entry leads with
    text = text.replace("`", "")
    end = re.search(r"\.(?=\s+[\"“„(]*[A-Z])|\.$", text)
    return (text[:end.end()] if end else text).strip()


def at(tag):
    """CHANGELOG.md as the tag has it: empty when the tag, or the file in it, is missing."""
    shown = subprocess.run(["git", "-C", ROOT, "show", f"{tag}:{CHANGELOG}"],
                           capture_output=True, text=True)
    return shown.stdout if shown.returncode == 0 else ""


def main():
    previous = sys.argv[1] if len(sys.argv) > 1 else ""
    try:
        with open(os.path.join(ROOT, CHANGELOG)) as f:
            now = f.read()
    except FileNotFoundError:
        print(f"no {CHANGELOG} here: the release goes out with no note", file=sys.stderr)
        return

    since = at(previous) if previous else ""
    if since:
        already = {text for text, _ in entries(since)}
        fresh = [(text, internal) for text, internal in entries(now) if text not in already]
    else:
        fresh = entries(newest_section(now))

    lines, said = [], set()
    for text, internal in fresh:
        line = headline(text)
        if not internal and line and line not in said:
            said.add(line)
            lines.append("- " + line)
    if not lines and fresh:
        lines = [INTERNALS_ONLY]
    if not lines:
        what = f"since {previous}" if since else "in its newest section"
        print(f"{CHANGELOG} has no new entry {what}: the release goes out with no note",
              file=sys.stderr)
        return
    print("\n".join(lines))


if __name__ == "__main__":
    main()
