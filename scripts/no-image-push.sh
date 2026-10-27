#!/bin/sh
# Nothing in this repository pushes an image to a registry or releases one, and this fails
# when something would. The harness image holds harnesses under their vendors' own
# licences, Claude Code and Copilot CLI among them, which the project may not
# redistribute, and an image built from unreleased source is not something the project may
# publish either (images/harness/README.md says why). Each machine builds its own with
# `make image`.
#
# It reads each file the way a shell would read a command in it: a line continued with a
# backslash is one line, quotes around a word do not count, the words in front of a
# command (`sudo -E`, `env`, `xargs`, `then`, `sh -c`, a YAML `run:`, a make `@`) are passed
# over, and an engine named by a variable (`"$runtime" push`, `$(ENGINE) push`) is an
# engine. It fails on:
#
# - a push or a registry login by an engine or a registry tool, with its subcommand found
#   past the options (`docker -H host push`) and through `image`, `manifest`, `compose`
#   and the like (`docker image push`, `docker compose push`), and the other verbs that
#   send an image (`crane cp`, `skopeo copy`, `ko publish`, `podman farm build`);
# - `--push`, `push=true` or `type=registry` on such a command;
# - the actions that push or log in, `imagetools`, a GitHub release made or uploaded to,
#   and a workflow that grants `packages: write` or `write-all`;
# - in Rust, Python and JavaScript, a string that is exactly "push", or "login" in a file
#   that names an engine, since those are a command's arguments however they are passed.
#
# It is a breaker against an accident, not a boundary: a person who means to push can
# write a push it does not read. It reads every file git knows or would add, except
# Markdown, these two scripts, the guard and its tests (which name the same commands to
# refuse them) and the vendored scripts under crates/web/static/vendor. A file it cannot
# read, a name git lists that is not a file, and a failure of git or awk all fail the run.
#
#   scripts/no-image-push.sh [file...]    default: the repository's files
#
# Run by `make check`.

set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH='' cd -- "$here/.." && pwd)

engines='docker|podman|podman-remote|buildah|nerdctl|wslc|wslc[.]exe|docker-compose|podman-compose|skopeo|crane|gcrane|oras|regctl|ctr|helm|ko'

