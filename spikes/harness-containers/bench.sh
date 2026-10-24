#!/bin/sh
# Inside a box with Node and the harnesses: harness-shaped work, timed in milliseconds.
# Start-up (Node, a harness's --version), small-file churn on the bind-mounted working
# tree and on /tmp, a copy of a large node_modules, and a CPU loop. One line per task: its
# time, or FAIL where the task did not complete, which a time would hide.
ms() { echo $(( $(date +%s%N) / 1000000 )); }
t() {
    name=$1; shift
    a=$(ms)
    if "$@" > /dev/null 2>&1; then b=$(ms); echo "$name $((b - a))"; else echo "$name FAIL"; fi
}
node_x20() { i=0; while [ $i -lt 20 ]; do node -e 0; i=$((i + 1)); done; }
claude_x5() { i=0; while [ $i -lt 5 ]; do claude --version; i=$((i + 1)); done; }
files() { mkdir -p "$1/f" && node -e '
const fs = require("fs"), d = process.argv[1];
for (let i = 0; i < 10000; i++) fs.writeFileSync(d + "/" + i + ".txt", "x".repeat(200));
let n = 0; for (const f of fs.readdirSync(d)) { fs.statSync(d + "/" + f); n += fs.readFileSync(d + "/" + f).length; }
' "$1/f" && rm -rf "$1/f"; }
copy_modules() { cp -r /usr/local/lib/node_modules/@google "$1/m" && rm -rf "$1/m"; }
cpu() { node -e 'let s = 0; for (let i = 0; i < 3e8; i++) s += i % 7; console.log(s)'; }
t node_start_x20 node_x20
t claude_version_x5 claude_x5
t files_10k_work files /work
t files_10k_tmp files /tmp
t copy_gemini_modules_tmp copy_modules /tmp
t cpu_loop cpu
