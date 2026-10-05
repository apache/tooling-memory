# The harness image: every harness `memory launch` knows, at pinned versions, on one base,
# with the memory binary where the sandbox expects it, and Python for workloads that are
# programs rather than harnesses, such as a Gofannon agent. The same image on every machine is
# the point; nothing here depends on what a developer's WSL or laptop contains.
#
# Build with `make image`, which copies the release binary in beside this file.
FROM ubuntu:24.04

ARG NODE_VERSION=24.21.0
ARG CLAUDE_CODE=2.1.287
ARG CODEX=0.160.0
ARG GEMINI_CLI=0.62.0
ARG COPILOT=1.0.91
ARG OPENCODE=1.18.34
ARG QWEN_CODE=0.24.7
ARG GROK=1.0.46

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl git xz-utils ripgrep procps less python3 python3-venv python3-pip \
 && rm -rf /var/lib/apt/lists/*

# Node from the official tarball into /usr/local, so npm's global bin is /usr/local/bin.
RUN curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
      | tar -xJ -C /usr/local --strip-components=1 \
 && node --version

RUN npm install -g --no-fund --no-audit \
      "@anthropic-ai/claude-code@${CLAUDE_CODE}" \
      "@openai/codex@${CODEX}" \
      "@google/gemini-cli@${GEMINI_CLI}" \
      "@github/copilot@${COPILOT}" \
      "opencode-ai@${OPENCODE}" \
      "@qwen-code/qwen-code@${QWEN_CODE}" \
      "@xai-official/grok@${GROK}" \
 && npm cache clean --force \
 && claude --version && codex --version && gemini --version && copilot --version && opencode --version \
 && qwen --version && grok --version

# Muse Code is a launcher script that fetches a statically linked binary from Meta's
# release channel; the channel, not a version, is what pins it, so the version the
# image carries is the channel's at build time, printed here. The launcher is told
# never to update at run time.
RUN curl -fsSL https://dev.meta.ai/install.sh | MUSE_INSTALL_DIR=/usr/local/bin bash \
 && MUSE_LOGIN=0 MUSE_NO_AUTO_UPDATE=1 muse --version
ENV MUSE_NO_AUTO_UPDATE=1

# The harness runs as `user`, uid 1000, with its private home mounted in at /home/user.
RUN userdel -r ubuntu 2>/dev/null || true \
 && useradd -u 1000 -m -d /home/user -s /bin/bash user \
 && mkdir -p /opt/memory /run/memory /work /memory /host-home \
 && chown user:user /work

COPY memory /opt/memory/memory
RUN chmod 0755 /opt/memory/memory && /opt/memory/memory --version

LABEL org.opencontainers.image.title="durable-memory harness" \
      org.opencontainers.image.source="https://github.com/sstriker/durable-memory"
