# GymClaw: one Rust binary (bot, agent, scheduler, dashboard) on Debian slim.
# Build from repo root: docker build -f deploy/Dockerfile .
# cargo-chef: dependencies compile in their own layer keyed only on Cargo.toml/Cargo.lock, so with
# CI's layer cache (.github/workflows/image.yml) a source-only change recompiles just this crate.
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef
# libheif from bookworm-backports (1.19): bookworm's 1.15 rejects current iPhone HEICs.
RUN echo "deb http://deb.debian.org/debian bookworm-backports main" > /etc/apt/sources.list.d/bookworm-backports.list \
    && apt-get update && apt-get install -y --no-install-recommends pkg-config clang \
    && apt-get install -y --no-install-recommends -t bookworm-backports libheif-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src

FROM chef AS plan
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=plan /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web ./web
RUN cargo build --release --locked && cp target/release/gymclaw /usr/local/bin/gymclaw

FROM debian:bookworm-slim
RUN echo "deb http://deb.debian.org/debian bookworm-backports main" > /etc/apt/sources.list.d/bookworm-backports.list \
    && apt-get update && apt-get install -y --no-install-recommends ca-certificates tini curl \
    && apt-get install -y --no-install-recommends -t bookworm-backports libheif1 libheif-plugin-libde265 \
    && rm -rf /var/lib/apt/lists/*
# uid 1000 = the old image's `node` user, so existing volumes stay writable.
RUN useradd --uid 1000 --create-home --shell /usr/sbin/nologin gymclaw
WORKDIR /opt/gymclaw
COPY --from=build /usr/local/bin/gymclaw /usr/local/bin/gymclaw
COPY assets/workout-guide ./assets/workout-guide
COPY config ./config
COPY deploy/entrypoint.sh deploy/backup.sh deploy/restore.sh /usr/local/bin/
RUN mkdir -p data openclaw /backups && chown gymclaw:gymclaw data openclaw /backups

USER gymclaw
ENV GYMCLAW_DATA_DIR=/opt/gymclaw/data \
    GYMCLAW_ASSETS_DIR=/opt/gymclaw/assets/workout-guide \
    GYMCLAW_TURN_LOG_DIR=/opt/gymclaw/data/turns \
    GYMCLAW_LEGACY_USER_MD=/opt/gymclaw/openclaw/USER.md \
    NO_PROXY=localhost,127.0.0.1,::1 \
    no_proxy=localhost,127.0.0.1,::1
ENTRYPOINT ["tini", "-s", "--", "entrypoint.sh"]
