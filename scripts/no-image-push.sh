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
# command (`sudo -u root`, `env -i`, `xargs -n 1`, `timeout 600`, `then`, `sh -c`, a YAML
# `run:`, a make `@`) are passed over with their options, an engine's own options are
# passed over with their values (`docker -H host push`, `compose -f a.yml push`), and an
# engine named by a variable (`"$runtime" push`, `$(ENGINE) push`) is an engine, unless
# the variable names git. It fails on:
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
# refuse them), the vendored scripts under crates/web/static/vendor, and a binary file not
# yet added. A file it cannot read, a name git lists that is not a file, git listing
# nothing, and a failure of git or awk all fail the run.
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
# The words in front of a command, and the options each takes before the command: a word
# here is passed over, and so is the value of an option that takes one. The line is in
# lower case by now, so `-C` and `-c` are one option here.
function wrapper(s) { return s ~ /^(sudo|doas|env|nice|ionice|xargs|timeout|time|nohup|exec|command|eval|then|do|else|if|while|until|!|-|run|run:|sh|bash|dash|zsh|ksh|ash)$/ }
function takes_value(tool, o) {
    if (tool == "sudo") return o ~ /^-[ugchprtd]$/
    if (tool == "doas") return o ~ /^-[uc]$/
    if (tool == "env") return o ~ /^-[ucs]$/
    if (tool == "nice") return o ~ /^-n$/
    if (tool == "ionice") return o ~ /^-[cnp]$/
    if (tool == "xargs") return o ~ /^-[inlpsdea]$/
    if (tool == "timeout") return o ~ /^-[sk]$/
    if (tool ~ /^(sh|bash|dash|zsh|ksh|ash)$/) return o ~ /^-[a-z]*o$/
    return 0
}
# An engine option that takes the next word as its value: `docker -H host push`,
# `docker compose -f a.yml push`, `ctr -n ns images push`.
function engine_value(o) {
    return o ~ /^(-[hcfnlp]|--(host|context|config|log-level|url|connection|identity|root|runroot|storage-driver|namespace|address|kubeconfig|file|project-name|profile|env-file|project-directory|name|network|tag|creds|authfile|platform|env|label|build-arg|retry-times|src|dest|src-creds|dest-creds|format|cache-from))$/ || o == "-e"
}
function verbs(tool) {
    if (tool == "skopeo") return " copy sync login "
    if (tool == "crane" || tool == "gcrane") return " push copy cp append auth "
    if (tool == "oras") return " push cp copy login "
    if (tool == "regctl") return " copy put login "
    if (tool == "ko") return " publish build resolve apply "
    return " push login "
}
function local_target(s) { return s ~ /^(oci|oci-archive|dir|docker-archive|containers-storage|docker-daemon):/ }
# One command, its words in w[t..n] with the tool at w[t]: what in it sends an image out,
# or "" for nothing.
function judge(t, n,   tool, i, m, nw, value, v, last) {
    tool = w[t]
    if (tool ~ /^\$/) tool = "$"; else tool = base(tool)
    m = 0; value = 0
    for (i = t + 1; i <= n; i++) {
        if (value) { value = 0; continue }
        # the program a container runs has options of its own: `podman run img x --push`
        if (m >= 1 && nw[1] ~ /^(run|exec|create|start)$/) break
        if (w[i] ~ /^--cache-from=/) continue
        if (w[i] == "--push" || w[i] ~ /push=true/ || w[i] ~ /type=registry/) return "--push"
        if (w[i] ~ /^-/) { if (w[i] !~ /=/ && engine_value(w[i])) value = 1; continue }
        nw[++m] = w[i]
    }
    if (m == 0) return ""
    v = verbs(tool)
    last = nw[m]
    if (index(v, " " nw[1] " ")) {
        # a copy or sync to a local store pulls; a local ko build, a crane credential
        # read and an oras copy to a layout push nothing
        if (tool == "skopeo" && nw[1] == "copy" && local_target(last)) return ""
        if (tool == "skopeo" && nw[1] == "sync" && joined ~ / --dest[ =](dir|oci)( |$)/) return ""
        if (tool == "ko" && joined ~ / (--local|-l|--push=false)( |$)/) return ""
        if (tool ~ /crane$/ && nw[1] == "auth" && nw[2] != "login") return ""
        if (tool == "oras" && joined ~ / --to-oci-layout/) return ""
        return nw[1]
    }
    if (nw[1] ~ /^(image|images|manifest|artifact|compose|system|registry)$/ && m >= 2 && index(v, " " nw[2] " ")) return nw[2]
    if (nw[1] == "farm" && nw[2] == "build") return "farm build"
    if (nw[1] == "buildx" && nw[2] == "imagetools" && nw[3] == "create") return "imagetools create"
    if (tool == "buildah" && nw[1] == "commit" && index(joined, "docker://")) return "commit docker://"
    return ""
}
function report(line) { printf "%s:%d: %s\n", name, NR, line; found = 1 }
function scan(line,   low, n, i, k, t, c, word, words, opts_of, value) {
    low = tolower(line)
    if (low ~ /build-push-action|push-to-registry|bake-action|copytoregistry|deploy-pages|write-all/ \
        || low ~ /uses:[^#]*(release|login|ghcr|registry|docker-build-push)/ \
        || low ~ /(packages|pages)[^#]*:[^#]*write/ \
        || low ~ /^[[:space:]]*push:[[:space:]]*true|"push"[[:space:]]*:[[:space:]]*true|output[^#]*type=registry/ \
        || low ~ /(^|[^[:alnum:]_-])gh[[:space:]]+release[[:space:]]+(create|upload|edit)/ \
        || low ~ /(^|[^[:alnum:]_-])gh[[:space:]]+api[^#]*\/(releases|packages)[^#]*(-x|--method|-f|--field|--raw-field|--input)/ \
        || low ~ ("[\"\047](" engines ")[\"\047][[:space:]]*,[[:space:]]*[\"\047](push|login)[\"\047]")) {
        report(line)
        return
    }
    while (match(low, /\$\([a-z0-9_]+\)/)) low = substr(low, 1, RSTART - 1) "$" substr(low, RSTART + 2, RLENGTH - 3) substr(low, RSTART + RLENGTH)
    while (match(low, /\$\{[^}]*\}/)) {
        inner = substr(low, RSTART + 2, RLENGTH - 3)
        gsub(/[^a-z0-9_]/, "_", inner)
        low = substr(low, 1, RSTART - 1) "$" inner substr(low, RSTART + RLENGTH)
    }
    gsub(/&&|\|\||[;|&(){}`]/, " ; ", low)
    n = split(low, words, /[[:space:]]+/)
    k = 0; t = 0; opts_of = ""; value = 0; joined = ""
    for (i = 1; i <= n + 1; i++) {
        word = (i <= n) ? unquote(words[i]) : ";"
        if (word == ";") {
            if (t > 0 && judge(t, k) != "") { report(line); return }
            k = 0; t = 0; opts_of = ""; value = 0; joined = ""
            continue
        }
        if (word == "") continue
        k++; w[k] = word; joined = joined " " word
        if (t != 0) continue
        if (value) { value = 0; continue }
        if (opts_of != "" && word ~ /^-/) { value = takes_value(opts_of, word); continue }
        if (opts_of == "timeout" && word ~ /^[0-9]/) continue
        if (wrapper(word)) { opts_of = word; continue }
        if (word ~ /^[a-z_][a-z0-9_]*=/ || word ~ /:$/) continue
        c = word
        sub(/^[@+-]+/, "", c)
        c = base(c)
        # a variable is an engine when its name says so: `$runtime`, `$ENGINE`, `${DOCKER}`
        if (c ~ /^\$/ && c ~ /(docker|podman|engine|runtime|container|ctr|wslc|buildah|nerdctl|crane|skopeo|oras|oci)/) { w[k] = c; t = k; continue }
        if (c != "" && engine(c)) { w[k] = c; t = k; continue }
        t = -1
    }
}
function code(line,   low) {
    if (line ~ /["\047]push["\047]/) { report(line); return }
    if (names_engine && line ~ /["\047]login["\047]/) { report(line); return }
    if (names_engine && line ~ /"[^"]*[[:space:]]push([[:space:]][^"]*)?"/) { report(line); return }
    low = tolower(line)
    if (low ~ ("(" engines ")[[:space:]]+((image|manifest)[[:space:]]+)?(push|login)([^[:alnum:]_-]|$)")) report(line)
}
# a line that ends in an odd number of backslashes goes on to the next
function continued(s) { return match(s, /\\+$/) && RLENGTH % 2 == 1 }
{
    gsub(/\r/, "")
    if (is_code) { code($0); next }
    if (continued($0)) { held = held substr($0, 1, length($0) - 1) " "; next }
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
if [ "${1:-}" = --listed ] || [ "${1:-}" = --listed-untracked ]; then
    # called by the whole-repository run below, with the names git gave. A file not yet
    # added that git would call binary, an image saved as a tarball say, is not read: it
    # reaches the repository only by a commit, and is read from then on.
    untracked=''
    [ "$1" = --listed-untracked ] && untracked=yes
    shift
    for file in "$@"; do
        skipped "$file" && continue
        if [ -n "$untracked" ] && [ -f "$file" ] && [ -s "$file" ] && ! grep -I -q '' "$file"; then
            continue
        fi
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
    git -c core.quotePath=false ls-files -z --cached |
        xargs -0 sh "$here/no-image-push.sh" --listed || status=$?
    git -c core.quotePath=false ls-files -z --others --exclude-standard |
        xargs -0 sh "$here/no-image-push.sh" --listed-untracked || status=$?
    [ "$status" -eq 0 ] || exit 1
    exit 0
fi

if [ "$failures" -ne 0 ]; then
    echo "no-image-push: $failures file(s) would push or release an image, or log in to a registry; this repository never does (images/harness/README.md)" >&2
    exit 1
fi
