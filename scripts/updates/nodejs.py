#!/usr/bin/env python3
"""Node.js: the newest LTS major.

Node only builds the frontend. Minor and patch releases already float
within the pinned major (`node:24-alpine`, `.nvmrc`'s `24`), so only a new
LTS major produces a PR. CI reads `frontend/.nvmrc`, so the workflows
don't name a version.
"""

from __future__ import annotations

import sys

from common import (
    Changes,
    Edit,
    Proposal,
    UpdateError,
    docker_tag_exists,
    fetch_json,
    files_list,
    find_one,
    parser,
    run,
    warn,
)

INDEX_URL = "https://nodejs.org/dist/index.json"


def latest_lts(index=None) -> tuple[int, str, str]:
    """(major, full version, LTS codename) of the newest LTS release."""
    index = fetch_json(INDEX_URL) if index is None else index
    best = None
    for release in index:
        if not release.get("lts"):
            continue
        version = release["version"].lstrip("v")
        key = tuple(int(p) for p in version.split("."))
        if best is None or key > best[0]:
            best = (key, version, release["lts"])
    if best is None:
        raise UpdateError(f"{INDEX_URL} lists no LTS release")
    return best[0][0], best[1], best[2]


def edits(major: int) -> list[Edit]:
    return [
        Edit("server/Dockerfile", r"\bnode:\d+-alpine\b", f"node:{major}-alpine"),
        Edit("frontend/.nvmrc", r"\A\d+\n?\Z", f"{major}\n"),
        Edit("dev/docker-compose.yml", r"\bimage: node:\d+-slim\b", f"image: node:{major}-slim"),
    ]


def check(args, index=None, image_exists=docker_tag_exists) -> Proposal | None:
    changes = Changes(args.root)
    current = int(find_one(changes.read("server/Dockerfile"), r"\bnode:(\d+)-alpine\b", "the Node.js image"))
    major, full, codename = latest_lts(index)
    print(f"Node.js: pinned {current}, newest LTS {major} ({full}, {codename})")
    if major <= current:
        print("up to date")
        return None
    for tag in (f"{major}-alpine", f"{major}-slim"):
        if not image_exists("node", tag):
            warn(f"Node.js {major} is LTS but node:{tag} isn't on Docker Hub yet; not proposing it")
            return None

    all_edits = edits(major)
    for e in all_edits:
        changes.apply(e)
    body = f"""Node.js **{current} → {major}**: {major} is now an LTS line ("{codename}", latest {full},
from `{INDEX_URL}`). Minor and patch releases float within the major, so
this PR only appears for a new LTS major.

- Release notes: https://nodejs.org/en/blog/release/v{full}
- Changelog for the major: https://github.com/nodejs/node/blob/main/doc/changelogs/CHANGELOG_V{major}.md

Changed:

{files_list(sorted({e.path for e in all_edits}))}

CI's frontend job reads `frontend/.nvmrc`; the server image builds the
frontend with `node:{major}-alpine`.

Also consider, on this branch: `@types/node` to `^{major}` in
`frontend/package.json` (`npm install -D @types/node@{major}` in `frontend/`).
Dependabot leaves its major alone so it moves with Node.

Opened by the weekly dependency check (`.github/workflows/dependency-updates.yml`).
Review and merge by hand.
"""
    return Proposal(title=f"Update Node.js to {major} LTS", body=body, changes=changes, version=str(major))


if __name__ == "__main__":
    sys.exit(run(check, parser(__doc__).parse_args()))
