# Contributing to Zaivern Code

Thanks for helping out. This document covers how to verify a change locally; the
[README](README.md) covers what the product is and how to install it.

## Before you start

- Open an [issue](https://github.com/tacyan/zaivern-code/issues) first for anything
  larger than a bug fix, so we can agree on the approach before you write code.
- Send pull requests against `main`.
- Rust 1.88 or newer is required to build from source.

## Build

```bash
git clone https://github.com/tacyan/zaivern-code.git
cd zaivern-code
rustup update stable
cargo run --release -- .
```

## Verify a change

`tools/verify.sh` runs formatting, compilation, warnings, and the tests for the modules
you touched in a single compile pass. With no arguments it picks the modules from
`git status`.

```bash
tools/verify.sh                  # only the modules you changed
tools/verify.sh git:: lsp::      # named module prefixes
tools/verify.sh --all            # every test
tools/verify.sh --quick          # skip the slower stages
tools/verify.sh --lint           # everything above, plus clippy
```

**Run `tools/verify.sh --lint` before every push.** CI lints with clippy, which is
stricter than rustc's own warnings, so a green `cargo test` is not enough on its own.

To run the suite the way CI does:

```bash
cargo nextest run --profile ci
```

The `terminal::` tests drive real PTYs. Running them inside a single `cargo test`
process accumulates child process trees, so CI uses cargo-nextest, which gives each test
its own process and serialises the `pty` group. The configuration lives in
`.config/nextest.toml`.

Tests must not touch a real `~/.zaivern`. Use `crate::test_util::unique_temp_dir` and
point `ZAIVERN_HOME` at a temporary directory — other contributors (and other running
instances) share that location.

## Check the other platforms locally

Code behind `#[cfg(windows)]` or a Linux-only branch never compiles during a macOS
build, so a fully green local run can still break CI. Both platforms are reproducible
on your own machine, which is much faster than a CI round trip.

```bash
tools/linux-test.sh              # run the Linux tests in Docker
tools/linux-test.sh keybinds::   # just one module
tools/linux-test.sh --check      # compile only

tools/windows-check.sh           # type-check for Windows (MSVC)
tools/windows-check.sh --build   # produce a real zai.exe, verifying the link step
tools/windows-check.sh --clippy  # clippy with the same allow-list CI uses
tools/windows-check.sh --gnu     # via mingw-w64 instead of cargo-xwin
```

The Windows checks need `cargo install cargo-xwin --locked` once. Neither script writes
to the host `target/` directory, so switching between platforms does not trigger a full
rebuild.

Runtime behaviour and the GUI are **not** covered by these checks on Windows. CI's
`windows-latest` job is the only gate for those.

## Conventions

- No hard-coded paths, user names, or OS assumptions — derive them from `std::env`, the
  `dirs` crate, or configuration, and implement both sides of any `cfg!(windows)` branch.
- egui is pinned to 0.29 and must not be upgraded.
- `vendor/vt100` is vendored with a local patch. If you bump the version, port the
  `visible_rows` fix with it.
- Add a new feature by creating `src/features/<name>.rs`. `build.rs` discovers it, so
  you do not need to edit `app.rs`, `palette.rs`, `feature.rs`, or `main.rs` — which is
  what keeps parallel branches from colliding.
- Every feature needs at least one way to reach it from the UI. Check `cargo check` for
  `never used` warnings before calling a change done.
- Don't measure performance with wall-clock thresholds; they produce false failures.
  Measure the property itself — syscall counts, structure sizes, or how a cost scales as
  the input doubles. See [docs/bench-honesty.md](docs/bench-honesty.md).

## Documentation

`docs/` is indexed by [docs/README.md](docs/README.md), grouped by the claim each
document backs. One document, one guarantee: if you add a measurement, put it with the
claim it supports, and state the conditions under which it was taken.

## License

By contributing, you agree that your contributions are licensed under the
[Apache License 2.0](LICENSE).