# shellcheck disable=SC2016  # the program is awk's, and its $ are awk's
program='
function base(s,   n, parts) { n = split(s, parts, "/"); return parts[n] }
function unquote(s) { gsub(/["\047\\]/, "", s); return s }
function engine(s) { return s ~ ("^(" engines ")$") }
function prefix(s) {
    if (s ~ /^(sudo|doas|env|time|nohup|exec|command|nice|ionice|xargs|eval|then|do|else|if|while|until|!|-|run|run:|sh|bash|dash|zsh|ksh|ash)$/) return 1
    if (s ~ /^-/ && seen_prefix) return 1
    if (s ~ /^[a-z_][a-z0-9_]*=/) return 1
    return s ~ /:$/
}
function verbs(tool) {
    if (tool == "skopeo") return " copy sync login "
    if (tool == "crane" || tool == "gcrane") return " push copy cp append auth "
    if (tool == "oras") return " push cp copy login "
    if (tool == "regctl") return " copy put login "
    if (tool == "ko") return " publish build resolve apply "
    return " push login "
}
# One command, its words in w[t..n] with the tool at w[t]: what in it sends an image out,
# or "" for nothing.
function judge(t, n,   tool, i, first, second, before, v) {
    tool = w[t]
    if (tool ~ /^\$/) tool = "$"; else tool = base(tool)
    first = ""; second = ""; before = ""
    for (i = t + 1; i <= n; i++) {
        if (w[i] == "--push" || w[i] ~ /push=true/ || w[i] ~ /type=registry/) return "--push"
        if (w[i] ~ /^-/) { if (first == "") before = w[i]; continue }
        if (first == "") { first = w[i]; continue }
        if (second == "") second = w[i]
    }
    v = verbs(tool)
    if (first != "" && index(v, " " first " ")) return first
    # the verb after a group (`image push`, `compose push`), or after an option that took
    # the first word as its value (`-H host push`)
    if (first ~ /^(image|images|manifest|artifact|compose|system|registry|farm|buildx)$/ || (before != "" && before !~ /=/)) {
        if (second != "" && index(v, " " second " ")) return second
        if (first == "farm" && second == "build") return "farm build"
    }
    return ""
}
function report(line) { printf "%s:%d: %s\n", name, NR, line; found = 1 }
function scan(line,   low, n, i, k, t, c, word, words) {
    low = tolower(line)
    if (low ~ /build-push-action|push-to-registry|login-action|podman-login|bake-action|action-gh-release|imagetools|write-all/ \
        || low ~ /packages[^#]*:[^#]*write/ \
        || low ~ /(^|[^[:alnum:]_-])gh[[:space:]]+release[[:space:]]+(create|upload)/) {
        report(line)
        return
    }
    gsub(/\$\([A-Za-z0-9_]+\)|\$\{[^}]*\}/, "$V", low)
    gsub(/&&|\|\||[;|&(){}`]/, " ; ", low)
    n = split(low, words, /[[:space:]]+/)
    k = 0; t = 0; seen_prefix = 0
    for (i = 1; i <= n + 1; i++) {
        word = (i <= n) ? unquote(words[i]) : ";"
        if (word == ";") {
            if (t > 0 && judge(t, k) != "") { report(line); return }
            k = 0; t = 0; seen_prefix = 0
            continue
        }
        if (word == "") continue
        k++; w[k] = word
        if (t != 0) continue
        if (prefix(word)) { seen_prefix = 1; continue }
        c = word
        sub(/^[@+-]+/, "", c)
        c = base(c)
        if (c != "" && (engine(c) || c ~ /^\$/)) { w[k] = c; t = k; continue }
        t = -1
    }
}
function code(line,   low) {
    if (line ~ /["\047]push["\047]/) { report(line); return }
    if (names_engine && line ~ /["\047]login["\047]/) { report(line); return }
    low = tolower(line)
    if (low ~ ("(" engines ")[[:space:]]+((image|manifest)[[:space:]]+)?(push|login)([^[:alnum:]_-]|$)")) report(line)
}
{
    gsub(/\r/, "")
    if (is_code) { code($0); next }
    if ($0 ~ /\\$/) { held = held substr($0, 1, length($0) - 1) " "; next }
    scan(held $0)
    held = ""
}
END {
    if (held != "") scan(held)
    exit found ? 1 : 0
}
'

check() {
    file=$1
    if [ ! -f "$file" ] || [ ! -r "$file" ]; then
        echo "no-image-push: cannot read $file" >&2
        return 2
    fi
    is_code=0
    names_engine=0
    case $file in
    *.rs | *.py | *.js | *.mjs | *.cjs | *.ts)
        is_code=1
        if tr -d '\000' < "$file" | grep -q -E "[\"']($engines)[\"']"; then names_engine=1; fi
        ;;
    esac
    # NUL bytes out first: a binary file is read as text, and UTF-16 text becomes ASCII
    status=0
    out=$(tr -d '\000' < "$file" |
        awk -v name="$file" -v engines="$engines" -v is_code="$is_code" \
            -v names_engine="$names_engine" "$program" 2>&1) || status=$?
    case $status in
    0) return 0 ;;
    1)
        printf '%s\n' "$out" | sed 's/^/no-image-push: /' >&2
        return 1
        ;;
    *)
        echo "no-image-push: awk failed on $file: $out" >&2
        return 2
        ;;
    esac
}

skipped() {
    case $1 in
    *.md | scripts/no-image-push.sh | scripts/no-image-push.test.sh) return 0 ;;
    .claude/hooks/guard-bash.sh | .claude/hooks/guard-bash.test.sh) return 0 ;;
    crates/web/static/vendor/*) return 0 ;;
    *) return 1 ;;
    esac
}

failures=0
if [ "${1:-}" = --listed ]; then
    # called by the whole-repository run below, with the names git gave
    shift
    for file in "$@"; do
        skipped "$file" && continue
        if [ ! -e "$file" ] && [ ! -L "$file" ] && [ -n "$(git ls-files --deleted -- "$file")" ]; then
            continue
        fi
        check "$file" || failures=$((failures + 1))
    done
elif [ $# -gt 0 ]; then
    for file in "$@"; do check "$file" || failures=$((failures + 1)); done
else
    cd "$root"
    if ! git -c core.quotePath=false ls-files -z --cached --others --exclude-standard > /dev/null; then
        echo "no-image-push: git could not list the files, so nothing was checked" >&2
        exit 1
    fi
    count=$(git -c core.quotePath=false ls-files -z --cached --others --exclude-standard | tr -cd '\000' | wc -c)
    if [ "$count" -eq 0 ]; then
        echo "no-image-push: git listed no files, so nothing was checked" >&2
        exit 1
    fi
    status=0
    git -c core.quotePath=false ls-files -z --cached --others --exclude-standard |
        xargs -0 sh "$here/no-image-push.sh" --listed || status=$?
    [ "$status" -eq 0 ] || exit 1
    exit 0
fi

if [ "$failures" -ne 0 ]; then
    echo "no-image-push: $failures file(s) would push or release an image, or log in to a registry; this repository never does (images/harness/README.md)" >&2
    exit 1
fi
