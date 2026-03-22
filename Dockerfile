FROM rust:1.93-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY rust/ ./

RUN cargo build --release

FROM debian:bookworm-slim

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
