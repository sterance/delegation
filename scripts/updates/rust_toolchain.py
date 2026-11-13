#!/usr/bin/env python3
"""The Rust toolchain: the current stable release, everywhere at once.

The project keeps its minimum supported Rust (`rust-version`) equal to the
toolchain it builds with, and the Docker images build with the same
version, so every place moves together. CI reads rust-toolchain.toml, so
the workflows don't name a version.
"""

from __future__ import annotations

import re
import sys
import tomllib

from common import (
    Changes,
    Edit,
    Proposal,
    UpdateError,
    docker_tag_exists,
    fetch_text,
    files_list,
    find_one,
    parser,
    run,
    version_tuple,
    warn,
)

CHANNEL_URL = "https://static.rust-lang.org/dist/channel-rust-stable.toml"
V = r"\d+\.\d+\.\d+"


def latest_stable() -> str:
    channel = tomllib.loads(fetch_text(CHANNEL_URL))
    try:
        full = channel["pkg"]["rust"]["version"]  # "1.100.0 (0ef1bc386 2026-11-09)"
    except KeyError as e:
        raise UpdateError(f"{CHANNEL_URL} has no pkg.rust.version") from e
    m = re.match(rf"({V})\b", full)
    if not m:
        raise UpdateError(f"unexpected Rust version {full!r} in {CHANNEL_URL}")
    return m.group(1)


def edits(new: str) -> list[Edit]:
    return [
        Edit("rust-toolchain.toml", rf'^channel = "{V}"$', f'channel = "{new}"'),
        *(
            Edit(f"{crate}/Cargo.toml", rf'^rust-version = "{V}"$', f'rust-version = "{new}"')
            for crate in ("client", "server", "protocol")
        ),
        *(
            Edit(f"{d}/Dockerfile", rf"\brust:{V}-slim-", f"rust:{new}-slim-")
            for d in ("client", "server")
        ),
        Edit("README.md", rf"(pinned in `rust-toolchain\.toml` \(){V}\)", rf"\g<1>{new})"),
    ]


def check(args, latest=latest_stable, image_exists=docker_tag_exists) -> Proposal | None:
    changes = Changes(args.root)
    current = find_one(changes.read("rust-toolchain.toml"), rf'^channel = "({V})"$', "the Rust toolchain")
    new = latest()
    print(f"Rust: pinned {current}, latest stable {new}")
    if version_tuple(new) <= version_tuple(current):
        print("up to date")
        return None

    codename = find_one(
        changes.read("client/Dockerfile") + changes.read("server/Dockerfile"),
        rf"\brust:{V}-slim-(\w+)",
        "the Rust image's Debian release",
    )
    image = f"rust:{new}-slim-{codename}"
    if not image_exists("rust", f"{new}-slim-{codename}"):
        # Images usually follow a release within a day; next week's run
        # will find it.
        warn(f"Rust {new} is out but {image} isn't on Docker Hub yet; not proposing it")
        return None

    all_edits = edits(new)
    for e in all_edits:
        changes.apply(e)
    paths = sorted({e.path for e in all_edits})
    body = f"""Rust **{current} → {new}** (current stable, from `{CHANNEL_URL}`).

- Release notes: https://github.com/rust-lang/rust/blob/main/RELEASES.md
- Docker image `{image}` exists on Docker Hub.

Every place the toolchain is pinned moves together; the minimum supported
Rust (`rust-version`) stays equal to the toolchain:

{files_list(paths)}

CI and the Windows build read `rust-toolchain.toml`.

**New Clippy lints can fail CI on this PR.** That's expected: fix them on
this branch (`cargo clippy --workspace --all-targets -- -D warnings`), in
this PR.

Opened by the weekly dependency check (`.github/workflows/dependency-updates.yml`).
Review and merge by hand.
"""
    return Proposal(title=f"Update Rust to {new}", body=body, changes=changes, version=new)


if __name__ == "__main__":
    sys.exit(run(check, parser(__doc__).parse_args()))
