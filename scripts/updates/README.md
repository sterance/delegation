# Dependency updates

Every pinned dependency is checked weekly, on Monday morning Melbourne
time. Updates arrive as pull requests (one issue, for Debian). CI tests
them, and **a person reviews and merges every one: nothing merges
automatically.**

| Dependency | Pinned in | Updated by |
|---|---|---|
| Rust crates | `Cargo.lock` | Dependabot (`cargo`), minor and patch grouped |
| npm packages | `frontend/package-lock.json` | Dependabot (`npm`), minor and patch grouped |
| GitHub Actions | `.github/workflows/*.yml` (third-party actions by commit SHA) | Dependabot (`github-actions`) |
| llama.cpp builds | `client/runtime-pins.json` (URLs, SHA-256, sizes) | `dependency-updates.yml` → PR on `updates/llama-cpp` |
| Rust toolchain | `rust-toolchain.toml`; `rust-version` in `client/`, `server/`, `protocol/` `Cargo.toml`; `rust:<v>-slim-…` in `client/Dockerfile` and `server/Dockerfile`; `README.md` | `dependency-updates.yml` → PR on `updates/rust` |
| Node.js (LTS major) | `frontend/.nvmrc`; `node:<n>-alpine` in `server/Dockerfile`; `node:<n>-slim` in `dev/docker-compose.yml` | `dependency-updates.yml` → PR on `updates/nodejs` |
| Inno Setup | `installer/inno-setup-version` | `dependency-updates.yml` → PR on `updates/inno-setup` |
| Debian base image | `debian:<codename>-slim`, and the Rust images' `-<codename>` suffix | `dependency-updates.yml` → **issue** |

The workflows read their versions from these files: CI's Rust from
`rust-toolchain.toml`, Node from `frontend/.nvmrc`, and the Windows build's
Inno Setup from `installer/inno-setup-version`. So the update PRs never
change `.github/workflows/`, and the token that opens them needs no
permission to change workflows.

Not covered: the test model in `config.env.example` (data, not software),
and the `sterance/infra` repository.

## One-time setup

### The GitHub App (preferred)

PRs opened with a workflow's own `GITHUB_TOKEN` don't trigger other
workflows, so CI wouldn't run on them. The update PRs are opened as a
GitHub App instead.

1. **Create the app:** your profile → Settings → Developer settings → GitHub
   Apps → New GitHub App.
   - Name: anything (e.g. "delegation-updates"). Homepage URL: the repository.
   - Webhook: **untick Active**.
   - Repository permissions: **Contents: Read and write**, **Pull requests:
     Read and write**. Nothing else (Metadata: read-only is added
     automatically).
   - Where can it be installed: Only on this account.
2. On the app's page, note the **Client ID**, then **Generate a private key**
   (a `.pem` file downloads).
