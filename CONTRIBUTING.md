# Contributing to MIMUS-EPICS

Thank you for your interest in contributing to MIMUS. This document explains how to set up, change, test and submit work on it.

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Getting Started](#getting-started)
- [Development Setup](#development-setup)
- [How to Contribute](#how-to-contribute)
- [Coding Standards](#coding-standards)
- [Commit Convention](#commit-convention)
- [Pull Request Process](#pull-request-process)
- [Testing](#testing)
- [Documentation](#documentation)
- [License](#license)

## Code of Conduct

MIMUS is developed in the context of scientific research. We are committed to providing a welcoming and inclusive environment for everyone, regardless of background or experience level.

Please be respectful, constructive, and professional in all interactions.

## Getting Started

MIMUS is a PVAccess traffic generator written in Rust. It reproduces what a PVXS IOC puts on the wire, at scales no fleet of soft IOCs can reach, so that archivers, gateways and displays can be measured under a load that is both realistic and reproducible. Before contributing, we recommend:

1. Reading the [README](README.md), and the [catalogue README](catalogue/README.md) for what calibration covers
2. Familiarizing yourself with [EPICS](https://epics-controls.org/) and the [PV Access protocol](https://docs.epics-controls.org/en/latest/pv-access/overview.html)
3. Having a working knowledge of Rust and async programming with [Tokio](https://tokio.rs/)

## Development Setup

### Prerequisites

- **Rust**, at the version pinned in [`rust-toolchain.toml`](rust-toolchain.toml). [rustup](https://rustup.rs/) installs it on the first `cargo` invocation in the repository.
- **PVXS** with `softIocPVX`, for calibration and the benchmark only. It is needed neither to build nor to serve an existing profile.

## How to Contribute

### Reporting Bugs

Open an issue on GitHub with:

- A clear, descriptive title
- Steps to reproduce the behavior
- Expected vs. actual behavior
- MIMUS version (`mimus --version`), Rust version, OS, and PVXS and EPICS base versions if a reference IOC is involved
- Relevant output, rerun with `--debug` where the command has it: `mimus calibrate --debug` prints what was seen channel by channel, `mimus serve --debug` prints the first message of each shape in hex

### Suggesting Features

Open an issue with the `enhancement` label. Describe:

- The problem your feature would solve
- Your proposed approach
- Any alternatives you considered

### Submitting Code

1. Fork the repository
2. Create a branch from `main` (`git checkout -b feat/my-feature`)
3. Make your changes following the [coding standards](#coding-standards)
4. Add or update tests as appropriate
5. Run the same checks as continuous integration (see [Testing](#testing))
6. Submit a pull request

## Coding Standards

- Follow the official [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- Run `cargo fmt --all` before every commit
- Keep `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean
- Document public items with `///`, and say why as well as what: most of MIMUS's rules come from something observed on the wire, and the comment is where that observation belongs
- No `unsafe` code without discussion first

## Commit Convention

We follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <short description>

[optional body]

[optional footer]
```

**Types:**

| Type       | Description                                             |
|------------|---------------------------------------------------------|
| `feat`     | New feature                                             |
| `fix`      | Bug fix                                                 |
| `refactor` | Code change that neither fixes a bug nor adds a feature |
| `docs`     | Documentation only                                      |
| `test`     | Adding or updating tests                                |
| `perf`     | Performance improvement                                 |
| `ci`       | CI configuration                                        |
| `chore`    | Build process, dependencies, tooling                    |

**Scopes:** `mimus-wire`, `mimus-profile`, `mimus-emit`, `mimus-calibrate`, `mimus-serve`, `cli`, `catalogue`, `bench`

**Examples:**

```
feat(mimus-wire): add a PVData walker that locates timeStamp in a value
feat(mimus-serve): add the UDP search responder
fix(cli): reject a rate or tick that no session could use
docs(catalogue): document coverage and how to check it
test(mimus-serve): add end-to-end tests over a real socket
```

## Pull Request Process

1. **One concern per PR.** Don't mix a bug fix with a refactor.
2. **Describe what and why.** The PR description should explain the change, not just list files.
3. **Link related issues.** Use `Closes #42` or `Relates to #17`.
4. **All CI checks must pass:** formatting, Clippy and the documentation with warnings denied, and the whole test suite.
5. **Request review** from a maintainer.
6. **Squash commits** before merging if the history is noisy. Keep meaningful commits if each one is self-contained.

### PR Template

```markdown
## What

Brief description of the change.

## Why

What problem does this solve? Link to issue if applicable.

## How

Key implementation details or design decisions.

## Testing

How was this tested? New tests added? Against a reference IOC?

## Checklist

- [ ] `cargo fmt --all` applied
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] Documentation updated (if applicable)
- [ ] No `unsafe` code added (or discussed and approved)
```

## Testing

### Running Tests

`--workspace` matters: without it, `cargo` runs only the command-line crate's tests, a small fraction of the suite.

```bash
# The same checks as continuous integration
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps

# Tests for one crate
cargo test -p mimus-wire

# One test
cargo test -p mimus-wire header::tests::rejects_bad_magic

# With output
cargo test --workspace -- --nocapture
```

Changes to calibration or serving should also be checked against a reference IOC, as the [README](README.md#quick-start) describes. Changes to the catalogue have their own check procedure in the [catalogue README](catalogue/README.md#checking-the-catalogue-on-a-new-build).

### Writing Tests

- Place unit tests in a `#[cfg(test)] mod tests` block at the bottom of the file
- Tests that need a real socket go in `crates/mimus-serve/tests/`, against the hand-written client there, which builds every message itself rather than through the server's message code
- Tests of the `mimus` command itself, run as a user runs it, go in `tests/cli.rs`
- Use descriptive test names that state the behavior: `a_truncated_search_response_is_an_error_not_a_guess`
- Test edge cases: empty and truncated messages, both byte orders, values that change length

## Documentation

- **Code docs:** Use `///` for public items. Run `cargo doc --workspace --no-deps --open` to check the rendering.
- **README:** The root README covers usage; the catalogue README covers what calibration reaches and how to check it. Keep the two consistent with the code.

## License

By contributing to MIMUS-EPICS, you agree that your contributions will be licensed under the [MIT License](LICENSE).

---

Questions? Open an issue on [GitHub](https://github.com/clemkraw/mimus-epics/issues).
