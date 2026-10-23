# syntax=docker/dockerfile:1
#
# The deploy-bundle image (U51): `asc` built with only the `serve` feature, so the image carries no
# VFS, no git-compat and no SSH listener. Build from the repository root:
#   docker build -f deploy/Dockerfile -t asc-serve:deploy .
# `docker compose -f deploy/compose.yml up -d --wait` builds it for you. For the full-featured
# production image (git smart-HTTP, SSH) use deploy/Dockerfile.serve.

FROM rust:1.95.0-trixie@sha256:f49565f188ee00bc2a18dd418183f2c5f23ef7d6e691890517ed341a598f67c3 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY proto ./proto
# Read at build time: tests/turbo is a workspace member, and the asc binary embeds the cheatsheet
# and the Windows packaging files.
COPY tests/turbo ./tests/turbo
COPY docs/CHEATSHEET.md ./docs/CHEATSHEET.md
COPY packaging/windows ./packaging/windows
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    CARGO_INCREMENTAL=0 CARGO_NET_RETRY=5 \
    cargo build --release --locked -p asc --no-default-features --features serve \
 && install -D -m 0755 target/release/asc /out/asc

FROM debian:trixie-20261020-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
# ca-certificates: HTTPS to buckets and issuers. curl: the container health checks.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 asc \
 && useradd --system --uid 10001 --gid 10001 --home-dir /var/lib/asc --no-create-home --shell /usr/sbin/nologin asc \
 && install -d -o 10001 -g 10001 -m 0750 /var/lib/asc /etc/asc
COPY --from=build /out/asc /usr/local/bin/asc
ENV ASC_SERVE_CONFIG=/etc/asc/asc-serve.toml
WORKDIR /var/lib/asc
VOLUME /var/lib/asc
USER 10001:10001
EXPOSE 7890 9464
ENTRYPOINT ["/usr/local/bin/asc"]
CMD ["serve"]
