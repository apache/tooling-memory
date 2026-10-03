#!/bin/sh
# Refresh the web terminal's vendored libraries. Bump the versions here and in
# crates/web/static/vendor/LICENSES.md together.
set -eu
d="$(dirname "$0")/../crates/web/static/vendor"
get() { curl -fsSL "$1" -o "$d/$2"; }
get https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/lib/xterm.min.js xterm.min.js
get https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/css/xterm.min.css xterm.min.css
get https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0/lib/addon-fit.min.js addon-fit.min.js
get https://cdn.jsdelivr.net/npm/marked@15.0.12/marked.min.js marked.min.js
get https://cdn.jsdelivr.net/npm/mermaid@11.6.0/dist/mermaid.min.js mermaid.min.js
ls -la "$d"
