FROM node:24-bookworm AS node
FROM rust:1-bookworm AS rust

# Match the Linux release runner's oldest supported userspace rather than
# linking the distributable application against Nix store paths.
FROM ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive
ENV PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ENV RUSTUP_HOME=/usr/local/rustup

COPY --from=node /usr/local/bin/ /usr/local/bin/
COPY --from=node /usr/local/lib/node_modules/ /usr/local/lib/node_modules/
COPY --from=rust /usr/local/cargo/ /usr/local/cargo/
COPY --from=rust /usr/local/rustup/ /usr/local/rustup/

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    file \
    git \
    gstreamer1.0-alsa \
    gstreamer1.0-libav \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-pulseaudio \
    gstreamer1.0-tools \
    libasound2-dev \
    libasound2-plugins \
    libayatana-appindicator3-dev \
    libgirepository1.0-dev \
    librsvg2-dev \
    libssl-dev \
    libwebkit2gtk-4.1-dev \
    libxdo-dev \
    patchelf \
    pkg-config \
    wget \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
