# The harness image: every harness `memory launch` knows, at pinned versions, on one base,
# with the memory binary where the sandbox expects it, and Python for workloads that are
# programs rather than harnesses, such as a Gofannon agent. The same image on every machine is
# the point; nothing here depends on what a developer's WSL or laptop contains.
#
# Build with `make image`, which copies the release binary in beside this file.
FROM ubuntu:24.04

# The npm harnesses' versions are in package.json, and every package under them in
# package-lock.json; the rest are here.
ARG NODE_VERSION=24.22.2
ARG PI=1.4.0

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
# included, at the version and sha512 the lock names, and with package.json's overrides,
# which a global install would ignore. The harnesses' own bins, and not their dependencies',
# are linked into /usr/local/bin, as a global install links them and where the launcher
# starts a harness in the image.
COPY package.json package-lock.json /opt/harness/
RUN cd /opt/harness && npm ci --no-fund --no-audit \
 && npm cache clean --force \
 && node -e 'const h = "/opt/harness"; \
      for (const n of Object.keys(require(h + "/package.json").dependencies)) { \
        const b = require(h + "/node_modules/" + n + "/package.json").bin || {}; \
        for (const k of typeof b === "string" ? [n.split("/").pop()] : Object.keys(b)) \
          require("fs").symlinkSync(h + "/node_modules/.bin/" + k, "/usr/local/bin/" + k); \
      }' \
 && claude --version && codex --version && gemini --version && copilot --version && opencode --version \
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
      org.opencontainers.image.source="https://github.com/apache/tooling-memory"
