#!/usr/bin/env python3
"""llama.cpp: move `client/runtime-pins.json` to the newest `b<N>` release.

A custom check of infra's weekly dependency updates (`.github/updates.yml`,
check `llama-cpp`; see README.md here). Run by infra with UPDATE_OUT set, it
edits the pins in the working tree and writes UPDATE_OUT/result.json and
body.md; infra makes the patch and opens the PR. UPDATE_DRY_RUN=1 skips the
downloads. Run by hand, it prints the diff instead (--dry-run) or writes it.

For every pinned archive, the new file name is the old one with the version
swapped. Its SHA-256 comes from the release API's asset digest *and* from
downloading it, and the two must match. Nothing is guessed: if a file is
missing or renamed, that variant stays on the old release and the problem
heads the PR description (or, for a CPU build, the check fails).
"""

from __future__ import annotations

import argparse
import copy
import difflib
import hashlib
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
# Generic on purpose: no project or personal details go to outside services.
USER_AGENT = "dependency-update-check (+https://docs.github.com/actions)"
TIMEOUT = 60


class UpdateError(Exception):
    """A check that can't complete: it exits non-zero and says why."""


def _request(url: str) -> urllib.request.Request:
    headers = {"User-Agent": USER_AGENT}
    # The workflow's read-only token, for the GitHub API's rate limit.
    token = os.environ.get("GITHUB_TOKEN")
    if token and url.startswith("https://api.github.com/"):
        headers["Authorization"] = f"Bearer {token}"
    return urllib.request.Request(url, headers=headers)


def fetch_json(url: str, tries: int = 3):
    last: Exception | None = None
    for attempt in range(tries):
        try:
            with urllib.request.urlopen(_request(url), timeout=TIMEOUT) as r:
                return json.loads(r.read())
        except json.JSONDecodeError as e:
            raise UpdateError(f"{url} didn't return JSON: {e}") from e
        except (urllib.error.URLError, TimeoutError, ConnectionError) as e:
            last = e
        if attempt + 1 < tries:
            time.sleep(2 * (attempt + 1))
    raise UpdateError(f"couldn't fetch {url}: {last}")


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
            raise UpdateError(f"{path} doesn't exist (has it moved? update scripts/updates/llama_cpp.py)") from e

    def set(self, path: str, text: str) -> None:
        self.files[path] = text

    def sub(self, path: str, pattern: str, replacement: str) -> None:
        """Replaces the one match of `pattern`, or fails."""
        new, n = re.subn(pattern, replacement, self.read(path), flags=re.MULTILINE)
        if n != 1:
            raise UpdateError(f"{path}: expected /{pattern}/ once, found it {n} time(s); update llama_cpp.py")
        self.files[path] = new

    def changed(self) -> dict[str, str]:
        out = {}
        for path, text in self.files.items():
            p = self.root / path
            if (p.read_text(encoding="utf-8") if p.exists() else "") != text:
                out[path] = text
        return out

    def diff(self) -> str:
        chunks = []
        for path, text in sorted(self.changed().items()):
            old = (self.root / path).read_text(encoding="utf-8")
            chunks.extend(difflib.unified_diff(old.splitlines(True), text.splitlines(True), f"a/{path}", f"b/{path}"))
        return "".join(chunks)

    def write(self) -> list[str]:
        written = sorted(self.changed())
        for path in written:
            (self.root / path).write_text(self.files[path], encoding="utf-8")
        return written


@dataclass
class Proposal:
    title: str
    body: str
    changes: Changes
    version: str

PINS = "client/runtime-pins.json"
REPO = "ggml-org/llama.cpp"
API = f"https://api.github.com/repos/{REPO}"
DOWNLOAD_PREFIX = f"https://github.com/{REPO}/releases/download/"
TAG = re.compile(r"^b(\d+)$")


def tag_number(tag: str) -> int:
    m = TAG.match(tag)
    if not m:
        raise UpdateError(f"{tag!r} isn't a llama.cpp build tag (b<number>)")
    return int(m.group(1))


def latest_release(get_json=fetch_json) -> dict:
    """The newest `b<N>` release. Not /releases/latest: the builds are
    published as prereleases, and "latest" is a separate versioned release
    without them."""
    releases = get_json(f"{API}/releases?per_page=50")
    builds = [r for r in releases if not r.get("draft") and TAG.match(r.get("tag_name", ""))]
    if not builds:
        raise UpdateError(f"no b<N> release among the newest 50 at {API}/releases")
    return max(builds, key=lambda r: tag_number(r["tag_name"]))


def download_sha256(url: str) -> tuple[str, int]:
    h = hashlib.sha256()
    size = 0
    with urllib.request.urlopen(_request(url), timeout=TIMEOUT) as r:
        while chunk := r.read(1 << 20):
            h.update(chunk)
            size += len(chunk)
    return h.hexdigest(), size


