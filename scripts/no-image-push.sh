#!/bin/sh
# Nothing in this repository pushes an image to a registry, and this fails when something
# would. The harness image holds harnesses under their vendors' own licences, Claude Code
# and Copilot CLI among them, which the project may not redistribute, and an image built
# from unreleased source is not something the project may publish either (images/harness/
# README.md says why). Each machine builds its own with `make image`.
#
# It reads every file git knows or would add, except Markdown, these two scripts and the
# guard and its tests, which name the same commands to refuse them. It fails on a shell
# line, a workflow step, a Makefile recipe or a Dockerfile that runs a push, logs in to a
# registry, or grants a workflow `packages: write`, and on Rust that passes "push" or
# "login" as a command's argument. A file it cannot read is a failure, not a pass.
#
#   scripts/no-image-push.sh [file...]    default: the repository's files
#
# Run by `make check`.

set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH='' cd -- "$here/.." && pwd)

# a push or a registry login by any of the tools that can make one, the actions and
# options that push as they build, and the workflow permission a push needs
command_pattern='(^|[^[:alnum:]_-])(docker|podman|buildah|nerdctl|wslc(\.exe)?)([[:space:]]+(image|manifest))?[[:space:]]+(push|login)([^[:alnum:]_-]|$)'
command_pattern="$command_pattern"'|buildx[^|;&]*--push|build-push-action|push-to-registry'
command_pattern="$command_pattern"'|(^|[^[:alnum:]_-])skopeo[[:space:]]+(copy|sync|login)|(^|[^[:alnum:]_-])crane[[:space:]]+(push|copy|append|auth)'
command_pattern="$command_pattern"'|(^|[^[:alnum:]_-])oras[[:space:]]+(push|login)|(^|[^[:alnum:]_-])ko[[:space:]]+(publish|build)'
command_pattern="$command_pattern"'|packages:[[:space:]]*write|type=registry|push=true'
rust_pattern='\.args?\([[:space:]]*(\[[^]]*)?"(push|login)"'

failures=0

check() {
    file=$1
    if [ ! -r "$file" ]; then
        echo "no-image-push: cannot read $file" >&2
        failures=$((failures + 1))
        return
    fi
    case $file in
    *.rs) pattern=$rust_pattern ;;
    *) pattern=$command_pattern ;;
    esac
    if hits=$(grep -n -I -E -i -- "$pattern" "$file"); then
        printf '%s\n' "$hits" | while IFS= read -r hit; do
            echo "no-image-push: $file:$hit" >&2
        done
        failures=$((failures + 1))
    fi
}

if [ $# -gt 0 ]; then
    for file in "$@"; do check "$file"; done
else
    cd "$root"
    list=$(git ls-files --cached --others --exclude-standard) || {
        echo "no-image-push: git could not list the files, so nothing was checked" >&2
        exit 1
    }
    [ -n "$list" ] || { echo "no-image-push: git listed no files" >&2; exit 1; }
    # a name with a newline in it would split; git quotes such names, and none exist here
    while IFS= read -r file; do
        case $file in
        *.md | scripts/no-image-push.sh | scripts/no-image-push.test.sh) continue ;;
        .claude/hooks/guard-bash.sh | .claude/hooks/guard-bash.test.sh) continue ;;
        crates/web/static/vendor/*) continue ;;
        esac
        [ -f "$file" ] || continue
        check "$file"
    done <<EOF
$list
EOF
fi

if [ "$failures" -ne 0 ]; then
    echo "no-image-push: $failures file(s) would push an image or log in to a registry; this repository never does (images/harness/README.md)" >&2
    exit 1
fi
