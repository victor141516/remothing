# syntax=docker/dockerfile:1.7
FROM ubuntu:24.04 AS base
ENV DEBIAN_FRONTEND=noninteractive

FROM base AS build-server
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl build-essential pkg-config clang libxcb1-dev \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
ENV PATH=/root/.cargo/bin:$PATH
WORKDIR /src
COPY Cargo.toml Cargo.lock* ./
COPY server server
COPY testapp testapp
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release \
    && cp target/release/remothing-server target/release/rt-testapp /usr/local/bin/

FROM node:22-slim AS build-client
WORKDIR /client
COPY client/package.json ./
RUN --mount=type=cache,target=/root/.npm npm install --no-audit --no-fund
COPY client .
RUN npm run build

FROM base AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
      xvfb x11-utils x11-xkb-utils xkb-data openbox xterm fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build-server /usr/local/bin/remothing-server /usr/local/bin/rt-testapp /usr/local/bin/
COPY --from=build-client /client/dist /opt/remothing/web
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
