# Contributing to xgrep

Thank you for your interest in contributing!

## Getting Started

```bash
git clone https://github.com/momokun7/xgrep.git
cd xgrep/rust
cargo build
cargo test
```

## Development

- **Rust edition**: 2021, MSRV 1.85
- **Formatting**: `cargo fmt` (enforced by CI)
- **Linting**: `cargo clippy --all-targets -- -D warnings` (enforced by CI)
- **Testing**: `cargo test` (all tests must pass)

`./scripts/check.sh` runs all of these, with and without default features, the same way CI does. Run it before opening a pull request (`--quick` skips the no-default-features pass).

## Pull Requests

1. Fork the repository
2. Create a feature branch (`git checkout -b feat/my-feature`)
3. Make your changes with tests (every bug fix gets a regression test)
4. Ensure `./scripts/check.sh` passes
5. Submit a pull request and fill in the template (Risk, Testing, How to revert)

## Review

See [REVIEW.md](REVIEW.md) for what reviewers look for and the risk tiers.

- Before opening a Medium or High risk PR, run an AI review locally with Claude Code: `/code-review` (use `/code-review high` and `/security-review` for High risk) and fix or answer every finding.
- CI must be green and every review thread resolved before merge. Answer each finding by fixing it or resolving the thread with a reason.
- AI review posts findings only; approval is a human decision.
- Keep PRs small and revertible. If a review goes three rounds without converging, step back and reconsider the approach.

## Releasing (maintainer)

Releases use [release-please](https://github.com/googleapis/release-please). It does not open release PRs on its own; when you want to release, open one from your machine so CI runs on it:

```bash
npx release-please@17.11.2 release-pr \
  --repo-url momokun7/xgrep \
  --token "$(gh auth token)" \
  --config-file release-please-config.json \
  --manifest-file .release-please-manifest.json
```

This computes the next version and CHANGELOG from the conventional commits since the last release. The whole repository is one release-please package: commits anywhere count, and `rust/Cargo.toml`, `rust/Cargo.lock` and `rust/CHANGELOG.md` are updated. Before 1.0, `feat` bumps the patch version and breaking changes bump the minor version. The version is pinned because the `Cargo.lock` update relies on how this release-please version parses TOML. Add `--release-as X.Y.Z` to choose the version yourself. Review the PR: a merge commit repeats the PR title in its body, which release-please lists as a second entry, so remove duplicate lines from both `rust/CHANGELOG.md` and the PR description (the release notes are taken from the description). Then let CI pass and merge it; the Release Please workflow then tags the release and starts the release build.

## Code Style

- Follow existing patterns in the codebase
- Add tests for new functionality
- Keep commits focused and well-described

## Reporting Issues

- Use GitHub Issues for bug reports and feature requests
- For security vulnerabilities, see [SECURITY.md](SECURITY.md)
