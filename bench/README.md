# Benchmarks

## Quick Start

```bash
# Small (xgrep's own source, ~30 files)
./bench/run.sh small

# Medium (ripgrep source, ~200 files, auto-downloads)
./bench/run.sh medium

# Large (Linux kernel, ~90K files, requires manual download)
git clone --depth 1 https://github.com/torvalds/linux.git bench/linux-src
./bench/run.sh large

# All benchmarks
./bench/run.sh all
```

With [mise](https://mise.jdx.dev): `mise run bench small` (also `mise run bench:fair`, `mise run bench:find`).
The benchmarks are POSIX-only (Linux or macOS); on Windows the tasks stop with a message.

## Requirements

- [hyperfine](https://github.com/sharkdp/hyperfine) - `cargo install hyperfine`
- [ripgrep](https://github.com/BurntSushi/ripgrep) - `cargo install ripgrep`
- xgrep - `cargo install xgrep-search`

## Results

Results are saved to `bench/results/` as JSON (machine-readable) and Markdown.
