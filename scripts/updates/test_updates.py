"""Offline tests of the dependency checks, against copies of this
repository's real files (so a pin that moves breaks a test, not the weekly
run). Run: python3 -m unittest discover -s scripts/updates
"""

from __future__ import annotations

import argparse
import json
import shutil
import tempfile
import unittest
from pathlib import Path

import common
import debian
import innosetup
import llama_cpp
import nodejs
import rust_toolchain

ROOT = common.REPO_ROOT
FILES = [
    "rust-toolchain.toml",
    "client/Cargo.toml",
    "server/Cargo.toml",
    "protocol/Cargo.toml",
    "client/Dockerfile",
    "server/Dockerfile",
    "README.md",
    "frontend/.nvmrc",
    "dev/docker-compose.yml",
    "installer/inno-setup-version",
    "client/runtime-pins.json",
]


class Repo:
    """A temp copy of the pinned files."""

    def __init__(self):
        self.dir = Path(tempfile.mkdtemp(prefix="updates-test-"))
        for f in FILES:
            (self.dir / f).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(ROOT / f, self.dir / f)

    def args(self, dry_run=False, **extra):
        return argparse.Namespace(root=self.dir, dry_run=dry_run, out=None, **extra)

    def read(self, f):
        return (self.dir / f).read_text(encoding="utf-8")

    def close(self):
        shutil.rmtree(self.dir)


class RepoTest(unittest.TestCase):
    def setUp(self):
        self.repo = Repo()

    def tearDown(self):
        self.repo.close()


class CommonTest(RepoTest):
    def test_an_edit_that_doesnt_match_fails(self):
        c = common.Changes(self.repo.dir)
        with self.assertRaises(common.UpdateError):
            c.apply(common.Edit("rust-toolchain.toml", r"^nope$", "x"))
        with self.assertRaises(common.UpdateError):
            c.apply(common.Edit("missing.txt", r"x", "y"))

    def test_versions_compare_numerically(self):
        self.assertLess(common.version_tuple("1.99.1"), common.version_tuple("1.100.0"))
        with self.assertRaises(common.UpdateError):
            common.version_tuple("1.x")


class RustTest(RepoTest):
    def current(self):
        return common.find_one(self.repo.read("rust-toolchain.toml"), r'^channel = "(.+)"$', "toolchain")

    def test_up_to_date_is_no_proposal(self):
        cur = self.current()
        self.assertIsNone(rust_toolchain.check(self.repo.args(), latest=lambda: cur, image_exists=lambda *_: True))

    def test_every_place_moves_together(self):
        p = rust_toolchain.check(self.repo.args(), latest=lambda: "9.0.0", image_exists=lambda *_: True)
        p.changes.write()
        self.assertIn('channel = "9.0.0"', self.repo.read("rust-toolchain.toml"))
        for crate in ("client", "server", "protocol"):
            self.assertIn('rust-version = "9.0.0"', self.repo.read(f"{crate}/Cargo.toml"))
        for d in ("client", "server"):
            self.assertIn("rust:9.0.0-slim-", self.repo.read(f"{d}/Dockerfile"))
        self.assertIn("(9.0.0)", self.repo.read("README.md"))
        self.assertEqual(len(p.changes.changed()), 0, "all written")

    def test_no_image_yet_is_no_proposal(self):
        self.assertIsNone(rust_toolchain.check(self.repo.args(), latest=lambda: "9.0.0", image_exists=lambda *_: False))


class NodeTest(RepoTest):
    def index(self, *releases):
        return [{"version": v, "lts": lts} for v, lts in releases]

    def test_only_a_newer_lts_major_counts(self):
        cur = int(self.repo.read("frontend/.nvmrc"))
        same = self.index((f"v{cur}.99.0", "X"), (f"v{cur + 1}.0.0", False))
        self.assertIsNone(nodejs.check(self.repo.args(), index=same, image_exists=lambda *_: True))
        newer = self.index((f"v{cur + 2}.0.0", False), (f"v{cur + 1}.3.0", "Y"), (f"v{cur}.99.0", "X"))
        p = nodejs.check(self.repo.args(), index=newer, image_exists=lambda *_: True)
        self.assertEqual(p.version, str(cur + 1))
        p.changes.write()
        self.assertEqual(self.repo.read("frontend/.nvmrc"), f"{cur + 1}\n")
        self.assertIn(f"node:{cur + 1}-alpine", self.repo.read("server/Dockerfile"))
        self.assertIn(f"node:{cur + 1}-slim", self.repo.read("dev/docker-compose.yml"))


