# The harness image: every harness `memory launch` knows, at pinned versions, on one base,
# with the memory binary where the sandbox expects it, and Python for workloads that are
# programs rather than harnesses, such as a Gofannon agent. The same image on every machine is
# the point; nothing here depends on what a developer's WSL or laptop contains.
#
# Build with `make image`, which copies the release binary in beside this file.
FROM ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac

ARG NODE_VERSION=24.21.0
ARG PI=1.1.0

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl git xz-utils ripgrep procps less python3 python3-venv python3-pip \
 && rm -rf /var/lib/apt/lists/*

# Node from the official tarball into /usr/local, so npm's global bin is /usr/local/bin.
RUN curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
      | tar -xJ -C /usr/local --strip-components=1 \
 && node --version

# The npm harnesses from the lockfile beside this file: every package, transitive ones
# included, at the version and sha512 the lock names. Their bins go on PATH.
COPY package.json package-lock.json /opt/harness/
RUN cd /opt/harness && npm ci --no-fund --no-audit \
 && npm cache clean --force
ENV PATH=/opt/harness/node_modules/.bin:$PATH
RUN claude --version && codex --version && gemini --version && copilot --version && opencode --version \
 && qwen --version && grok --version

# pi, installed as its own docs say, without its install scripts, which only phone home.
RUN npm install -g --no-fund --no-audit --ignore-scripts "@earendil-works/pi-coding-agent@${PI}" \
 && npm cache clean --force && pi --version

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
