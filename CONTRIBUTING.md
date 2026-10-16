# Contributing to MIMUS-EPICS

Thank you for your interest in contributing to MIMUS. This document provides guidelines and information for contributors.

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

MIMUS is a PVAccess traffic generator written in Rust. It reproduces what a PVXS IOC puts on 
the wire, at scales no fleet of soft IOCs can reach, so that archivers, gateways and displays can be 
measured under a load that is both realistic and reproducible. Before contributing, we recommend:

1. Reading the [architecture documentation](docs/architecture.md) to understand the design and goals
2. Familiarizing yourself with [EPICS](https://epics-controls.org/) and the [PV Access protocol](https://docs.epics-controls.org/en/latest/pv-access/overview.html)
3. Having a working knowledge of Rust and async programming with [Tokio](https://tokio.rs/)

## Development Setup

### Prerequisites

- **Rust** 1.95.0 — pinned by `rust-toolchain.toml`; [rustup](https://rustup.rs/) installs it automatically on the first `cargo` invocation
- **PVXS** with `softIocPVX`, for calibration and conformance testing only. Not needed to build, and not needed to serve an existing profile.

## How to Contribute

### Reporting Bugs

Open an issue on GitHub with:

- A clear, descriptive title
- Steps to reproduce the behavior
- Expected vs. actual behavior
- MIMUS version, Rust version, OS
- Relevant log output (with `RUST_LOG=mimus=debug`)

### Suggesting Features

Open an issue with the `enhancement` label. Describe:

- The problem your feature would solve
- Your proposed approach
- Any alternatives you considered

### Submitting Code

1. Fork the repository
2. Create a feature branch from `main` (`git checkout -b feat/my-feature`)
3. Make your changes following the [coding standards](#coding-standards)
4. Add or update tests as appropriate
5. Run the full test suite (`cargo test`)
6. Run the linter (`cargo clippy -- -D warnings`)
7. Run the formatter (`cargo fmt`)
8. Submit a pull request

## Coding Standards

### Rust Style

- Follow the official [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- Run `cargo fmt` before every commit — no exceptions
- Run `cargo clippy -- -D warnings` and resolve all warnings
- All public items must have doc comments (`///`)

## Commit Convention

We follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <short description>

[optional body]

[optional footer]
```

**Types:**

| Type       | Description                                      |
|------------|--------------------------------------------------|
| `feat`     | New feature                                      |
| `fix`      | Bug fix                                          |
| `refactor` | Code change that neither fixes a bug nor adds a feature |
| `docs`     | Documentation only                               |
| `test`     | Adding or updating tests                         |
| `perf`     | Performance improvement                          |
| `ci`       | CI/CD configuration                              |
| `chore`    | Build process, dependencies, tooling             |

**Scopes:** `calibrate`, `emit`, `profile`, `serve`, `wire`

**Examples:**

```
feat(calibrate): implement windowed entropy computation
fix(emit): handle malformed CA beacon packets gracefully
docs(profile): add key schema documentation
test(serve): add segmenter regime-change detection tests
perf(wire): batch RocksDB writes with WriteBatch
```

## Pull Request Process

1. **One concern per PR.** Don't mix a bug fix with a refactor.
2. **Describe what and why.** The PR description should explain the change, not just list files.
3. **Link related issues.** Use `Closes #42` or `Relates to #17`.
4. **All CI checks must pass.** This includes `cargo test`, `cargo clippy`, `cargo fmt --check`.
5. **Request review** from at least one maintainer.
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

How was this tested? New tests added?

## Checklist

- [ ] `cargo fmt` applied
- [ ] `cargo clippy -- -D warnings` passes
- [ ] `cargo test` passes
- [ ] Documentation updated (if applicable)
- [ ] No `unsafe` code added (or discussed and approved)
```

## Testing

### Running Tests

```bash
# All tests
cargo test

# Tests for a specific crate
cargo test -p mimus-wire

# A specific test
cargo test -p mimus-wire framing::tests::rejects_bad_magic

# With output
cargo test -- --nocapture
```

### Writing Tests

- Every public function should have at least one unit test
- Place unit tests in a `#[cfg(test)] mod tests` block at the bottom of the file
- Integration tests go in `tests/` at the workspace root
- Use descriptive test names: `a_truncated_search_response_is_an_error_not_a_guess`
- Test edge cases: empty windows, NaN/infinity values, zero-variance signals

## Documentation

- **Code docs:** Use `///` for public items. Run `cargo doc --open` to verify rendering.
- **Architecture docs:** Kept in `docs/` as Markdown files.
- **README:** Keep the root README concise : link to detailed docs rather than duplicating.
- **Changelog:** Maintained in `CHANGELOG.md` following [Keep a Changelog](https://keepachangelog.com/) format.

## License

By contributing to MIMUS-EPICS, you agree that your contributions will be licensed under the [MIT License](LICENSE).

---

Questions? Open a [discussion](https://github.com/clemkraw/mimus-epics/discussions) or reach out to the maintainers.