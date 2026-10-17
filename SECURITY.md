# Security Policy

## Supported Versions

Security fixes are released for the latest minor version of each package:
the `xgrep-search` crate and PyPI package, and the `xgrep` npm package.

| Version | Supported |
|---------|-----------|
| 0.8.x   | Yes       |
| < 0.8   | No        |

## Reporting a Vulnerability

If you discover a security vulnerability in xgrep, please report it responsibly.

**Do NOT open a public issue.**

Instead, use **GitHub's [private vulnerability reporting](https://github.com/momokun7/xgrep/security/advisories/new)** feature on this repository.

## Response Timeline

- Acknowledgment: best effort (typically within a few days)
- Fix release: as soon as practical

## Scope

- Index format parsing (malicious index files)
- MCP server input handling
- Path traversal in file operations
- Memory safety issues
