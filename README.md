# MATS — Minecraft Automated Testing System

MATS is a single-file command line tool that takes a Minecraft mod JAR, works out
what it needs, downloads the official game files, boots the real game in an
isolated JVM, and writes a report that says exactly what it observed.

It does not reimplement Minecraft, mock game classes, or pretend something
worked. If a test could not run — no GPU, no display, a missing dependency, a
loader that refuses to start — the report says so instead of showing a green
tick.

```
$ mats test mymod.jar

  Mod:        InputBooster 4.1.2-alpha-fabric-mc263
  Target:     Minecraft 26.3 · Fabric 0.20.1 · both
  Java:       Java 25.0.5
  Verdict:    INCOMPLETE

  PASSED     server startup           reached startup in 8.71 s
  NOT TESTED Mod discovery            the mod declares environment `client`; it is not loaded on the server side
  PASSED     Mod initialisation       no initialisation errors were reported
  PASSED     Mixin application        all declared mixins were applied without errors
  PASSED     Runtime errors           the process was shut down cleanly after the startup phase
  NOT TESTED Client startup           no graphical display or Xvfb is available; client tests are reported as not tested
```

---

## Table of contents

- [Why MATS exists](#why-mats-exists)
- [Features](#features)
- [Requirements](#requirements)
- [Install](#install)
  - [Linux and macOS](#linux-and-macos)
  - [Windows](#windows)
  - [Build from source](#build-from-source)
- [Quick start](#quick-start)
- [Commands](#commands)
  - [`mats test`](#mats-test)
  - [`mats inspect`](#mats-inspect)
  - [`mats analyze`](#mats-analyze)
  - [`mats matrix`](#mats-matrix)
  - [`mats versions`](#mats-versions)
  - [`mats doctor`](#mats-doctor)
  - [`mats cache`](#mats-cache)
  - [`mats env`](#mats-env)
  - [`mats history`](#mats-history)
  - [`mats report`](#mats-report)
  - [`mats update`](#mats-update)
- [Understanding the result](#understanding-the-result)
- [Reports](#reports)
- [How MATS works](#how-mats-works)
- [Version and Java resolution](#version-and-java-resolution)
- [Client testing and headless machines](#client-testing-and-headless-machines)
- [Isolation, safety and the Minecraft EULA](#isolation-safety-and-the-minecraft-eula)
- [Cache and on-disk layout](#cache-and-on-disk-layout)
- [Configuration](#configuration)
- [Self-update](#self-update)
- [Continuous integration and releases](#continuous-integration-and-releases)
- [Troubleshooting](#troubleshooting)
- [Development](#development)
- [Contributing](#contributing)
- [License](#license)

---

## Why MATS exists

Mod authors usually find out that a build is broken when a player reports a
crash. Reproducing that crash means installing the right loader, the right
Minecraft version, the right Java runtime, and every dependency — per platform,
per side, by hand.

MATS automates that loop:

1. Read the JAR and its loader metadata (no execution).
2. Decide which Minecraft/loader/Java combination the mod claims to support.
3. Download exactly those official files and verify them.
4. Start the real game — client, dedicated server, or both — in a throwaway
   directory.
5. Watch the log, detect crashes, failed mixins, missing classes and conflicting
   mods.
6. Write a report a human can read and a machine can parse.

It is designed for CI as much as for a desktop: deterministic exit codes, JSON
output, no interactive prompts unless you ask for something destructive.

## Features

- **Loader-aware inspection** — Fabric, Quilt, Forge, NeoForge and vanilla, from
  `fabric.mod.json`, `quilt.mod.json`, `META-INF/mods.toml` and
  `META-INF/neoforge.mods.toml`.
- **Static analysis without execution** — mixin configs, access wideners, nested
  JARs, class-file Java version, duplicate mod ids and overlapping mixin
  targets.
- **Every Minecraft version from Mojang's manifest** — legacy `1.x` releases,
  calendar versions (`26.1`+), and future versions, with no hardcoded list.
- **Automatic provisioning** — Java (Temurin), Minecraft client or server, all
  libraries and natives, asset index, loader installer and loader libraries.
- **Automatic dependencies** — required mods such as Fabric API are resolved
  from Modrinth for the exact Minecraft/loader pair (`--no-deps` opts out).
- **Two sides, separately prepared** — a client environment and a server
  environment, each with its own launch command, so `--side both` really runs
  both.
- **Honest verdicts** — `PASSED`, `FAILED`, `INCOMPLETE` and `NOT TESTED` are
  distinct; untested behaviour is never reported as passing.
- **Real diagnostics** — crash reports, `hs_err` files, exception classes,
  mixin apply failures, missing dependency messages and out-of-memory detection.
- **Reports** — Markdown and JSON, plus dedicated `test.md`, `error.md`,
  `environment.json` and copied game logs.
- **Repeatable environments** — content-addressed cache, reusable environment
  ids, and a `reason` for every download.
- **Self-update** — signed-off GitHub Releases, SHA-256 verification, atomic
  replacement with backup and rollback.
- **One executable** — no JAR, DLL, Python or helper script is needed at
  runtime.

## Requirements

| Requirement | Details |
|---|---|
| Operating system | Windows 10/11 (x86_64, ARM64), Linux (x86_64, ARM64), macOS 11+ (Intel, Apple silicon) |
| Disk space | ~1.5 GB for a first Fabric client run (Java, game, libraries, assets index) |
| Network | Needed for the first run; afterwards `--offline` uses the cache |
| Java | **Not required** — MATS downloads the correct Temurin runtime |
| Graphics | Only for client startup tests. A server-only test needs no GPU |
| EULA | A dedicated-server test needs `--accept-eula` |

## Install

### Linux and macOS

```sh
curl -fsSL https://raw.githubusercontent.com/Ahaduzzamankhan/Minecraft-Automated-Testing-System/main/install.sh | sh
```

The installer detects your OS and architecture, downloads the matching release
asset, verifies it against the published `SHA256SUMS`, installs it to
`~/.local/bin`, and adds that directory to your `PATH`.

Pin a version or change the location:

```sh
MATS_VERSION=v1.0.0 MATS_INSTALL_DIR="$HOME/bin" sh install.sh
```

### Windows

```powershell
irm https://raw.githubusercontent.com/Ahaduzzamankhan/Minecraft-Automated-Testing-System/main/install.ps1 | iex
```

Installs to `%LOCALAPPDATA%\MATS\bin`, verifies the SHA-256, and adds the
directory to your user `PATH`. Open a new terminal afterwards.

### Build from source

```sh
git clone https://github.com/Ahaduzzamankhan/Minecraft-Automated-Testing-System
cd Minecraft-Automated-Testing-System
cargo build --release
./target/release/mats --version
```

Requires Rust 1.85 or newer (the repository pins a toolchain in
`rust-toolchain.toml`).

## Quick start

```sh
# 1. Check that this machine can run tests at all
mats doctor

# 2. See what MATS understood from the JAR (nothing is executed)
mats inspect mymod.jar

# 3. Test it against the versions it declares
mats test mymod.jar

# 4. Test a dedicated server too, and a specific Minecraft version
mats test mymod.jar --side server --accept-eula --mc 26.3

# Result: reports/<mod>-<timestamp>/report.md
```

Common variations:

```sh
# Client only, with an extra mod supplied manually
mats test mymod.jar --side client --with fabric-api.jar

# Test every side, give it more heap and a longer startup window
mats test mymod.jar --side both --accept-eula --memory 4096 --timeout 300

# Use only what is already cached
mats test mymod.jar --offline

# Machine-readable output for CI
mats --json test mymod.jar
```

## Commands

Global options work with every command:

| Option | Meaning |
|---|---|
| `--home <DIR>` | Override the MATS home directory (also `MATS_HOME`) |
| `--config <FILE>` | Load a specific `config.toml` |
| `--json` | Emit JSON where supported |
| `-q`, `--quiet` | Only essential output |
| `-v`, `--verbose` | Extra diagnostic logging |
| `--no-update-check` | Skip the startup update check for this run |

### `mats test`

Inspects the JAR, resolves a target, prepares it, runs the real game and writes
a report.

| Option | Meaning |
|---|---|
| `--mc <VERSION>` | Test this Minecraft version instead of the declared range |
| `--loader <NAME>` | `fabric`, `quilt`, `forge`, `neoforge` or `vanilla` |
| `--loader-version <V>` | Pin a loader build |
| `--side <SIDE>` | `client`, `server` or `both` (default) |
| `--timeout <SECONDS>` | Per-launch timeout |
| `--memory <MIB>` | Maximum JVM heap |
| `--with <JAR>` | Add another mod to the environment (repeatable) |
| `--no-deps` | Do not download required dependencies automatically |
| `--accept-eula` | Accept the Minecraft EULA (required for server tests) |
| `--offline` | Never touch the network |
| `--fresh` | Rebuild the environment even if it is cached |
| `--download-assets` | Download the full client asset set |
| `--display <MODE>` | `auto`, `native`, `virtual` or `disabled` |
| `--allow-snapshots` | Allow snapshots when no release matches |
| `--report-dir <DIR>` | Write reports somewhere else |

Exit codes: `0` passed or incomplete, `1` failed or not tested, `2` setup error.

### `mats inspect`

Prints metadata without executing anything: file hashes, detected loaders and
their evidence, class count and Java version, mixin configs, access wideners,
nested JARs, mod id, version, authors, licence, declared side, Minecraft range,
Java constraint, entrypoints and required dependencies.

### `mats analyze`

Deep static analysis with `--with <JAR>` for conflict comparison: metadata
problems, mixin targets that do not exist, duplicate mod ids, overlapping mixin
classes and bytecode warnings. Exit code `1` when blocking problems are found.

### `mats matrix`

Shows which Minecraft versions and loader builds can actually be tested for this
mod, resolved live from Mojang and the loader's Maven repository.

```
MINECRAFT   JAVA   LOADER   STATUS
26.3        25     0.20.1   supported
```

### `mats versions`

Lists recent Minecraft releases with their Java requirement, and with
`--loader fabric` the newest loader build for the newest release.

### `mats doctor`

Checks the home directory, configuration, Mojang and Fabric metadata
reachability, available Java runtimes, display/Xvfb availability, cache size and
update settings. Use it first when a test fails for environmental reasons.

### `mats cache`

- `mats cache info` — size, object count, Java runtimes, environments.
- `mats cache clean [--envs] [--yes]` — remove cached files (confirmation is
  required unless `--yes`).

### `mats env`

- `mats env list` — prepared environments with Minecraft, loader, size and age.
- `mats env clean --all | --older-than-hours N [--yes]`.

### `mats history`

Shows recent runs, newest first. `--json` for scripts.

### `mats report`

Prints the newest report. `--path` prints its directory, `--json` the JSON
report.

### `mats update`

- `mats update` or `mats update check` — is a newer release available?
- `mats update install [--yes] [--channel stable|prerelease]` — download,
  verify and install.
- `mats update --version` — installed and latest versions, without installing.

## Understanding the result

| Verdict | Meaning |
|---|---|
| `PASSED` | Every test that ran observed the behaviour it checks |
| `FAILED` | A test that ran observed a failure, or a blocking finding was detected |
| `INCOMPLETE` | Nothing failed, but at least one relevant test could not be executed |
| `NOT TESTED` | Nothing could be executed at all |

Individual tests carry one of `PASSED`, `FAILED`, `SKIPPED`, `NOT TESTED` or
`TIMED OUT`. A graphical client that cannot open a window is `NOT TESTED`, not a
pass — and a server pass is never presented as proof of client compatibility.

## Reports

Every run writes a directory under `<home>/reports` (or `--report-dir`):

```
reports/inputbooster-20261113-072758/
├── report.md          # the human report
├── report.json        # the same data as structured JSON
├── test.md            # one section per test, with details
├── error.md           # only blocking findings, for quick triage
├── environment.json   # exact version, loader, Java, env id and launch command
└── logs/              # stdout, stderr, combined log and crash files
```

`report.json` is stable enough to diff between builds and to feed a dashboard.

## How MATS works

```
JAR ─► jar-inspector ─► metadata-parser ─► version-resolver ─► plan
                                                        │
                       resource-manager ──► java-manager ─┤
                                                        ▼
                                            runtime-builder (per side)
                                                        │
                                              process-monitor (JVM)
                                                        │
        log-analyzer · crash-detector · mixin-analyzer · conflict-detector
                                                        │
                                              diagnostics ─► report-generator
```

Nothing about a mod is trusted: archives are validated before extraction, paths
are checked for traversal, checksums are verified, and network responses are
cached with explicit TTLs.

## Version and Java resolution

- Minecraft versions come from Mojang's version manifest, not a list baked into
  the binary. Every past release and any future release is eligible.
- Versions are compared numerically across both eras, so `26.2` is newer than
  `1.21.11`, and pre-releases sort below their release.
- Declared ranges are honoured (`>=26.1`, `[26.2,26.3)`, `1.21.x`, `~1.21.1`,
  `^1.20`). A mod with no range gets the newest releases, with the assumption
  stated in the report.
- Candidate resolution walks through the manifest until it finds a version for
  which the loader actually publishes a build.
- The Java requirement is read from each version's Mojang JSON
  (`javaVersion.majorVersion`). A range-based fallback covers offline runs and
  every legacy release. Minecraft 26.x needs Java 25.

## Client testing and headless machines

Client startup tests need a graphical context. MATS handles three situations:

| Situation | Behaviour |
|---|---|
| A real display is available | Client runs natively |
| No display, but `xvfb-run` exists | Client runs under Xvfb with software OpenGL (`LIBGL_ALWAYS_SOFTWARE=1`) |
| Neither | Client tests are reported `NOT TESTED` with the reason; nothing is faked |

Force a strategy with `--display native|virtual|disabled`. On servers and in
containers, install `xvfb` and Mesa (`apt install xvfb libgl1 libglx-mesa0`) if
you want client coverage; otherwise use server tests there and run client tests
on a machine with a GPU.

## Isolation, safety and the Minecraft EULA

- Each environment lives in its own directory under `<home>/envs/<id>` and is
  launched as its own process group, so a timeout stops the game *and* its
  wrappers.
- Archives are extracted with path-traversal checks; links must resolve inside
  the destination.
- Downloads are verified against Mojang SHA-1 or release SHA-256 values when
  published; an update without a checksum is refused.
- Mods are only executed by `mats test`. `inspect` and `analyze` never run mod
  code.
- A dedicated server requires `--accept-eula`. MATS will not accept Mojang's EULA
  on your behalf; without the flag you get an actionable error instead.

## Cache and on-disk layout

```
<home>/
├── config.toml
├── cache/
│   ├── objects/    # content-addressed files, sharded by SHA-1
│   ├── meta/       # cached manifests, Maven XML, Modrinth responses
│   └── java/       # downloaded Temurin runtimes
├── envs/<id>/      # prepared client/server environments
├── reports/        # one directory per run
└── history.jsonl   # append-only run history
```

Default home: `%APPDATA%\mats` on Windows, `~/.local/share/mats` on Linux,
`~/Library/Application Support/mats` on macOS. Override with `--home` or
`MATS_HOME`.

Environment ids hash the Minecraft version, loader build, side, Java major and
the sorted mod names and checksums — the same inputs reuse the same prepared
environment.

## Configuration

`config.toml` is optional; missing keys fall back to defaults.

```toml
[updates]
enabled = true
auto_check = true
auto_install = false
channel = "stable"        # or "prerelease"
check_interval_hours = 24
timeout_ms = 2500
repository = "Ahaduzzamankhan/Minecraft-Automated-Testing-System"

[runtime]
java = "/usr/bin/java"    # optional explicit JVM
java_major = 25           # optional override
max_heap_mb = 1536
timeout_secs = 300
max_parallel_downloads = 6
offline = false
download_assets = false
auto_dependencies = true
display = "auto"          # auto | native | virtual | disabled
keep_environments = true
extra_jvm_args = []

[reports]
dir = "reports"           # relative to the current directory
include_logs = true

[cache]
max_env_age_hours = 336
warn_above_gb = 10
```

## Self-update

`mats update check` queries the GitHub Releases API, compares semantic versions,
and only accepts prereleases when the `prerelease` channel is enabled.
`mats update install` downloads the asset that matches the current platform
(`mats-v<version>-<os>-<arch>[.exe]`), verifies its SHA-256 against the release's
`SHA256SUMS`, keeps a backup, and replaces the running executable. On Windows a
short-lived helper waits for MATS to exit before swapping the file. Downgrades
require `--allow-downgrade`.

Automatic checks run at most once per interval and never block a test for longer
than `timeout_ms`. Set `auto_install = true` if you want updates installed
without asking.

## Continuous integration and releases

`.github/workflows/ci.yml` builds the workspace and runs the full test suite on
`ubuntu-latest`, `windows-latest` and `macos-latest`, checks formatting and
lints, smoke-tests `mats --version` / `--help`, and uploads logs when a job
fails.

`.github/workflows/release.yml` runs on a `v*` tag (or manually):

| Platform | Target | Asset |
|---|---|---|
| Windows x86_64 | `x86_64-pc-windows-msvc` | `mats-v<version>-windows-x86_64.exe` |
| Windows ARM64 | `aarch64-pc-windows-msvc` | `mats-v<version>-windows-aarch64.exe` |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `mats-v<version>-linux-x86_64` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | `mats-v<version>-linux-aarch64` |
| macOS Intel | `x86_64-apple-darwin` | `mats-v<version>-macos-x86_64` |
| macOS Apple silicon | `aarch64-apple-darwin` | `mats-v<version>-macos-aarch64` |

Each job runs the staged binary (`--version`) before uploading, so a broken
cross-build fails the release. Publishing waits for all six, rejects a missing
asset, writes `SHA256SUMS`, and creates or updates the GitHub release.

Release procedure:

```sh
git tag v1.0.1
git push origin v1.0.1
```

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `Java 25 could not be provisioned` | Download or disk problem. Re-run; check `mats doctor`; `mats cache clean` |
| `Client startup — no graphical display` | Install `xvfb` and Mesa, or run on a machine with a display |
| `the mod declares environment 'client'; it is not loaded on the server side` | Expected for client-only mods; test `--side client` |
| `requires fabric-api ... which is not installed` | Offline, or Modrinth could not be reached; pass `--with fabric-api.jar` |
| `--accept-eula` error | Dedicated-server tests require `mats test --side server --accept-eula` |
| Loader rejects the mod's version range | Use `--mc <version>` to test an explicit version, or `--allow-snapshots` |
| Run takes very long on first use | First run downloads Java, game files and libraries; later runs reuse the cache |
| `mats update install` refuses | The asset must publish a SHA-256; verify the release includes `SHA256SUMS` |

Logs for the most recent run are in the report directory under `logs/`.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
cargo build --release
```

Workspace layout:

| Crate | Responsibility |
|---|---|
| `mats-core` | Domain types, configuration, paths, hashing, version ordering |
| `jar-inspector` | Safe ZIP inspection, loader detection, bytecode statistics |
| `metadata-parser` | Loader metadata and mixin configuration parsing |
| `version-resolver` | Mojang manifest, loader resolution, target planning |
| `resource-manager` | HTTP, cache, resumable parallel downloads, safe extraction |
| `java-manager` | JVM detection and Temurin provisioning |
| `runtime-builder` | Environment preparation, launch commands, mod installation |
| `process-monitor` | Process supervision, markers, timeouts, resource sampling |
| `performance-monitor` | GC log parsing and performance summary |
| `log-analyzer` | Log structure, exceptions, mod list, known failure patterns |
| `crash-detector` | Crash reports, `hs_err` files, abnormal exits |
| `mixin-analyzer` | Static and runtime mixin problems |
| `conflict-detector` | Duplicate mods and overlapping mixin targets |
| `diagnostics` | Finding aggregation, ordering and severity counts |
| `report-generator` | Markdown and JSON reports |
| `storage` | History, environment registry, cache cleaning |
| `updater` | Release checks, verified downloads, safe replacement |
| `test-runner` | End-to-end orchestration |
| `mats-cli` | The `mats` binary |

## Contributing

Bug reports, failing mods and pull requests are welcome. Please read
[CONTRIBUTING.md](CONTRIBUTING.md) and [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
first. Security issues should follow [SECURITY.md](SECURITY.md) instead of a
public issue.

## License

MIT. See [LICENSE](LICENSE).
