"""Shared helpers for the weekly dependency checks (see
scripts/updates/README.md).

Every check script works the same way:

1. find the latest version from an authoritative source,
2. compare it with the pinned one, stopping if there's nothing newer,
3. rewrite every place the dependency is pinned (or, with --dry-run, print
   the diff instead),
4. leave a pull request title and body in --out for the workflow, and
   `changed=true` in $GITHUB_OUTPUT.

Anything unexpected (an API that doesn't answer, a pin that isn't where it
should be) raises `UpdateError`, which exits non-zero, so the workflow run
shows the failure. Standard library only, so the scripts run anywhere
Python 3.11+ does.
"""

from __future__ import annotations

import argparse
import difflib
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

REPO_ROOT = Path(__file__).resolve().parents[2]
# Generic on purpose: no project or personal details go to outside services.
USER_AGENT = "dependency-update-check (+https://docs.github.com/actions)"
TIMEOUT = 60


class UpdateError(Exception):
    """A check that can't complete: the job fails and says why."""


# ---------------- fetching ----------------


def _request(url: str, accept: str | None = None) -> urllib.request.Request:
    headers = {"User-Agent": USER_AGENT}
    if accept:
        headers["Accept"] = accept
    # The GitHub API allows 60 unauthenticated requests an hour per IP,
    # which shared runners exceed; the workflow's own token lifts that.
    token = os.environ.get("GITHUB_TOKEN")
    if token and url.startswith("https://api.github.com/"):
        headers["Authorization"] = f"Bearer {token}"
    return urllib.request.Request(url, headers=headers)


def fetch(url: str, accept: str | None = None, tries: int = 3) -> bytes:
    """GETs `url`, retrying transient failures. A 404 is not retried."""
    last: Exception | None = None
    for attempt in range(tries):
        try:
            with urllib.request.urlopen(_request(url, accept), timeout=TIMEOUT) as r:
                return r.read()
        except urllib.error.HTTPError as e:
            if e.code == 404:
                raise NotFound(url) from e
            last = e
        except (urllib.error.URLError, TimeoutError, ConnectionError) as e:
            last = e
        if attempt + 1 < tries:
            time.sleep(2 * (attempt + 1))
    raise UpdateError(f"couldn't fetch {url}: {last}")


def fetch_json(url: str, accept: str | None = None):
    try:
        return json.loads(fetch(url, accept))
    except json.JSONDecodeError as e:
        raise UpdateError(f"{url} didn't return JSON: {e}") from e


def fetch_text(url: str, accept: str | None = None) -> str:
    return fetch(url, accept).decode("utf-8")


class NotFound(UpdateError):
    def __init__(self, url: str):
        super().__init__(f"{url}: not found (404)")
        self.url = url


def exists(url: str, accept: str | None = None) -> bool:
    try:
        fetch(url, accept)
        return True
    except NotFound:
        return False


def docker_tag_exists(repository: str, tag: str) -> bool:
    """Whether `repository:tag` (an official image, e.g. "rust") is on
    Docker Hub."""
    return exists(f"https://hub.docker.com/v2/repositories/library/{repository}/tags/{tag}")


def version_tuple(v: str) -> tuple[int, ...]:
    """"1.100.0" -> (1, 100, 0). Raises UpdateError for anything else."""
    if not re.fullmatch(r"\d+(\.\d+)*", v):
        raise UpdateError(f"{v!r} isn't a numeric version")
    return tuple(int(p) for p in v.split("."))


# ---------------- rewriting ----------------


@dataclass
class Edit:
    """One pinned place: `pattern` must match in `path` exactly `count`
    times (or at least once when count is None), and each match is
    replaced by `replacement` (re.sub syntax)."""

    path: str
    pattern: str
    replacement: str | Callable[[re.Match], str]
    count: int | None = 1
    flags: int = re.MULTILINE


