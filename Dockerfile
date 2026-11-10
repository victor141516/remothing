# syntax=docker/dockerfile:1.7
# Optional Linux deployment: the native `remothing` binary plus Xvfb and X tools.
FROM ubuntu:24.04 AS base
ENV DEBIAN_FRONTEND=noninteractive

FROM node:22-slim AS build-client
WORKDIR /client
COPY client/package.json ./
RUN --mount=type=cache,target=/root/.npm npm install --no-audit --no-fund
COPY client .
RUN npm run build

FROM base AS build-server
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl build-essential pkg-config clang libxcb1-dev \
      libavcodec-dev libavformat-dev libavutil-dev libswscale-dev \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
ENV PATH=/root/.cargo/bin:$PATH
WORKDIR /src
COPY Cargo.toml Cargo.lock* ./
COPY crates crates
COPY testapp testapp
# Web clients are embedded into the binary.
COPY --from=build-client /client/dist client/dist
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p remothing -p rt-testapp \
    && cp target/release/remothing target/release/rt-testapp /usr/local/bin/

FROM base AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
      xvfb x11-utils x11-xkb-utils xkb-data openbox xterm fonts-dejavu-core \
      libavcodec60 libavformat60 libswscale7 \
    && rm -rf /var/lib/apt/lists/*
# NVENC needs the driver's video libraries injected by `--gpus`.
ENV NVIDIA_DRIVER_CAPABILITIES=compute,utility,video
COPY --from=build-server /usr/local/bin/remothing /usr/local/bin/rt-testapp /usr/local/bin/
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh
EXPOSE 8080 8081
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]

# Full Ubuntu desktop for trying it out: `docker build --target desktop -t remothing-desktop .`
FROM runtime AS desktop
RUN apt-get update && apt-get install -y --no-install-recommends \
      xfce4 xfce4-terminal thunar mousepad ristretto dbus-x11 at-spi2-core \
      fonts-noto-core fonts-noto-color-emoji adwaita-icon-theme-full ca-certificates curl gpg locales \
    && install -d -m 0755 /etc/apt/keyrings \
    && curl -fsSL https://packages.mozilla.org/apt/repo-signing-key.gpg -o /etc/apt/keyrings/packages.mozilla.org.asc \
    && echo "deb [signed-by=/etc/apt/keyrings/packages.mozilla.org.asc] https://packages.mozilla.org/apt mozilla main" \
         > /etc/apt/sources.list.d/mozilla.list \
    && printf 'Package: *\nPin: origin packages.mozilla.org\nPin-Priority: 1000\n' > /etc/apt/preferences.d/mozilla \
    && apt-get update && apt-get install -y --no-install-recommends firefox \
    && sed -i 's/# es_ES.UTF-8/es_ES.UTF-8/' /etc/locale.gen && locale-gen \
    && useradd -m -s /bin/bash user \
    && rm -rf /var/lib/apt/lists/*
ENV LANG=es_ES.UTF-8

# Default build target: the slim image (simulated display + openbox/xterm).
FROM runtime
