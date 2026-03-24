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
- **Linting**: `cargo clippy -- -D warnings` (enforced by CI)
- **Testing**: `cargo test` (all tests must pass)

A pre-commit hook runs fmt, clippy, and tests automatically.

## Pull Requests

1. Fork the repository
2. Create a feature branch (`git checkout -b feat/my-feature`)
3. Make your changes with tests
4. Ensure `cargo fmt && cargo clippy -- -D warnings && cargo test` passes
5. Submit a pull request

## Code Style

- Follow existing patterns in the codebase
- Add tests for new functionality
- Keep commits focused and well-described

## Reporting Issues

- Use GitHub Issues for bug reports and feature requests
- For security vulnerabilities, see [SECURITY.md](SECURITY.md)