@dataclass
class Changes:
    """New file contents, keyed by repo-relative path."""

    root: Path
    files: dict[str, str] = field(default_factory=dict)

    def read(self, path: str) -> str:
        if path in self.files:
            return self.files[path]
        try:
            return (self.root / path).read_text(encoding="utf-8")
        except FileNotFoundError as e:
            raise UpdateError(f"{path} doesn't exist (has it moved? update scripts/updates/)") from e

    def apply(self, edit: Edit) -> None:
        text = self.read(edit.path)
        new, n = re.subn(edit.pattern, edit.replacement, text, flags=edit.flags)
        if (edit.count is None and n == 0) or (edit.count is not None and n != edit.count):
            want = "at least once" if edit.count is None else f"{edit.count} time(s)"
            raise UpdateError(
                f"{edit.path}: expected /{edit.pattern}/ {want}, found it {n} time(s); "
                "the pin has moved or changed shape, so update scripts/updates/"
            )
        self.files[edit.path] = new

    def set(self, path: str, text: str) -> None:
        self.files[path] = text

    def changed(self) -> dict[str, str]:
        """Only the files whose contents differ from disk."""
        out = {}
        for path, text in self.files.items():
            p = self.root / path
            old = p.read_text(encoding="utf-8") if p.exists() else ""
            if old != text:
                out[path] = text
        return out

    def diff(self) -> str:
        chunks = []
        for path, text in sorted(self.changed().items()):
            p = self.root / path
            old = p.read_text(encoding="utf-8") if p.exists() else ""
            chunks.extend(
                difflib.unified_diff(
                    old.splitlines(keepends=True),
                    text.splitlines(keepends=True),
                    fromfile=f"a/{path}",
                    tofile=f"b/{path}",
                )
            )
        return "".join(chunks)

    def write(self) -> list[str]:
        written = []
        for path, text in sorted(self.changed().items()):
            (self.root / path).write_text(text, encoding="utf-8")
            written.append(path)
        return written


def find_one(text: str, pattern: str, what: str) -> str:
    """The first group of the only match of `pattern`."""
    found = re.findall(pattern, text, flags=re.MULTILINE)
    if not found:
        raise UpdateError(f"couldn't find {what} (/{pattern}/)")
    values = set(found)
    if len(values) > 1:
        raise UpdateError(f"{what} disagrees with itself: {sorted(values)}")
    return found[0]


# ---------------- results ----------------


@dataclass
class Proposal:
    """What a check proposes: a PR (`changes`) or an issue (no changes)."""

    title: str
    body: str
    changes: Changes | None = None
    version: str = ""


def parser(description: str) -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description=description)
    p.add_argument(
        "--dry-run",
        action="store_true",
        help="print what would change instead of writing it",
    )
    p.add_argument(
        "--out",
        type=Path,
        help="directory for title.txt and body.md (the PR or issue text)",
    )
    p.add_argument("--root", type=Path, default=REPO_ROOT, help="the repository (default: this one)")
    return p


def set_output(name: str, value: str) -> None:
    path = os.environ.get("GITHUB_OUTPUT")
    if path:
        with open(path, "a", encoding="utf-8") as f:
            f.write(f"{name}={value}\n")


def finish(proposal: Proposal | None, args: argparse.Namespace) -> None:
    """Prints, writes and reports a check's result."""
    if proposal is None:
        set_output("changed", "false")
        return
    print(f"\n{proposal.title}\n")
    if proposal.changes is not None:
        if args.dry_run:
            print(proposal.changes.diff() or "(no file changes)")
        else:
            for path in proposal.changes.write():
                print(f"wrote {path}")
    if args.dry_run:
        print("\n---- description ----\n")
        print(proposal.body)
    if args.out:
        args.out.mkdir(parents=True, exist_ok=True)
        (args.out / "title.txt").write_text(proposal.title + "\n", encoding="utf-8")
        (args.out / "body.md").write_text(proposal.body, encoding="utf-8")
    set_output("changed", "true")
    set_output("version", proposal.version)
    set_output("title", proposal.title)


def run(main: Callable[[argparse.Namespace], Proposal | None], args: argparse.Namespace) -> int:
    try:
        finish(main(args), args)
        return 0
    except UpdateError as e:
        # An annotation, so the run's summary shows the reason.
        print(f"::error::{e}" if os.environ.get("GITHUB_ACTIONS") else f"error: {e}", file=sys.stderr)
        return 1


def warn(message: str) -> None:
    print(f"::warning::{message}" if os.environ.get("GITHUB_ACTIONS") else f"warning: {message}")


def files_list(paths: list[str]) -> str:
    return "\n".join(f"- `{p}`" for p in paths)