FEED = """<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"
      xmlns:d="http://schemas.microsoft.com/ado/2007/08/dataservices"
      xmlns:m="http://schemas.microsoft.com/ado/2007/08/dataservices/metadata">
  {entries}
  {next}
</feed>"""


def feed(versions, next_url=None):
    entries = "".join(
        f"<entry><m:properties><d:Version>{v}</d:Version>"
        f"<d:IsPrerelease>{'true' if pre else 'false'}</d:IsPrerelease></m:properties></entry>"
        for v, pre in versions
    )
    nxt = f'<link rel="next" href="{next_url}" />' if next_url else ""
    return FEED.format(entries=entries, next=nxt).encode()


class InnoTest(RepoTest):
    def test_the_highest_stable_version_across_pages(self):
        pages = {
            innosetup.FEED: feed([("5.5.9.20160928", False), ("6.2.0", False)], "page2"),
            "page2": feed([("99.0.0-beta", True), ("6.10.0", False), ("7.0.0", True)]),
        }
        self.assertEqual(innosetup.latest(lambda url, accept=None: pages[url]), "6.10.0")

    def test_a_newer_version_rewrites_the_pin(self):
        get = lambda url, accept=None: feed([("99.1.0", False)])
        p = innosetup.check(self.repo.args(), get=get)
        p.changes.write()
        self.assertEqual(self.repo.read(innosetup.PIN), "99.1.0\n")
        self.assertIn("New major version", p.body)
        cur = self.repo.read(innosetup.PIN).strip()
        self.assertIsNone(innosetup.check(self.repo.args(), get=lambda u, a=None: feed([(cur, False)])))


class DebianTest(RepoTest):
    def release(self, codename, version):
        return f"Origin: Debian\nCodename: {codename}\nVersion: {version}\n"

    def test_a_newer_stable_is_an_issue(self):
        cur = common.find_one(self.repo.read("client/Dockerfile"), r"debian:(\w+)-slim", "debian")
        suites = {"stable": self.release("zzz", 99), cur: self.release(cur, 12)}
        get = lambda url: suites[url.split("/")[-2]]
        p = debian.check(self.repo.args(), get=get, image_exists=lambda *_: True)
        self.assertIsNone(p.changes, "an issue, not a PR")
        self.assertIn(f"from Debian {cur} to zzz", p.title)
        self.assertIn("libgomp1", p.body)

    def test_the_same_release_is_nothing(self):
        cur = common.find_one(self.repo.read("client/Dockerfile"), r"debian:(\w+)-slim", "debian")
        get = lambda url: self.release(cur, 12)
        self.assertIsNone(debian.check(self.repo.args(), get=get, image_exists=lambda *_: True))


