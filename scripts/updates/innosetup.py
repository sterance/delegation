#!/usr/bin/env python3
"""Inno Setup: the newest release of the Chocolatey `innosetup` package.

The Windows build installs exactly the version in
`installer/inno-setup-version` (windows.yml), so a signed release is
reproducible. The PR's Windows build in CI is the test.
"""

from __future__ import annotations

import re
import sys
import xml.etree.ElementTree as ET

from common import Changes, Proposal, UpdateError, fetch, parser, run, version_tuple

PIN = "installer/inno-setup-version"
PACKAGE = "innosetup"
FEED = f"https://community.chocolatey.org/api/v2/FindPackagesById()?id='{PACKAGE}'"
NS = {
    "a": "http://www.w3.org/2005/Atom",
    "d": "http://schemas.microsoft.com/ado/2007/08/dataservices",
    "m": "http://schemas.microsoft.com/ado/2007/08/dataservices/metadata",
}
MAX_PAGES = 20


def parse_feed(xml: bytes) -> tuple[list[tuple[str, bool]], str | None]:
    """(version, is prerelease) per entry, and the next page's URL."""
    try:
        root = ET.fromstring(xml)
    except ET.ParseError as e:
        raise UpdateError(f"the Chocolatey feed isn't XML: {e}") from e
    out = []
    for entry in root.findall("a:entry", NS):
        props = entry.find("m:properties", NS)
        if props is None:
            continue
        version = props.findtext("d:Version", default="", namespaces=NS)
        pre = props.findtext("d:IsPrerelease", default="false", namespaces=NS) == "true"
        out.append((version, pre))
    nxt = root.find("a:link[@rel='next']", NS)
    return out, None if nxt is None else nxt.get("href")


def latest(get=fetch) -> str:
    """The highest stable version of the package, across every page."""
    url: str | None = FEED
    versions = []
    for _ in range(MAX_PAGES):
        if url is None:
            break
        entries, url = parse_feed(get(url, "application/atom+xml"))
        versions.extend(v for v, pre in entries if not pre and re.fullmatch(r"\d+(\.\d+)*", v))
    else:
        raise UpdateError(f"the Chocolatey feed for {PACKAGE} has more than {MAX_PAGES} pages")
    if not versions:
        raise UpdateError(f"Chocolatey lists no stable {PACKAGE} release ({FEED})")
    return max(versions, key=version_tuple)


def check(args, get=fetch) -> Proposal | None:
    changes = Changes(args.root)
    current = changes.read(PIN).strip()
    version_tuple(current)  # it must be a version
    new = latest(get)
    print(f"Inno Setup: pinned {current}, latest on Chocolatey {new}")
    if version_tuple(new) <= version_tuple(current):
        print("up to date")
        return None
    changes.set(PIN, new + "\n")
    major_note = ""
    if version_tuple(new)[0] != version_tuple(current)[0]:
        major_note = f"""
**New major version.** windows.yml runs `ISCC.exe` from
`Program Files (x86)\\Inno Setup {version_tuple(current)[0]}`; Inno Setup
{version_tuple(new)[0]} installs elsewhere, so update that path (and check
`installer/delegation.iss` against its changes) on this branch.
"""
    body = f"""Inno Setup **{current} → {new}** (the Chocolatey `{PACKAGE}` package).

- Package: https://community.chocolatey.org/packages/{PACKAGE}/{new}
- Inno Setup: https://jrsoftware.org/isinfo.php (see "What's New")
{major_note}
Changed: `{PIN}`, which the Windows build reads (it installs exactly that
version with Chocolatey when the runner has a different one).

**Test:** this PR's Windows build in CI. Check that it produced the
installer (the `DelegationTray-Setup-…` artifact) and, ideally, that the
installer runs.

Opened by the weekly dependency check (`.github/workflows/dependency-updates.yml`).
Review and merge by hand.
"""
    return Proposal(title=f"Update Inno Setup to {new}", body=body, changes=changes, version=new)


if __name__ == "__main__":
    sys.exit(run(check, parser(__doc__).parse_args()))