def similar_names(name: str, assets: dict) -> list[str]:
    """Assets whose names differ from `name` in exactly one number (a CUDA
    13.4 -> 13.5 bump, say). Suggestions for a human; never applied."""
    shape, numbers = re.sub(r"\d+", "#", name), re.findall(r"\d+", name)

    def one_number_apart(other: str) -> bool:
        theirs = re.findall(r"\d+", other)
        return (
            re.sub(r"\d+", "#", other) == shape
            and sum(a != b for a, b in zip(numbers, theirs)) == 1
        )

    return sorted(a for a in assets if one_number_apart(a))


@dataclass
class Result:
    pins: dict
    # (variant label, file, sha256, size, unchanged)
    updated: list[tuple[str, str, str, int, bool]] = field(default_factory=list)
    problems: list[str] = field(default_factory=list)
    stale_variants: list[str] = field(default_factory=list)


def update_pins(pins: dict, release: dict, verify=download_sha256) -> Result:
    """The pins moved to `release`, verifying each file. `verify(url)`
    downloads and hashes one file; None skips downloading (dry runs)."""
    old, new = pins["version"], release["tag_name"]
    assets = {a["name"]: a for a in release.get("assets", [])}
    out = Result(pins=copy.deepcopy(pins))
    out.pins["version"] = new
    old_prefix = f"{DOWNLOAD_PREFIX}{old}/"

    for variant in out.pins["variants"]:
        label = f"{variant['os']} {variant['arch']} {variant['variant']}"
        new_archives = []
        variant_problems = []
        for archive in variant["archives"]:
            if not archive["url"].startswith(old_prefix):
                raise UpdateError(f"{label}: {archive['url']} isn't a file of {old}")
            old_name = archive["url"][len(old_prefix):]
            name = old_name.replace(old, new)
            asset = assets.get(name)
            if asset is None:
                hint = similar_names(name, assets)
                variant_problems.append(
                    f"**{label}**: `{name}` isn't in {new}"
                    + (f" (similar: {', '.join(f'`{h}`' for h in hint)})" if hint else "")
                )
                continue
            digest = asset.get("digest") or ""
            if not digest.startswith("sha256:"):
                variant_problems.append(f"**{label}**: the release API gives no SHA-256 for `{name}`")
                continue
            sha, size = digest.removeprefix("sha256:"), asset["size"]
            url = f"{DOWNLOAD_PREFIX}{new}/{name}"
            unchanged = sha == archive["sha256"] and size == archive["size_bytes"]
            # A file with the hash already pinned was verified when it was
            # pinned (the Windows CUDA runtime is often unchanged).
            if verify is not None and not unchanged:
                print(f"  downloading {name} ({size / 1e6:.0f} MB)", flush=True)
                got_sha, got_size = verify(url)
                if (got_sha, got_size) != (sha, size):
                    raise UpdateError(
                        f"{name}: the download (sha256 {got_sha}, {got_size} bytes) doesn't match "
                        f"the release API (sha256 {sha}, {size} bytes); not pinning it"
                    )
            new_archives.append(
                {"url": url, "sha256": sha, "size_bytes": size, "format": archive["format"]}
            )
            out.updated.append((label, name, sha, size, unchanged))
        if variant_problems:
            if variant["variant"] == "cpu":
                raise UpdateError(
                    f"the CPU build (the fallback every client needs) can't be updated to {new}: "
                    + "; ".join(p.replace("**", "") for p in variant_problems)
                )
            out.problems.extend(variant_problems)
            out.stale_variants.append(label)
            # Left exactly as it was, on the old release.
            out.updated = [u for u in out.updated if u[0] != label]
        else:
            variant["archives"] = new_archives
    return out


