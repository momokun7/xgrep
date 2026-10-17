"""Run a benchmark shell script on Linux or macOS, and stop with a clear message on Windows.

The benchmarks measure xg against ripgrep/fd on POSIX systems (hyperfine, /proc,
`time -v`), so numbers from Windows would not be comparable. Called from
`mise run bench*` as `uv run bench/posix.py ...`.

Usage: uv run bench/posix.py <script.sh> [args...]
"""

import os
import shutil
import subprocess
import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2
    if os.name == "nt":
        print(
            "The benchmarks are POSIX-only (Linux or macOS). "
            "Run them on a Linux/macOS machine or inside WSL.",
            file=sys.stderr,
        )
        return 1
    bash = shutil.which("bash")
    if bash is None:
        print("bash was not found on PATH.", file=sys.stderr)
        return 1
    script = Path(sys.argv[1]).resolve()
    return subprocess.call([bash, str(script), *sys.argv[2:]])


if __name__ == "__main__":
    sys.exit(main())
