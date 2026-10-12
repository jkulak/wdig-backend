# Two stages: the builder compiles with the stable toolchain on Debian 13; the runtime is a
# slim Debian 13 with the binary, CA certificates and wget for the compose healthcheck, run
# as a user of its own. Development runs the builder stage with the source mounted.
FROM rust:1-slim-trixie AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock* rust-toolchain.toml ./
COPY src ./src
ARG APP_VERSION
ARG GIT_SHA
ENV APP_VERSION=${APP_VERSION} GIT_SHA=${GIT_SHA}
RUN cargo build --release --locked 2>/dev/null || cargo build --release

FROM debian:trixie-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates wget \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /nonexistent --shell /usr/sbin/nologin wdig
COPY --from=builder /app/target/release/wdig /usr/local/bin/wdig
USER wdig
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/wdig"]
CMD ["serve"]
