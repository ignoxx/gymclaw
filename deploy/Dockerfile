# GymClaw: one Rust binary (bot, agent, scheduler, dashboard) on Debian slim.
# Build from repo root: docker build -f deploy/Dockerfile .
FROM rust:1-bookworm AS build
# libheif from bookworm-backports (1.19): bookworm's 1.15 rejects current iPhone HEICs.
RUN echo "deb http://deb.debian.org/debian bookworm-backports main" > /etc/apt/sources.list.d/bookworm-backports.list \
    && apt-get update && apt-get install -y --no-install-recommends pkg-config clang \
    && apt-get install -y --no-install-recommends -t bookworm-backports libheif-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web ./web
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/gymclaw /usr/local/bin/gymclaw

FROM debian:bookworm-slim
RUN echo "deb http://deb.debian.org/debian bookworm-backports main" > /etc/apt/sources.list.d/bookworm-backports.list \
    && apt-get update && apt-get install -y --no-install-recommends ca-certificates tini curl \
    && apt-get install -y --no-install-recommends -t bookworm-backports libheif1 libheif-plugin-libde265 \
    && rm -rf /var/lib/apt/lists/*
# uid 1000 = the old image's `node` user, so existing volumes stay writable.
RUN useradd --uid 1000 --create-home --shell /usr/sbin/nologin gymclaw
WORKDIR /opt/gymclaw
COPY --from=build /usr/local/bin/gymclaw /usr/local/bin/gymclaw
COPY gymclaw/assets/workout-guide ./assets/workout-guide
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