3. **Install the app** (Install App, in the app's settings) on **only** the
   `delegation` repository.
4. Add two **repository secrets** (repository → Settings → Secrets and
   variables → Actions → New repository secret):
   - `UPDATES_APP_CLIENT_ID`: the Client ID.
   - `UPDATES_APP_PRIVATE_KEY`: the whole contents of the `.pem` file.
   Then delete the `.pem` file.

Each run creates a token that lasts one hour, is limited to this
repository, and has only those two permissions.

### A personal access token (fallback)

If you'd rather not have an app, create a fine-grained personal access token
(Settings → Developer settings → Personal access tokens → Fine-grained) for
only this repository, with **Contents: Read and write** and **Pull
requests: Read and write**, and save it as the repository secret
`UPDATES_TOKEN`. The workflow uses it when the app secrets aren't set. The
downsides: the PRs are opened as you, the commits aren't signed, and the
token expires, at which point the checks fail until you replace it.

Without either, a check that finds an update fails ("there's no token to
open the PR with") and the failure issue says so.

### Dependabot security updates

Turn on **Dependabot alerts** and **Dependabot security updates**
(repository → Settings → Advanced Security), so security advisories get a
PR at once rather than on Monday. Or, with the GitHub CLI:

```sh
gh api -X PUT repos/sterance/delegation/vulnerability-alerts
gh api -X PUT repos/sterance/delegation/automated-security-fixes
```

## How the custom checks work

`.github/workflows/dependency-updates.yml` runs every Sunday at 22:00 UTC
(Monday 9:00 in Melbourne during daylight saving, 8:00 in winter) and on
demand (Actions → Dependency updates → Run workflow). Each dependency is
its own job, so one failing check doesn't block the others. The logic is in
`scripts/updates/`, one script per dependency (Python, standard library
only):

1. Find the latest version from an authoritative source.
2. Compare it with the pinned one; stop if there's nothing newer.
3. Rewrite every place it's pinned. A pin that isn't where the script
   expects fails the check rather than being skipped.
4. Open the pull request, or update the open one, on `updates/<name>`
   (`peter-evans/create-pull-request`), so there's never more than one per
   dependency.

If any check fails, the last job opens (or updates) a single **"Dependency
update check failed"** issue naming the failed checks. The next run where
every check succeeds closes it.

| Check | Latest version from | Notes |
|---|---|---|
| `llama_cpp.py` | The newest `b<N>` release of `ggml-org/llama.cpp` (GitHub API) | Not `/releases/latest`: the builds are published as prereleases, and "latest" is a separate versioned release without them. |
| `rust_toolchain.py` | `https://static.rust-lang.org/dist/channel-rust-stable.toml` | Only proposed once `rust:<v>-slim-<codename>` is on Docker Hub. |
| `nodejs.py` | The newest LTS in `https://nodejs.org/dist/index.json` | Majors only: minor and patch releases float within `node:<n>`. Waits for the Docker images. |
| `innosetup.py` | The Chocolatey `innosetup` package | |
| `debian.py` | `https://deb.debian.org/debian/dists/stable/Release` | An issue, not a PR: see below. |

### Running a check locally

Every script has `--dry-run`, which prints the diff and the PR text
without changing anything:

```sh
python3 scripts/updates/rust_toolchain.py --dry-run
python3 scripts/updates/nodejs.py --dry-run
python3 scripts/updates/innosetup.py --dry-run
python3 scripts/updates/debian.py --dry-run
GITHUB_TOKEN=$(gh auth token) python3 scripts/updates/llama_cpp.py --dry-run
```

The llama.cpp dry run takes hashes from the release API without
downloading. Add `--verify-downloads` to download and check every changed
file as the workflow does (several GB, mostly the CUDA runtimes). Add
`--catalog <file>` (a `MODEL_CATALOG_PATH` file) to have the PR note
which catalog models the new release makes installable
(`min_runtime_version`).

The scripts' tests run offline, in CI too:

```sh
python3 -m unittest discover -s scripts/updates
```

## Reviewing the PRs

**llama.cpp.** The PR lists the old and new versions, the upstream
comparison, and every updated file with its SHA-256 and size. Each hash is
the release API's digest and also matched a download of the file (except
files whose hash was already pinned, such as the Windows CUDA runtime,
which often doesn't change). Its `runtime-smoke` check
(`.github/workflows/runtime-smoke.yml`, which runs on PRs that change
`runtime-pins.json`) runs the pins validation test, then
`client/src/inference/smoke.rs`, which:

- downloads the Linux x64 CPU build through the client's own runtime code,
  so the real hash check runs;
- checks that `llama-server --help` still lists every flag the client passes
  (taken from `server_args`, not a copy);
- checks that `libllama` still has the log formats the GPU offload parser
  reads, and that lines in those formats parse (the CPU build never prints
  them, but its `libllama` is the same code as the GPU builds');
- loads the 0.5B test model (cached between runs), generates a few tokens
  through the client's streaming code, and checks the timings parse.

To run the smoke test locally:

```sh
export DELEGATION_SMOKE_DIR=/tmp/smoke
eval "$(grep -E '^TEST_MODEL_(ID|URL|SHA256|SIZE_BYTES)=' config.env.example | sed 's/^/export /')"
cargo test -p delegation-client --lib smoke -- --ignored --nocapture
```

GPU builds can't run on GitHub's runners, so the PR has a checklist for a
check by hand on an NVIDIA machine (and Vulkan, if available).

If a file is missing from the new release, or has been renamed (for
example, upstream moving a CUDA build from 13.4 to 13.5), the script
doesn't guess. That variant stays on the old release, and the problem
heads the PR description with any similarly named files as suggestions.
The client pins a single release, so the pins test, and with it CI, fails
on that PR until someone fixes the variant on the branch: the right file,
its SHA-256 and size, and for CUDA `requires.cuda_driver`. A missing or
renamed **CPU** build fails the check instead, because every client needs
it.

**Rust.** All seven places change together, and the PR is only opened once
the Docker image exists. New Clippy lints may fail CI; fix them in the same
PR.

**Node.js.** Bump `@types/node` to the same major on the branch
(Dependabot leaves its major alone for this reason).

**Inno Setup.** The PR's Windows build is the test: it installs exactly
the pinned version (with Chocolatey, if the runner has a different one) and
fails if the compiler reports another. Check that it produced the
installer.

**Debian (issue).** Moving the base image changes glibc and the system
libraries that the downloaded llama.cpp builds load: glibc 2.34 or newer,
GCC 12's libstdc++, `libssl3`, `libgomp1`, and the Vulkan loader and
drivers. So it's a deliberate change. The issue lists the files and the
checks.

## Testing the workflow

- Run it by hand (Actions → Dependency updates → Run workflow) on a branch
  where each dependency is set back one version. It opens one PR per
  dependency, against that branch. Running it again updates those PRs
  rather than opening new ones.
- The `fail` input makes one check fail on purpose. That opens the "check
  failed" issue, and the next normal run closes it.
- Dependabot's first runs show under Insights → Dependency graph →
  Dependabot.
