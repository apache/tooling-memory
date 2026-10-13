#!/bin/sh
# Chrome from the official Playwright image, which carries the libraries a host may lack.
# The reader's browser tests run it when no Chrome is installed (crates/web/tests/
# reader_browser.rs), and `CHROME=scripts/chrome-container.sh` asks for it outright; the
# same arrangement as apache/tooling-agenda's end-to-end suite. Arguments go to Chrome. It
# shares the host's network, so 127.0.0.1 is the test's own server. Of files it is handed
# this checkout alone, read-only and at the same path, so `--load-extension` can name the
# companion extension under crates/web/companion; what else it loads it fetches.
#
#   CONTAINER_RUNTIME   podman or docker (default: podman, else docker)
#   PLAYWRIGHT_IMAGE    the image (default below; the first run pulls it)
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
image=${PLAYWRIGHT_IMAGE:-mcr.microsoft.com/playwright:v1.61.1-noble}
runtime=${CONTAINER_RUNTIME:-}
if [ -z "$runtime" ]; then
    if command -v podman >/dev/null 2>&1; then
        runtime=podman
    elif command -v docker >/dev/null 2>&1; then
        runtime=docker
    else
        echo "no container runtime for Chrome: install podman or docker, or a Chrome" >&2
        exit 1
    fi
fi

exec "$runtime" run --rm --network host -v "$repo:$repo:ro" "$image" \
    sh -c 'exec /ms-playwright/chromium-*/chrome-linux64/chrome "$@"' chrome "$@"
