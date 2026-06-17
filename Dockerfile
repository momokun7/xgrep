FROM rust:1.82-slim-bookworm@sha256:2893c948181a4f145098f8461ba4dfc61d5b85e7f3c46d18dddc099f0d73217c AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY rust/ ./
RUN cargo build --release
FROM debian:bookworm-slim@sha256:35ae959f6e83ffb465e7614d27b4fddd28288caa551fbca2798367567cce80d3
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    ca-certificates \
    ripgrep \
    time \
    hyperfine \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/xgrep /usr/local/bin/xgrep
COPY bench/docker-bench.sh /usr/local/bin/bench.sh
RUN chmod +x /usr/local/bin/bench.sh
RUN git clone --depth 1 https://github.com/BurntSushi/ripgrep /test/ripgrep-src
WORKDIR /test/ripgrep-src
CMD ["bench.sh"]