class LlamaTest(RepoTest):
    def pins(self):
        return json.loads(self.repo.read(llama_cpp.PINS))

    def release_for(self, new, rename=None, drop_digest=None):
        """A fake `new` release with every pinned file under its new name;
        `rename` maps a substring to replace in names (an upstream rename)."""
        pins = self.pins()
        old = pins["version"]
        assets = []
        for v in pins["variants"]:
            for a in v["archives"]:
                name = a["url"].rsplit("/", 1)[1].replace(old, new)
                if rename:
                    for k, r in rename.items():
                        name = name.replace(k, r)
                unchanged = old not in a["url"].rsplit("/", 1)[1]
                sha = a["sha256"] if unchanged else (name.encode().hex() * 2)[:64].ljust(64, "0")
                assets.append(
                    {
                        "name": name,
                        "size": a["size_bytes"] if unchanged else 1000,
                        "digest": None if name == drop_digest else f"sha256:{sha}",
                    }
                )
        return {"tag_name": new, "draft": False, "prerelease": True, "assets": assets}

    def run_check(self, release, verify, **kw):
        get_json = lambda url: [{"tag_name": "v0.9.0", "draft": False}, release, {"tag_name": "b99999999", "draft": True}]
        args = self.repo.args(verify_downloads=False, catalog=None, **kw)
        return llama_cpp.check(args, get_json=get_json, verify=verify)

    def verify_from(self, release):
        by_name = {a["name"]: a for a in release["assets"]}
        downloaded = []

        def verify(url):
            a = by_name[url.rsplit("/", 1)[1]]
            downloaded.append(a["name"])
            return a["digest"].removeprefix("sha256:"), a["size"]

        return verify, downloaded

    def new_tag(self):
        return "b" + str(llama_cpp.tag_number(self.pins()["version"]) + 7)

    def test_the_newest_build_not_releases_latest(self):
        rel = self.release_for("b5")
        got = llama_cpp.latest_release(
            lambda url: [{"tag_name": "v0.9.0"}, {"tag_name": "b4"}, rel, {"tag_name": "b9", "draft": True}]
        )
        self.assertEqual(got["tag_name"], "b5")

    def test_every_file_moves_and_is_verified(self):
        new = self.new_tag()
        rel = self.release_for(new)
        verify, downloaded = self.verify_from(rel)
        p = self.run_check(rel, verify)
        p.changes.write()
        pins = self.pins()
        self.assertEqual(pins["version"], new)
        urls = [a["url"] for v in pins["variants"] for a in v["archives"]]
        self.assertTrue(all(f"/download/{new}/" in u for u in urls), urls)
        # The Windows CUDA runtime keeps its name and hash: not downloaded again.
        self.assertEqual(len(downloaded), len([u for u in urls if new in u.rsplit("/", 1)[1]]))
        self.assertIn(f"compare/", p.body)
        self.assertIn("Manual check", p.body)
        self.assertNotIn("need a decision", p.body)
        self.assertIn(f"`{new}`", self.repo.read("README.md"))
        # Up to date afterwards.
        self.assertIsNone(self.run_check(rel, verify))

    def test_a_renamed_gpu_file_stays_on_the_old_release_and_is_reported(self):
        old, new = self.pins()["version"], self.new_tag()
        rel = self.release_for(new, rename={"cuda-13.4": "cuda-13.5"})
        verify, _ = self.verify_from(rel)
        p = self.run_check(rel, verify)
        self.assertTrue(p.body.startswith("## ⚠ Assets that need a decision"), p.body[:200])
        self.assertIn("cuda-13.5", p.body, "a suggestion")
        p.changes.write()
        for v in self.pins()["variants"]:
            want = old if v["variant"] == "cuda-13" else new
            for a in v["archives"]:
                self.assertIn(f"/download/{want}/", a["url"])

    def test_a_missing_cpu_build_fails(self):
        rel = self.release_for(self.new_tag(), rename={"bin-ubuntu-x64": "bin-ubuntu-22.04-x64"})
        verify, _ = self.verify_from(rel)
        with self.assertRaisesRegex(common.UpdateError, "CPU build"):
            self.run_check(rel, verify)

    def test_a_download_that_doesnt_match_the_api_fails(self):
        rel = self.release_for(self.new_tag())
        with self.assertRaisesRegex(common.UpdateError, "doesn't match"):
            self.run_check(rel, lambda url: ("0" * 64, 1000))

    def test_a_missing_digest_is_a_problem_not_a_guess(self):
        new = self.new_tag()
        rel = self.release_for(new, drop_digest=f"llama-{new}-bin-ubuntu-vulkan-x64.tar.gz")
        verify, _ = self.verify_from(rel)
        p = self.run_check(rel, verify)
        self.assertIn("no SHA-256", p.body)

    def test_a_dry_run_doesnt_download(self):
        rel = self.release_for(self.new_tag())
        p = self.run_check(rel, lambda url: self.fail("downloaded"), dry_run=True)
        self.assertIn("dry run", p.body)

    def test_catalog_notes(self):
        f = self.repo.dir / "catalog.json"
        f.write_text(json.dumps([{"id": "a", "min_runtime_version": "b15"}, {"id": "b", "min_runtime_version": "b30"}]))
        self.assertEqual(len(llama_cpp.catalog_notes(f, "b10", "b20")), 1)


if __name__ == "__main__":
    unittest.main()
