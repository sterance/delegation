#!/usr/bin/env python3
"""The Debian base image: an issue, not a PR, when Debian's stable release
is newer than the one the images are built on.

Moving the base changes glibc and the system libraries that the downloaded
llama.cpp builds load, so it needs a deliberate check rather than a weekly
bump.
"""

from __future__ import annotations

import re
import subprocess
import sys

from common import (
    Changes,
    NotFound,
    Proposal,
    UpdateError,
    docker_tag_exists,
    fetch_text,
    files_list,
    find_one,
    parser,
    run,
)

DISTS = "https://deb.debian.org/debian/dists"


def release_info(suite: str, get=fetch_text) -> tuple[str, int] | None:
    """(codename, major version) of a suite ("stable" or a codename), or
    None if the mirror no longer has it (it's older than oldstable)."""
    try:
        text = get(f"{DISTS}/{suite}/Release")
    except NotFound:
        return None
    codename = re.search(r"^Codename:\s*(\S+)", text, re.MULTILINE)
    version = re.search(r"^Version:\s*(\d+)", text, re.MULTILINE)
    if not codename or not version:
        raise UpdateError(f"{DISTS}/{suite}/Release has no Codename or Version")
    return codename.group(1), int(version.group(1))


def files_naming(root, codename: str) -> list[str]:
    """Tracked files that mention the codename."""
    try:
        out = subprocess.run(
            ["git", "grep", "-l", "-w", codename, "--", ".", ":!docs/*-spec.md"],
            cwd=root,
            capture_output=True,
            text=True,
            check=False,
        )
        return [p for p in out.stdout.splitlines() if p]
    except FileNotFoundError:
        return ["client/Dockerfile", "server/Dockerfile"]


def check(args, get=fetch_text, image_exists=docker_tag_exists) -> Proposal | None:
    changes = Changes(args.root)
    dockerfiles = changes.read("client/Dockerfile") + changes.read("server/Dockerfile")
    current = find_one(dockerfiles, r"\bdebian:(\w+)-slim\b", "the Debian base image")
    rust_suffix = find_one(dockerfiles, r"\brust:[\d.]+-slim-(\w+)", "the Rust image's Debian release")
    if rust_suffix != current:
        raise UpdateError(f"the Rust image is on {rust_suffix} but the runtime image on {current}")
    rust = find_one(dockerfiles, r"\brust:([\d.]+)-slim-", "the Rust image")

    stable = release_info("stable", get)
    if stable is None:
        raise UpdateError(f"{DISTS}/stable/Release is missing")
    new, new_major = stable
    print(f"Debian: images on {current}, stable is {new} ({new_major})")
    if new == current:
        print("up to date")
        return None
    mine = release_info(current, get)
    if mine is not None and mine[1] >= new_major:
        print(f"{current} ({mine[1]}) isn't older than stable; nothing to do")
        return None

    rust_image = f"rust:{rust}-slim-{new}"
    rust_note = (
        f"`{rust_image}` exists on Docker Hub."
        if image_exists("rust", f"{rust}-slim-{new}")
        else f"`{rust_image}` isn't on Docker Hub (yet); the Rust image may need a newer Rust first."
    )
    status = (
        f"{current} is Debian {mine[1]}, now oldstable or older"
        if mine
        else f"{current} is no longer on the Debian mirrors"
    )
    body = f"""Debian's stable release is now **{new}** (Debian {new_major}); the Docker
images are built on **{current}** ({status}).

This is an issue, not a PR, on purpose: moving the base image changes glibc
and the system libraries that the downloaded llama.cpp builds depend on, so
it needs a deliberate check, not a weekly bump. llama-server needs:

- glibc 2.34 or newer, and GCC 12's libstdc++ or newer;
- `libssl3` and `libgomp1` (check the package names: Debian renames
  libraries across releases, e.g. for the 64-bit time transition);
- the Vulkan loader and Mesa drivers (`libvulkan1`, `mesa-vulkan-drivers`)
  for the Vulkan image.

## To move to {new}

1. Change `debian:{current}-slim` and the `-{current}` suffix of the Rust
   images to `{new}`. {rust_note}
2. Check every apt package in `client/Dockerfile` and `server/Dockerfile`
   still exists under that name in {new}.
3. Build both images, then on the client image: run a model on the CPU
   build, on CUDA (an NVIDIA machine with the NVIDIA Container Toolkit,
   `client/docker-compose.gpu.yml`) and on Vulkan
   (`client/docker-compose.amd.yml`); and run the server image.
4. Update the comments and docs that name {current}.

Files that mention `{current}`:

{files_list(files_naming(args.root, current))}

Opened by the weekly dependency check (`.github/workflows/dependency-updates.yml`).
It updates this issue while {current} is behind; close it when the move is done.
"""
    return Proposal(title=f"Move the Docker images from Debian {current} to {new}", body=body, version=new)


if __name__ == "__main__":
    sys.exit(run(check, parser(__doc__).parse_args()))
