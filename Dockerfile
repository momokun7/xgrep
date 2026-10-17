FROM rust:1.85.1-slim-bookworm@sha256:9f841bbe9e7d8e37ceb96ed907265a3a0df7f44e3737d0b100e7907a679acb36 AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY rust/ ./
RUN cargo build --release --locked --bin xg
FROM debian:bookworm-slim@sha256:35ae959f6e83ffb465e7614d27b4fddd28288caa551fbca2798367567cce80d3
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    ca-certificates \
    ripgrep \
    time \
    hyperfine \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/xg /usr/local/bin/xg
COPY bench/docker-bench.sh /usr/local/bin/bench.sh
RUN chmod +x /usr/local/bin/bench.sh
RUN git clone --depth 1 https://github.com/BurntSushi/ripgrep /test/ripgrep-src
WORKDIR /test/ripgrep-src
CMD ["bench.sh"]
