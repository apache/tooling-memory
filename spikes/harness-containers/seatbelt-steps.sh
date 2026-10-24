#!/bin/sh
# Where a launch under our Seatbelt backend stops: `amem launch --harness command` with
# commands of growing reach, one launch each, printing what each exits with. A launch whose
# command dies on a signal exits 1 without a word (the relay's `unwrap_or(1)`), so a step
# that prints nothing and exits 1 is the place to look. AMEM names the binary; PYTHON the
# interpreter, /usr/bin/python3 by default.
here=$(cd "$(dirname "$0")" && pwd)
amem=$(realpath "${AMEM:-$here/../../target/debug/amem}")
python=${PYTHON:-/usr/bin/python3}
root=$(mktemp -d /tmp/am.XXXXXX)
work=$(mktemp -d "${TMPDIR:-/tmp}/steps.XXXXXX")
trap 'rm -rf "$root" "$work"' EXIT
"$amem" --root "$root" init --owner human:spike:me --domain dom-spike >/dev/null || exit 1
step() {
    name=$1
    shift
    OPENAI_API_KEY=sk-placeholder "$amem" --root "$root" launch --harness command --auth key \
        --workdir "$work" -- "$@" </dev/null > "$work/.out" 2>&1
    code=$?
    printf 'STEP  %-22s exit=%s\n' "$name" "$code"
    grep -v '^memory: ' "$work/.out" | sed 's/^/      /' | head -20
}
step true /usr/bin/true
step echo /bin/echo inside
step sh /bin/sh -c 'echo sh ok'
step sh-env /bin/sh -c 'echo "HOME=$HOME TMPDIR=$TMPDIR PATH=${PATH:-unset}"'
step python-version "$python" --version
step python-print "$python" -I -c 'print("python ok")'
step python-socket "$python" -I -c 'import socket; print(socket.if_nameindex())'
step relay-port /bin/sh -c 'echo "MEMORY_MCP_URL=$MEMORY_MCP_URL"'
