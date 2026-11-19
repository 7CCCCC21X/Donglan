# 容器方式（可选）：Google Cloud 上推荐 deploy/gcp/rt.sh 直接装在虚拟机上（systemd），这里给本地测试或别的平台用。
# docker build -t ruletaker .  （Python 版备用：docker build -t ruletaker-py python/）
FROM rust:1.85-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY src ./src
COPY web ./web
COPY tests ./tests
# 依赖按 Cargo.lock 锁定的版本和哈希；离线测试有一个失败就构建失败
RUN cargo test --release --locked -q && cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data ruletaker && mkdir -p /data && chown ruletaker /data
COPY --from=build /src/target/release/ruletaker /usr/local/bin/ruletaker
ENV STATE_DB=/data/ruletaker.sqlite3
VOLUME ["/data"]
USER ruletaker
CMD ["ruletaker", "run"]