def catalog_notes(catalog: Path | None, old: str, new: str) -> list[str]:
    """Catalog models that need a release in (old, new]."""
    if catalog is None:
        return []
    try:
        entries = json.loads(catalog.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as e:
        raise UpdateError(f"couldn't read the catalog {catalog}: {e}") from e
    lo, hi = tag_number(old), tag_number(new)
    notes = []
    for e in entries if isinstance(entries, list) else []:
        need = e.get("min_runtime_version")
        if isinstance(need, str) and TAG.match(need) and lo < tag_number(need) <= hi:
            notes.append(f"`{e.get('id', '?')}` needs {need}: this bump makes it installable.")
    return notes


GPU_CHECKLIST = """\
GitHub's runners have no GPU, so CI only runs the CPU build
(`runtime-smoke`). Before merging, on a machine with an NVIDIA GPU (and, if
you have one, an AMD or Intel GPU for Vulkan), with this branch's client:

- [ ] **CUDA**: install a model; the client picks the CUDA build, the log
      shows `llama.cpp cuda-… sees: <your GPU>`, and the dashboard shows the
      model's layers on the GPU.
- [ ] **CUDA**: chat with it; replies stream and report tokens/s.
- [ ] **Vulkan** (if available): the same, with the Vulkan build
      (select it from the dashboard if the client picks CUDA).
- [ ] **Windows** (if available): the same on the Windows tray build from
      this PR's artifacts.
"""


def body_for(old: str, new: str, result: Result, release: dict, notes: list[str], verified: bool) -> str:
    parts = []
    if result.problems:
        parts.append(
            "## ⚠ Assets that need a decision\n\n"
            + "\n".join(f"- {p}" for p in result.problems)
            + f"\n\nThese variants were **left on {old}** rather than guessed: "
            + ", ".join(result.stale_variants)
            + f". The client pins a single release, so its pins test (and CI) fails until each is "
            "fixed by hand on this branch: point it at the right file of "
            f"{new} (check the name and, for CUDA, `requires.cuda_driver`), with the asset's "
            "SHA-256 and size, or remove the variant.\n"
        )
    parts.append(
        f"llama.cpp **{old} → {new}**.\n\n"
        f"- Release: {release.get('html_url', f'https://github.com/{REPO}/releases/tag/{new}')}\n"
        f"- Changes: https://github.com/{REPO}/compare/{old}...{new}\n"
    )
    if notes:
        parts.append("## Catalog\n\n" + "\n".join(f"- {n}" for n in notes) + "\n")
    rows = "\n".join(
        f"| {label} | `{name}` | `{sha}` | {size:,}{' (unchanged)' if same else ''} |"
        for label, name, sha, size, same in result.updated
    )
    how = (
        "Each SHA-256 is the release API's digest, and matched a download of the file "
        "(files whose hash was already pinned weren't downloaded again)."
        if verified
        else "**Hashes are from the release API only (not downloaded): this was a dry run.**"
    )
    parts.append(f"## Assets\n\n{how}\n\n| Variant | File | SHA-256 | Bytes |\n|---|---|---|---|\n{rows}\n")
    parts.append("## Manual check (GPU)\n\n" + GPU_CHECKLIST)
    return "\n".join(parts)


def check(args, get_json=fetch_json, verify=download_sha256) -> Proposal | None:
    changes = Changes(args.root)
    pins = json.loads(changes.read(PINS))
    old = pins["version"]
    release = latest_release(get_json)
    new = release["tag_name"]
    print(f"llama.cpp: pinned {old}, newest build {new}")
    if tag_number(new) <= tag_number(old):
        print("up to date")
        return None

    download = verify if (args.verify_downloads or not args.dry_run) else None
    result = update_pins(pins, release, download)
    changes.set(PINS, json.dumps(result.pins, indent=2) + "\n")
    changes.sub("README.md", rf"(The release \(currently\s+`){re.escape(old)}`", rf"\g<1>{new}`")
    notes = catalog_notes(args.catalog, old, new)
    title = f"Update llama.cpp to {new}" + (" (some variants need attention)" if result.problems else "")
    body = body_for(old, new, result, release, notes, verified=download is not None)
    return Proposal(title=title, body=body, changes=changes, version=new)


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--dry-run", action="store_true", help="print the diff, change nothing, don't download")
    p.add_argument(
        "--verify-downloads",
        action="store_true",
        help="with --dry-run, still download every changed file and check its hash (several GB)",
    )
    p.add_argument("--catalog", type=Path, help="a MODEL_CATALOG_PATH file, to note models this release makes installable")
    p.add_argument("--root", type=Path, default=REPO_ROOT, help="the repository (default: this one)")
    args = p.parse_args(argv)
    out = os.environ.get("UPDATE_OUT")
    if out and os.environ.get("UPDATE_DRY_RUN") == "1":
        args.dry_run = True
    try:
        proposal = check(args)
    except UpdateError as e:
        print(f"::error::{e}" if os.environ.get("GITHUB_ACTIONS") else f"error: {e}", file=sys.stderr)
        return 1
    if out:
        # Infra's custom-check contract: edit the tree (even in a dry run, so the diff can be shown).
        result = {"result": "none"}
        if proposal is not None:
            for path in proposal.changes.write():
                print(f"wrote {path}")
            (Path(out) / "body.md").write_text(proposal.body, encoding="utf-8")
            result = {"result": "pr", "title": proposal.title, "version": proposal.version}
        (Path(out) / "result.json").write_text(json.dumps(result) + "\n", encoding="utf-8")
        return 0
    if proposal is None:
        return 0
    print(f"\n{proposal.title}\n")
    if args.dry_run:
        print(proposal.changes.diff() or "(no file changes)")
        print("\n---- description ----\n")
        print(proposal.body)
    else:
        for path in proposal.changes.write():
            print(f"wrote {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
