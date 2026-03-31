# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-04-01

### Added
- Structured error types with `thiserror` — library consumers can now match on `XgrepError` variants (#25)
- `InvalidArgument` error variant for CLI argument validation errors
- `Config` struct with `quiet` field for controlling stderr output (#28)
- `with_config()` builder method on `Xgrep` for runtime configuration
- criterion.rs benchmark infrastructure (`cargo bench`) for search latency, index build, and candidate resolution (#26)
- Positional trigrams design document for future index format v3 (#29)

### Changed
- **Breaking:** Public API return type changed from `anyhow::Result` to `Result<T, XgrepError>` (#25)
- **Breaking:** `search` module functions are now `pub(crate)` — use `Xgrep::search()` instead
- RegexMatcher: restored early return for non-matching files + zero-copy UTF-8 with `Cow` (#27)
- CaseInsensitiveMatcher: hybrid approach — full-file SIMD rejection + per-line processing (#27)
- Global `MCP_MODE` replaced with struct-based `Config` propagation (#28)
- Slimmed down README (282 → 145 lines), moved details to `docs/benchmarks.md`

### Removed
- Global `AtomicBool` state (`MCP_MODE`) from `mcp.rs`
- `anyhow` dependency from library layer (retained in CLI binary)

## [0.2.1] - 2026-03-31

### Changed
- Unified crates.io and npm publish into single release workflow (single `v*` tag)
- npm package version auto-synced from Cargo.toml (no manual package.json updates)
- Removed standalone npm-publish.yml

### Fixed
- Windows path separator in test assertions

## [0.2.0] - 2026-03-31

### Added
- Single file as PATH argument: `xg "pattern" /path/to/file.rs` (#19)
- `--find` mode for file discovery by glob or substring: `xg --find "*.rs"` (#18)
- `--find` combined with `-t`, `--changed`, `--exclude` for filtered file discovery
- `--absolute-paths` flag and `XGREP_ABSOLUTE_PATHS` env var (#22)
- `--list-types` flag to show all supported file types (#20)
- `--exclude` flag for path-based result filtering (repeatable)
- `--no-hints` flag and `XGREP_NO_HINTS` env var to suppress regex pattern hints
- `XGREP_LLM_CONTEXT` env var for default `--format llm` context lines (#21)
- `xg status` subcommand to show index freshness, file count, and age
- Regex pattern hint detection: warns when literal search pattern looks like regex
- 17 new file types: kotlin, swift, dart, gradle, proto, zig, elixir, php, scala, r, lua, haskell, terraform, jsx, vue, svelte
- Reproducible `--find` benchmark script (`benchmarks/bench_find.sh`)
- CLI integration tests for exit codes and `--find` mode

### Changed
- Exit code for missing pattern: `1` -> `2` (usage error, not "no match") (#23)

### Fixed
- Exit codes now follow ripgrep convention: 0=match, 1=no match, 2=error (#23)

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

[Unreleased]: https://github.com/momokun7/xgrep/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/momokun7/xgrep/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/momokun7/xgrep/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/momokun7/xgrep/compare/v0.1.6...v0.2.0
[0.1.6]: https://github.com/momokun7/xgrep/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/momokun7/xgrep/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/momokun7/xgrep/compare/v0.1.0...v0.1.4
[0.1.0]: https://github.com/momokun7/xgrep/releases/tag/v0.1.0
