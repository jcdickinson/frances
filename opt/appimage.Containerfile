FROM denoland/deno:bin-2.5.6 AS deno

FROM rust:1.95.0-bookworm

COPY --from=deno /deno /usr/local/bin/deno

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential \
        file \
        gzip \
        jq \
        libappindicator3-dev \
        librsvg2-dev \
        libssl-dev \
        libwebkit2gtk-4.1-dev \
        patchelf \
        pkg-config \
        wget \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add \
        aarch64-unknown-linux-musl \
        x86_64-unknown-linux-musl

WORKDIR /workspace
