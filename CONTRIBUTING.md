# Contributing to MATS

Thanks for helping improve MATS. This guide covers the common ways to
contribute: reporting a bug, reporting a mod that fails to test, and sending
code.

## Code of conduct

By taking part you agree to follow [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## Reporting a bug

Open an issue with:

- the MATS version (`mats --version`), OS and CPU architecture;
- the exact command you ran;
- the mod JAR that reproduces it, or its metadata if the JAR is private;
- `mats doctor` output;
- the report directory (`mats report --path`) or at least `report.md`,
  `error.md` and `logs/`.
- `RUST_BACKTRACE=1` output if MATS itself panicked.

Please do not paste access tokens, private mod files or personal paths. Report
security problems privately as described in [SECURITY.md](SECURITY.md).

## Reporting a mod that fails

A "failure" is useful information, so include what MATS observed:

1. `mats inspect mod.jar` — what MATS understood.
2. `mats analyze mod.jar` — static findings.
3. `mats matrix mod.jar` — the versions it believes are supported.
4. `mats test mod.jar` — the run itself.

If the mod needs other mods, list them and whether `--with` was used. If the
failure only happens on a dedicated server, say so: client-only mods are
expected to be absent from the server's mod list.

## Development setup

```sh
git clone https://github.com/Ahaduzzamankhan/Minecraft-Automated-Testing-System
cd Minecraft-Automated-Testing-System
cargo build
cargo test --workspace
```

The repository pins Rust 1.85+ in `rust-toolchain.toml`. No Java installation is
needed for the test suite; the tests that need a live game are opt-in and live
outside the default run.

### Before opening a pull request

```sh
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
```

All three should behave the same way they do on `main`. New behaviour needs a
test; bug fixes should include a regression test when one is practical.

## Project conventions

- **Never report untested behaviour as passing.** If a test cannot run, use
  `NotTested` with a reason. This rule matters more than any other.
- Keep user-facing errors actionable: say what failed, why, and what to do next.
- Do not add dependencies casually. Prefer the standard library and the existing
  stack (blocking `ureq`, `zip`, `tar`, `serde`, `clap`).
- Network access in tests is not allowed. Use `file://` mirrors and temporary
  directories.
- Archive handling must stay defensive: validate every path, verify checksums,
  and never extract links outside the destination.
- Rust 2024 edition, formatted with `rustfmt` defaults.
- Commit messages describe the reason for the change, in the imperative mood.

### Repository layout

| Path | Contents |
|---|---|
| `crates/mats-core` | Shared types, config, paths, version logic |
| `crates/mats-cli` | The `mats` binary and command implementations |
| `crates/test-runner` | End-to-end orchestration |
| `crates/runtime-builder` | Environment preparation and launch commands |
| `crates/resource-manager` | Downloads, cache, safe extraction |
| `.github/workflows` | CI and release automation |
| `install.sh`, `install.ps1` | One-line installers |

## Pull request checklist

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --workspace --all-targets` adds no new warnings
- [ ] `cargo test --workspace` passes
- [ ] New or changed behaviour has a test
- [ ] Documentation in `README.md` is updated when a command or flag changes
- [ ] No secrets, tokens, private mods or machine-specific paths are committed

## License

Contributions are accepted under the MIT license described in
[LICENSE](LICENSE).
