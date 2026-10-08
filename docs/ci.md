# CI

GitHub Actions runs `.github/workflows/ci.yml`, including Linux checks,
Windows and Android Clippy, and TeamSpeak integration smoke tests. Automatic
runs are off: CI, the dependency audit and fuzzing run only by hand
(Actions → the workflow → Run workflow; the same on Gitea). Only the package
builds run by themselves, on `v*` tags, and they run none of these checks.
Both GitHub and Gitea provide release packaging; see [release.md](release.md)
and [building.md](building.md).

Cargo builds use one job in CI (`ci.yml`) so the generated UI library and
its test target do not compile concurrently. GitHub's release workflow uses
one job too, also inside its Flatpak build: on its 16 GB runners the UI
library does not fit next to other crates, and the runner is killed. The
Gitea release workflow, its Docker images and the Flatpak manifest itself
use Cargo's defaults (one job per CPU, no incremental builds in release
mode). The release profile uses ThinLTO and
16 codegen units instead of fat LTO and one unit, reducing peak compiler
memory while retaining optimised builds and panic unwinding. This trades some
whole-program optimisation for lower build-memory requirements; release
runtime performance has not been benchmarked for this change.

## Gitea Actions

`.gitea/workflows/ci.yml` runs a single Linux job when dispatched by hand
(automatic runs on pushes and pull requests are off).

The job runs:

- formatting and warnings-as-errors Clippy for our crates;
- workspace Clippy (vendored warnings are reported, compilation errors fail);
- a locked workspace all-targets build and workspace tests under Xvfb;
- cargo-deny 0.20.2 workspace policy checks and the cargo-about 0.9.2 notices check;
- offline regression tests of the Gitea draft-release uploader.

The workspace tests are not disabled. Xvfb supplies the display for X11
capture and hotkey tests. This is Linux CI, not parity with every GitHub job:
it does not run Windows/Android checks, Docker-based TeamSpeak smoke tests,
opt-in browser interop, or package builds. Gitea release packaging runs in
the separate `release.yml` workflow below.

### Runner setup

1. Enable Actions on the Gitea instance and in the repository settings.
2. Register an **x86_64 Linux** `act_runner` with a Docker-capable host and
   this label (the left side must match the workflow's `runs-on`):

   ```text
   ubuntu-24.04:docker://ghcr.io/catthehacker/ubuntu:act-24.04
   ```

   Configure this in the runner's labels when registering/configuring it;
   an `ubuntu-latest` label alone does not match. The job image must provide
   Bash, Git, Node.js for checkout v4, and root or passwordless sudo for apt.
3. Allow network access from the runner and job container to the Gitea
   instance, GitHub (checkout action, dependencies and RustSec advisories),
   GHCR (runner image), Ubuntu apt mirrors, rustup distribution servers,
   crates.io and dependency/license sources used by cargo-about.
4. Provision enough RAM, swap and free disk for a full Slint workspace build
   plus cargo-deny/cargo-about compilation. These are large builds; monitor
   the first run and size the runner accordingly. The workflow limits Cargo
   to one build job, disables incremental compilation, and reuses build
   outputs between steps in one job. It does not create swap or modify the
   Gitea host.
5. The workflow requests a three-hour job timeout. Legacy `act_runner`
   versions ignore `timeout-minutes`; configure their runner-level limit
   in the runner configuration instead:

   ```yaml
   runner:
     timeout: 3h
   ```

   Newer `gitea-runner` (2.0+) supports the workflow-level timeout. Check
   the installed runner's configuration rather than assuming the YAML
   timeout is enforced. The Gitea server also stops any task running
   longer than `ENDLESS_TASK_TIMEOUT` (default `3h`, `[actions]` in
   `app.ini`), whatever the runner or the workflow say; raise it above the
   longest job (see the release runner below).

The workflow shares the repository's local system-dependencies action with
GitHub CI and bootstraps rustup if absent, then installs the toolchain and
components pinned in `rust-toolchain.toml`. Checkout uses a fully qualified
GitHub action URL rather than depending on the instance's action-source
configuration.

No GitHub cache/artifact service or reusable-workflow support is required.
The job does not need a Docker socket mounted inside its container; the
runner itself still needs Docker access to start that container. Keep this
runner isolated from production services and credentials, particularly when
accepting untrusted pull requests. No release signing or publishing secrets
are needed.

After registering the runner, dispatch **CI** in Gitea Actions and inspect
all step results. Local YAML/shell checks do not prove that a particular
Gitea instance, runner registration, network policy or resource allocation
can complete this job.

### Gitea release runner

The instance at `https://git.faumaray.ru` reported Gitea **1.25.4** on
2026-10-04. Release publishing uses its REST API; artifact upload uses the
pinned Gitea artifact-action fork, not stock GitHub upload-artifact v4.

Register a separate **repository-scoped**, **x86_64 Ubuntu 24.04 native
host** runner with the label:

```text
voelin-release:host
```

Provision Bash, Git, Node.js 20+, Python 3.11+, curl, tar, gzip and rootful
Docker 23+ with BuildKit. Use **act_runner 0.2.7 or newer** (prefer the latest
maintained release), or gitea-runner 2+. Older act_runner does not provide
`ACTIONS_RESULTS_URL`, which the pinned artifact action needs. Register the
runner against `https://git.faumaray.ru`, reachable from both the host and
build containers. Gitea 1.25.4 provides the required artifact-v4 API.
The shared checks install native build dependencies
through root or passwordless sudo and bootstrap Rust. Docker builds install
Windows cross-toolchains, Android SDK/NDK and Java themselves; no Windows
runner or preinstalled Android SDK is needed. The Flatpak builder runs a
privileged container using the same Freedesktop 25.08 image as GitHub.
Allow network access to the existing CI sources, Flathub, Google's Android
downloads/Maven, Chromium's libvpx git server, Docker Hub, GHCR and the
gateway image's distroless base at GCR. Configure Gitea release attachment
types/size limits and reverse-proxy upload limits for the APKs, Flatpak and
gateway image; artifact and release storage must fit the full package set.

This host is not the container runner used for pull requests. Restrict it
to this repository's trusted release workflow, protect `v*` tags, and only
dispatch trusted branches. Do not advertise this label on an instance-wide
runner that untrusted repositories or PR workflows can select. Docker and
Flatpak privileges effectively grant host access. Use an isolated machine
with no production services or credentials.

Configure **one release runner**, with capacity 1, so runs do not race:

```yaml
runner:
  capacity: 1
  timeout: 8h
```

The workflow requests eight hours; older act_runner ignores job-level
timeouts, so the runner limit is required. The server's own limit must
allow it too, or Gitea stops the release after three hours:

```ini
[actions]
ENDLESS_TASK_TIMEOUT = 9h
```

Keep sufficient RAM/swap and free disk for all package builds and their
Docker caches. The package builds run one after another, each with as many
Cargo jobs as the host has CPUs. The workflow never prunes shared Docker
state or creates host swap. The token and optional Android signing secrets
are documented in
[Gitea releases](release.md#gitea-releases).

The Gitea release job builds packages only: it runs no formatting, Clippy,
tests, native Windows, Android Clippy or TS3/TS6 smoke tests; run CI by hand
for those. Full hosted builds and a draft upload must be verified on the registered
runner; local shell/API fixtures do not establish deployment acceptance.
