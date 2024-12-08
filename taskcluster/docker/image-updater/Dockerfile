FROM rust:1.83-bookworm AS builder
LABEL org.opencontainers.image.source=https://github.com/eijebong/image-updater

# %include ./
COPY /topsrcdir /src

RUN apt update && apt install -y libgit2-dev
WORKDIR /src

RUN cargo build --release
RUN strip /src/target/release/image-updater

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive
RUN apt update && \
    apt install -y libgit2-1.5 && \
    rm -rf /var/lib/{apt,dpkg,cache,log}/

WORKDIR /usr/local/bin
COPY --from=builder /src/target/release/image-updater /usr/local/bin/

