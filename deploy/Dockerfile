# GymClaw gateway: official OpenClaw image + GymClaw's Python domain.
# Build from repo root: docker build -f deploy/Dockerfile .
FROM ghcr.io/openclaw/openclaw:2026.7.1

USER root
COPY --from=ghcr.io/astral-sh/uv:0.12 /uv /usr/local/bin/uv
# Base image ships Python 3.11; GymClaw needs 3.12+.
ENV UV_PYTHON_INSTALL_DIR=/opt/python UV_LINK_MODE=copy UV_NO_CACHE=1
WORKDIR /opt/gymclaw
COPY pyproject.toml alembic.ini ./
COPY gymclaw ./gymclaw
# Precompile bytecode: the container is read-only, so without this every gymclaw-tool
# call recompiles all imported modules (~3s per call).
RUN uv venv --python 3.13 .venv && uv pip install --python .venv/bin/python -e . \
    && .venv/bin/python -m compileall -q -j0 -x '/(test|tests|idlelib|lib2to3)/' /opt/python .venv gymclaw \
    && chmod -R a+rX /opt/python /opt/gymclaw

COPY scripts/gymclaw-tool ./scripts/gymclaw-tool
COPY config ./config
# Repo-owned workspace files. Entrypoint syncs them into the persistent workspace
# volume mounted at /opt/gymclaw/openclaw; plugins load straight from here.
COPY openclaw ./openclaw.dist
COPY deploy/entrypoint.sh deploy/backup.sh deploy/restore.sh /usr/local/bin/
RUN mkdir -p openclaw data /home/node/.openclaw-gymclaw /backups \
    && chown node:node openclaw data /home/node/.openclaw-gymclaw /backups

USER node
# OPENCLAW_* match what `--profile gymclaw` derives. Cron children inherit them too, so
# GymClaw's job declarations are identical whether synced from a shell or a callback.
ENV PYTHONDONTWRITEBYTECODE=1 \
    GYMCLAW_DB_URL=sqlite:////opt/gymclaw/data/gymclaw.db \
    GYMCLAW_OPENCLAW_PROFILE=gymclaw \
    OPENCLAW_PROFILE=gymclaw \
    OPENCLAW_STATE_DIR=/home/node/.openclaw-gymclaw \
    OPENCLAW_CONFIG_PATH=/home/node/.openclaw-gymclaw/openclaw.json \
    NODE_USE_ENV_PROXY=1 \
    NO_PROXY=localhost,127.0.0.1,::1 \
    no_proxy=localhost,127.0.0.1,::1
ENTRYPOINT ["tini", "-s", "--", "entrypoint.sh"]
