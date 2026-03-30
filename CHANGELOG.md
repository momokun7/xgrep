# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.6] - 2026-03-31

### Added
- `--version` / `-V` flag to display version from Cargo.toml
- Optional PATH positional argument (`xg "pattern" /path/to/dir`) to search without `cd`
- PATH argument also works with `xg init /path/to/dir`

### Fixed
- `--fresh` and `--changed` path doubling when xgrep root is a git subdirectory (#15)
- `--since` also affected by the same git-root-relative path issue

## [0.1.5] - 2026-03-25

### Added
- Version header (magic bytes + version) to trigram cache format for forward compatibility
- Strict input validation for MCP tool parameters (type checking for integers and booleans)
- Case-insensitive file type matching (`--type RUST` now works)
- Code coverage reporting in CI (cargo-tarpaulin + Codecov)
- CHANGELOG.md following Keep a Changelog format

### Fixed
- Suppress stderr output in MCP server mode to avoid interfering with JSON-RPC clients

### Security
- SAFETY comments on all unsafe blocks documenting invariants

## [0.1.4] - 2026-03-24

### Added
- MCP server mode with 5 tools (search, find_definitions, read_file, index_status, build_index)
- `--format llm` output mode optimized for AI context windows
- `--fresh` flag for on-demand index freshness checking
- Background index auto-rebuild (30s interval limit)
- Case-insensitive search (`-i` flag)
- Regex search (`-e` flag)
- File type filtering (`--type` / `-t`)
- Path pattern filtering
- JSON output (`--json`)
- Count mode (`-c`) and files-only mode (`-l`)
- Git-aware file filtering (`--changed`, `--since`)
- Incremental index builds with trigram cache
- Advisory file locking for concurrent build prevention
- Property-based tests and fuzz targets
- Multi-platform CI (Ubuntu, macOS, Windows)

### Security
- Path traversal prevention in MCP read_file tool
- SHA-pinned GitHub Actions for supply chain security
- Gitleaks, Trivy, and cargo-audit in CI pipeline

## [0.1.0] - 2026-03-23

### Added
- Initial release
- Trigram inverted index (Russ Cox method)
- CLI search with `xg` command
- Binary index format v2 with LEB128 varint + delta encoding
- Memory-mapped I/O for index reading
- Parallel file scanning with rayon
- SIMD-accelerated pattern matching via memchr

[Unreleased]: https://github.com/momokun7/xgrep/compare/v0.1.6...HEAD
[0.1.6]: https://github.com/momokun7/xgrep/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/momokun7/xgrep/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/momokun7/xgrep/compare/v0.1.0...v0.1.4
[0.1.0]: https://github.com/momokun7/xgrep/releases/tag/v0.1.0
