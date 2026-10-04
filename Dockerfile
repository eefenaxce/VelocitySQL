# syntax=docker/dockerfile:1

# Build stage. The workspace is pure Rust, so this needs a toolchain and nothing
# else. The cache mounts keep the dependency build across image builds, which is
# what makes a rebuild after a source change take seconds instead of minutes.
FROM rust:1-bookworm AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p vsql-server -p vsql-cli \
 && cp target/release/velocitysql-server target/release/velocitysql-cli /usr/local/bin/

# Runtime stage. The binaries need a libc and nothing else: there is no TLS and
# no HTTP client in this engine.
FROM debian:bookworm-slim

RUN useradd --system --uid 10001 --create-home --user-group velocitysql \
 && install -d -o velocitysql -g velocitysql /data

COPY --from=build /usr/local/bin/velocitysql-server /usr/local/bin/
COPY --from=build /usr/local/bin/velocitysql-cli /usr/local/bin/

# The snapshot path (`data/velocitysql.snapshot`) is relative to the working
# directory, so running from `/` puts it in the volume directly rather than in a
# `data/data` that nobody expects.
WORKDIR /
VOLUME ["/data"]

USER velocitysql
EXPOSE 5210

# The binary defaults to 127.0.0.1, which inside a container is only reachable
# from that container's own network namespace. Flags for a deployment follow the
# image name:
#     docker run … ghcr.io/eefenaxce/velocitysql --superuser-password s3cret
ENTRYPOINT ["velocitysql-server", "--host", "0.0.0.0"]

# There is no client in the image to speak the wire protocol with, so this only
# proves the port is accepting connections.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/5210"]
