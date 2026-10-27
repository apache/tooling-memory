#!/bin/sh
# Hold the rules AGENTS.md states as absolutes, at the moment one is about to be broken.
#
# Claude Code runs this before every Bash tool call, handing it the call as JSON on
# stdin. Exiting 2 refuses the call and shows this script's stderr to the model, which
# then knows what to run instead. The rules that earn the machinery:
#
#   - stage the files you changed; never `git add -A`, nor the `git commit -a` that
#     spells the same thing on the commit
#   - the stash stack is shared with every other worktree, so never bare `git stash`
#   - `make check` runs before a commit, so never `--no-verify`
#   - anything posted to an issue or a pull request says who wrote it
#   - `cargo` or `make` piped into head or tail reports the pipe's exit status, so a
#     run that failed reads as one that passed
#
# A rule the harness holds does not have to be remembered. That is the point of putting
# these here rather than in another paragraph of AGENTS.md: the paragraph is read once
# per session and skipped when the context is long, and this is not read at all.
#
# Adapted from apache/tooling-agenda, whose guard also keeps a toolchain inside a dev
# container. This repository runs cargo on the host, so that rule and its make-target
# companion are gone; everything about git and gh is kept as it was.

set -eu

payload=$(cat)

# The body of a heredoc is data. The shell will not execute it - a quoted `<<'MD'` is not
# even expanded - so nothing in there is a command, and reading it as one refused an issue
# body that quoted `uv run ...` in a fenced example. The fix each time was to write the
# file with a different tool, which is the one habit a guard cannot afford to teach (#701).
#
# The line that opens the heredoc is kept, because the command on it is real, and so is
# everything after the terminator: `uv run ...` before or after a heredoc is still uv on
# the host, and that is the half of this worth testing.
#
# A `<<` the shell would not read as an operator opens nothing, and two places put one on a
# line that is not one: inside quotes, where no redirection is parsed, and after a `#`,
# where the rest of the line is a comment. `git log --format="%h<<%s"` is the innocent
# shape; the harmful one is a `<<` in either position whose invented delimiter some later
# line then matches, dropping every command in between. The scan therefore walks each line
# tracking quote state, the way split_on_separators does below, stops at a comment, and
# opens a body only on a `<<` that survives both.
#
# A body whose terminator never arrives is kept and read. The shell would swallow the rest
# of the line the same way, but agreeing with it there costs more than it is worth: a `<<`
# this misreads would take every command after it out of view, and reading a few lines the
# shell would not is the safe direction for a guard to be wrong in.
drop_heredoc_bodies() {
    awk '
        # The delimiter an operator opens with, and how much of the line it took. Returns the
        # count of characters consumed after the `<<`, and reports the delimiter in DELIM -
        # empty when the operator opens no body at all.
        # \047 and \042 are the quotes, spelled so this program can be held in a shell string
        # without escaping them.
        function read_delimiter(rest,   quote, closing, taken) {
            DELIM = ""
            STRIP = 0
            taken = 0
            # `<<<` is a here-string: it feeds one word, and opens no body
            if (substr(rest, 1, 1) == "<") return 1
            # `<<-WORD` lets the body and its terminator be indented with tabs
            if (substr(rest, 1, 1) == "-") { STRIP = 1; rest = substr(rest, 2); taken = 1 }
            if (match(rest, /^[ \t]+/) > 0) { taken += RLENGTH; rest = substr(rest, RLENGTH + 1) }
            quote = substr(rest, 1, 1)
            if (quote == "\047" || quote == "\042") {
                closing = index(substr(rest, 2), quote)
                if (closing == 0) return taken + 1
                DELIM = substr(rest, 2, closing - 1)
                return taken + closing + 1
            }
            if (quote == "\\") { rest = substr(rest, 2); taken++ }
            if (match(rest, /^[A-Za-z0-9_.-]+/) == 0) return taken
            DELIM = substr(rest, 1, RLENGTH)
            return taken + RLENGTH
        }
        BEGIN { head = 1; tail = 0; held = 0; quote = "" }
        head <= tail {
            line = $0
            if (dash[head]) sub(/^\t+/, "", line)
            if (line == word[head]) { head++; held = 0 } else { hold[++held] = $0 }
            next
        }
        {
            print
            line = $0
            at = 1
            while (at <= length(line)) {
                char = substr(line, at, 1)
                if (quote != "") {
                    if (char == quote) quote = ""
                    at++
                } else if (char == "\\") {
                    at += 2
                } else if (char == "\047" || char == "\042") {
                    quote = char
                    at++
                } else if (char == "#" && (at == 1 || substr(line, at - 1, 1) ~ /[ \t]/)) {
                    # the rest of the line is a comment, and a comment opens no heredoc
                    break
                } else if (char == "<" && substr(line, at + 1, 1) == "<") {
                    at += 2 + read_delimiter(substr(line, at + 2))
                    # a line may open more than one, and they close in the order they opened
                    if (DELIM != "") { tail++; word[tail] = DELIM; dash[tail] = STRIP }
                } else {
                    at++
                }
            }
        }
        END { for (i = 1; i <= held; i++) print hold[i] }
    '
}

# The command, read out of the payload without a JSON parser. jq is not reliably on a
# host, and a guard that fails to start is a guard that silently stops guarding. The pattern takes a JSON string body - any escape, or any character
# that is neither quote nor backslash - so a quote inside the command does not end it
# early. An escaped newline becomes a real one, so that the heredocs can be found and
# their bodies dropped, and what is left becomes a `;` - which is what a newline means to
# a shell, and keeps a command written across several lines split into its parts.
command=$(
    printf '%s' "$payload" |
        sed -nE 's/.*"command"[[:space:]]*:[[:space:]]*"((\\.|[^"\\])*)".*/\1/p' |
        sed -e 's/\\n/\
/g' -e 's/\\[rt]/ /g' -e 's/\\"/"/g' -e 's/\\\\/\\/g' |
        drop_heredoc_bodies |
        tr '\n' ';'
)

# Nothing recognisable: allow it. Refusing a payload this cannot read would cost every
# call, to catch the ones it cannot see anyway.
if [ -z "$command" ]; then
    exit 0
fi

# The same call with the heredoc bodies kept, and its newlines with them. Every rule above
# reads a command, and so must not see a body; the comment rule reads the text about to be
# posted, and a body is where that text usually is.
posted_text=$(
    printf '%s' "$payload" |
        sed -nE 's/.*"command"[[:space:]]*:[[:space:]]*"((\\.|[^"\\])*)".*/\1/p' |
        sed -e 's/\\n/\
/g' -e 's/\\[rt]/ /g' -e 's/\\"/"/g' -e 's/\\\\/\\/g'
)

refuse() {
    printf '%s\n\n%s\n\nRefused by .claude/hooks/guard-bash.sh, which holds the rules in AGENTS.md.\n' \
        "$1" "$2" >&2
    exit 2
}

# What a part of the line actually runs, past whatever stands in front of it: leading
# whitespace, an opening subshell or quote, `VAR=value` prefixes, and the wrappers that
# take a command as their argument.
strip_prefixes() {
    _rest=$1
    while [ -n "$_rest" ]; do
        case $_rest in
            [[:space:]]* | '('* | '{'* | "'"* | '"'*)
                _rest=${_rest#?}
                continue
                ;;
            *) : ;;
        esac
        case ${_rest%%[[:space:]]*} in
            sudo | env | time | nohup | exec | command | *=*) : ;;
            # `timeout 600 make test` runs make, and so does `timeout --signal=KILL 600`.
            timeout)
                _rest=${_rest#*[[:space:]]}
                while :; do
                    case ${_rest%%[[:space:]]*} in
                        -* | [0-9]*) : ;;
                        *) break ;;
                    esac
                    case $_rest in
                        *[[:space:]]*) _rest=${_rest#*[[:space:]]} ;;
                        *) break ;;
                    esac
                done
                continue
                ;;
            *) break ;;
        esac
        case $_rest in
            *[[:space:]]*) _rest=${_rest#*[[:space:]]} ;;
            *) break ;;
        esac
    done
    printf '%s' "$_rest"
}

# Whether a bundle of short options carries one of the letters named. git lets them be
# written together, so the letter that matters is not always the first: `git add -vA` and
# `git commit -qva` are the whole-tree spellings of `-A` and `-a`.
#
# Read left to right, stopping at the first option that takes a value, because what follows
# that one is the value and not more options: `git commit -qma` is `-q -m a`, and the `a` is
# the message. $1 is the word, $2 the letters that are the finding, $3 the letters that take
# a value. $2 must not be empty - an empty pattern matches every letter.
cluster_carries() {
    _cluster=${1#-}
    _wanted=$2
    _valued=$3
    while [ -n "$_cluster" ]; do
        _letter=${_cluster%"${_cluster#?}"}
        case $_wanted in
            *"$_letter"*) return 0 ;;
            *) : ;;
        esac
        case $_valued in
            *"$_letter"*) return 1 ;;
            *) : ;;
        esac
        _cluster=${_cluster#?}
    done
    return 1
}

# A wildcard stages what this change never touched, which is how a credential, a scratch
# file or a generated tree reaches a commit that was meant to hold three files.
check_git_add() {
    for _word in "$@"; do
        _staged_wide=no
        case $_word in
            --all | --update | . | ./ | :/ | '*') _staged_wide=yes ;;
            --*) : ;;
            # `git add` has no short option that takes a value, so nothing stops the read.
            -*)
                if cluster_carries "$_word" 'Au' ''; then
                    _staged_wide=yes
                fi
                ;;
            *) : ;;
        esac
        if [ "$_staged_wide" = yes ]; then
            refuse "\`git add $_word\` stages files this change never touched." \
                "Name the files instead: git add crates/core/src/lib.rs PLAN.md"
        fi
    done
}

# The stash stack is one stack for the whole repository, shared by every linked worktree
# and so by every session running in one. A pop here can take the work of a session next
# door, and it cannot be given back.
check_git_stash() {
    case ${1:-} in
        '' | save | pop | clear | drop)
            refuse "\`git stash ${1:-}\` reaches into a stack shared with every other worktree in this repository." \
                "Set work aside with a temporary commit instead. If it really must be the stash: git stash push -u -m <unique-tag>, then apply it by sha."
            ;;
        push)
            # Both spellings of each: `-m tag` and `-m"tag"`, which unquoting leaves as the
            # single word `-m_`. Only the spaced forms were looked for, so the attached ones
            # were refused for having no message when they carry one.
            case " $* " in
                *' -m '* | *' -m'* | *' --message '* | *' --message='*) : ;;
                *)
                    refuse "\`git stash push\` without a message leaves an entry no session can tell apart from anybody else's." \
                        "Tag it: git stash push -u -m <unique-tag>"
                    ;;
            esac
            ;;
        *) : ;;
    esac
}

# Nothing in this repository pushes an image, or logs in to a registry to push one
# (images/harness/README.md, "Never published"). The harness image carries harnesses
# under their vendors' own licences, which the project may not redistribute, so a push
# from here is never right, whatever the tag. scripts/no-image-push.sh holds the files to
# the same rule; this holds what a session types.
#
# The subcommand is the first word that is not an option, or the second where an option
# takes a value (`docker -H host push`); `image push` and `manifest push` are pushes too.
check_image_push() {
    _tool=${1##*/}
    shift
    _first='' _second=''
    for _w in "$@"; do
        case $_w in
            -*) continue ;;
            *) : ;;
        esac
        if [ -z "$_first" ]; then
            _first=$_w
        else
            _second=$_w
            break
        fi
    done
    _verbs='push login'
    case $_tool in
        skopeo) _verbs='copy sync login' ;;
        crane) _verbs='push copy append auth' ;;
        oras) _verbs='push login' ;;
        *) : ;;
    esac
    _hit=''
    for _v in $_verbs; do
        if [ "$_first" = "$_v" ] || [ "$_second" = "$_v" ]; then
            _hit=$_v
        fi
    done
    case $_first in
        buildx)
            case " $* " in
                *' --push '* | *push=true* | *type=registry*) _hit=push ;;
                *) : ;;
            esac
            ;;
        *) : ;;
    esac
    if [ -n "$_hit" ]; then
        refuse "\`$_tool ... $_hit\` would send an image to a registry, or log in to send one. This repository never publishes an image: the harness image holds harnesses the project may not redistribute." \
            "Build and keep it local: make image. images/harness/README.md says why."
    fi
}

# A quoted run of several words is a value, not options.
#
# The words this guard walks are the call split on whitespace with the quotes still in
# them, so a commit message is several words and each is read as a flag: `git commit -m
# "guard: refuse git commit -a"` was refused for an `-a` that was part of a sentence. The
# rule that sorts it out is about the quotes rather than about which option preceded them,
# because there is no reliable way to know: `-m`, `-m"..."`, `--author="..."` and `-m $'...'`
# all carry one, and a path like `-C "../my worktree"` carries one too.
#
# So: a quoted run holding whitespace becomes a single `_`, which is a word and not a flag,
# and a quoted run holding one word becomes that word. The second half matters - `git add
# '*'` is quoted precisely so that git sees the glob rather than the shell, and replacing
# it with `_` would narrow the staging rule to the unquoted spelling.
#
# `_` rather than nothing so that a value goes on being a value: `git -C "../my worktree"
# add -A` has to leave `add` as the subcommand, and a run that vanished would make it `-A`.
# An empty run is the exception, and a shell treats it two ways depending on where it is.
# Inside a word it goes: `git add ''-A` is `git add -A`, and standing in for it with a
# placeholder made that word `_-A`, which is a way past the staging rule. As a whole word
# it stays, as an empty argument - `git --shallow-file '' add -A` passes git an empty path
# and stages everything, so that one has to keep its place or `add` becomes the value.
#
# A backslash spells both halves of that too. Outside quotes it escapes the space rather
# than the word, and `-C ../my\ worktree` is the same one value as `-C "../my worktree"` -
# so the escaped whitespace becomes the same `_`, or the value comes apart into words
# again. Inside `$'...'` it escapes the quote, which a plain single-quoted run never does:
# `$'don\'t refuse -a'` is one word in a shell, and reading that `\'` as the end of the run
# left the rest of the sentence as bare flags.
unquote_values() {
    _rest=$1
    _out=''
    _quote=''
    _escapes=no
    _run=''
    _lone=no
    while [ -n "$_rest" ]; do
        _char=${_rest%"${_rest#?}"}
        _rest=${_rest#?}
        if [ -n "$_quote" ]; then
            # Where a backslash escapes what follows - inside double quotes, and inside
            # `$'...'` - it does not close the run: `-m "the \"guard\" refuses -a"` is one
            # message, not three words, and so is `-m $'the guard\'s -a'`.
            if [ "$_escapes" = yes ] && [ "$_char" = "\\" ] && [ -n "$_rest" ]; then
                _run=$_run${_rest%"${_rest#?}"}
                _rest=${_rest#?}
                continue
            fi
            if [ "$_char" = "$_quote" ]; then
                _quote=''
                case $_run in
                    '')
                        # Kept only where the empty quotes are the whole word: nothing
                        # before them, and nothing after them either.
                        if [ "$_lone" = yes ]; then
                            case ${_rest%"${_rest#?}"} in
                                '' | [[:space:]]) _out=${_out}_ ;;
                                *) : ;;
                            esac
                        fi
                        ;;
                    *[[:space:]]*) _out=${_out}_ ;;
                    *) _out=$_out$_run ;;
                esac
                _run=''
            else
                _run=$_run$_char
            fi
            continue
        fi
        case $_char in
            # Outside quotes a backslash makes the next character ordinary, quotes
            # included. `'don'\''t refuse this -a'` is one word an apostrophe was written
            # into, and reading that `\'` as an opening quote left the rest of the sentence
            # unquoted - so the guard refused a commit message for the `-a` in it.
            "\\")
                if [ -n "$_rest" ]; then
                    _next=${_rest%"${_rest#?}"}
                    _rest=${_rest#?}
                    case $_next in
                        # The escaped character joins the word rather than ending it, so
                        # whitespace takes the same placeholder a quoted run of several
                        # words takes: `git -C ../my\ worktree add -A` has to leave `add`
                        # as the subcommand, and a value split back into two words made it
                        # `worktree`, which matched no rule here.
                        [[:space:]]) _out=${_out}_ ;;
                        *) _out=$_out$_next ;;
                    esac
                else
                    _out=$_out$_char
                fi
                ;;
            # `$'...'` is one word in which a backslash escapes what follows. The `$` is
            # part of the quoting rather than of the value, so it goes with the quotes:
            # left in front, `$'-a'` is the word `$-a`, which is the flag under a spelling
            # no rule here matches.
            '$')
                case $_rest in
                    "'"*)
                        _rest=${_rest#?}
                        _quote="'"
                        _escapes=yes
                        case $_out in
                            '' | *[[:space:]]) _lone=yes ;;
                            *) _lone=no ;;
                        esac
                        ;;
                    *) _out=$_out$_char ;;
                esac
                ;;
            "'" | '"')
                _quote=$_char
                # A backslash is an ordinary character inside single quotes and an escape
                # inside double ones: `'don\t'` is `don\t` and `"don\"t"` is `don"t`. So
                # a `\'` in a single-quoted run closes it, and the words after it are bare.
                case $_char in
                    '"') _escapes=yes ;;
                    *) _escapes=no ;;
                esac
                case $_out in
                    '' | *[[:space:]]) _lone=yes ;;
                    *) _lone=no ;;
                esac
                ;;
            *) _out=$_out$_char ;;
        esac
    done
    # An unterminated quote is the shell's problem, not this one's. Keep what was read so
    # that the words before it are still judged.
    if [ -n "$_quote" ]; then
        case $_run in
            '') : ;;
            *[[:space:]]*) _out=${_out}_ ;;
            *) _out=$_out$_run ;;
        esac
    fi
    printf '%s' "$_out"
}

# `git commit -a` stages every tracked file it finds modified. That is `git add -u` under
# another name, and `check_git_add` refuses that one for putting into a commit what the
# change never touched - so the shorter spelling was the way past a rule this file already
# holds.
check_git_commit_staging() {
    for _word in "$@"; do
        _staged_wide=no
        case $_word in
            --all) _staged_wide=yes ;;
            --*) : ;;
            # -m -F -c -C -t -S take a value, and -u takes one too (`--untracked-files=`).
            -*)
                if cluster_carries "$_word" 'a' 'mFcCtuS'; then
                    _staged_wide=yes
                fi
                ;;
            *) : ;;
        esac
        if [ "$_staged_wide" = yes ]; then
            refuse "\`git commit $_word\` stages every modified file, including ones this change never touched." \
                "Name what you changed - git add crates/core/src/lib.rs - and commit without the flag."
        fi
    done
}

# `make check` is the gate, and skipping it moves the failure to CI at best and to main
# at worst. -n means --no-verify to commit and --dry-run to push, so only commit's is a
# refusal.
check_git_verification() {
    _subcommand=$1
    for _word in "$@"; do
        case $_word in
            --no-verify)
                refuse "\`git $_subcommand --no-verify\` skips the pre-commit hooks that AGENTS.md requires to pass." \
                    "Run make check, fix what it reports, and commit without the flag."
                ;;
            -n)
                if [ "$_subcommand" = commit ]; then
                    refuse "\`git commit -n\` skips the pre-commit hooks that AGENTS.md requires to pass." \
                        "Run make check, fix what it reports, and commit without the flag."
                fi
                ;;
            *) : ;;
        esac
    done
}

check_git() {
    _git=$(unquote_values "$*")
    set -f
    # shellcheck disable=SC2086  # splitting the call into its words is the point
    set -- $_git
    set +f
    shift

    # git's own options come before the subcommand, and reading the subcommand as whatever
    # word follows `git` misses every one of them: `git -C ../other add -A` had `-C` for its
    # subcommand and matched nothing here, so every rule below was one `-C` away from not
    # applying. `-C <dir>` is the tidier way to work in a linked worktree than `cd &&`, which
    # made the better-written form the unchecked one.
    #
    # The subcommand is the first word that is not an option, so anything else beginning
    # with `-` is skipped rather than stopping the walk. Stopping made the fix one flag
    # wide instead of none: `git --no-optional-locks add -A` broke off at a word this list
    # had not heard of, read `--no-optional-locks` as the subcommand, matched nothing, and
    # ran. The named ones are those whose value is the next word rather than attached to
    # it - `git --attr-source HEAD add -A` has to skip the `HEAD` too, or that becomes the
    # subcommand. The list is `handle_options()` in git.c, which is where it had to be read
    # from: the manual page is not a list of which options take a separated value, and
    # taking it for one left out `--shallow-file`, whose omission put `git --shallow-file
    # '' add -A` - which does stage everything - straight past every rule here. It also
    # names `--exec-path` as taking a path, which it does not take separated: `git
    # --exec-path add -A` prints the exec path and stages nothing.
    #
    # An option git adds later that takes a separated value is the one shape still
    # unaccounted for. Nothing here can notice that on its own.
    while [ "$#" -gt 0 ]; do
        case $1 in
            -C | -c | --git-dir | --work-tree | --namespace | --attr-source | --config-env | --super-prefix | --shallow-file)
                shift
                [ "$#" -gt 0 ] && shift
                ;;
            -?*) shift ;;
            *) break ;;
        esac
    done

    case ${1:-} in
        add)
            shift
            check_git_add "$@"
            ;;
        stash)
            shift
            check_git_stash "$@"
            ;;
        commit)
            check_git_verification "$@"
            shift
            check_git_commit_staging "$@"
            ;;
        push) check_git_verification "$@" ;;
        *) : ;;
    esac
}

# The line AGENTS.md requires at the end of anything a session posts, naming the agent and
# the account it went up under.
#
# Anchored where the line starts and nowhere else. What is read here is shell source rather than
# the comment that comes out of it, so the line the signature is on ends in whatever the command
# needed next - the closing quote, then `--label enhancement`, a redirection, a `&&`. Requiring
# the signature to end the line refused every one of those, and `gh issue create --body "..."
# --label x` is an ordinary thing to write. Starting the line is what "a line of its own" means;
# what the shell puts after it is the shell's business.
ATTRIBUTION="^Posted by .+ on behalf of @[A-Za-z0-9][A-Za-z0-9-]*"

# How many comments this call has been found to post. More than one is refused rather than read,
# because a payload-wide search cannot tell which body a signature belongs to: two comments and
# one signature would pass, and signing one of two is the likely lapse rather than an invented
# one. Posting them separately makes each answerable on its own.
#
# Counted as the parts are read rather than by looking for the command in the text. Text is not
# a command: a comment explaining how to reply, quoting `gh issue comment` on a line of its own,
# was counted as a second post and the whole - correctly signed - call refused. It is the same
# mistake as reading a heredoc body as commands (#701), and `check_line` walks the parts in this
# shell, so counting where they are dispatched costs nothing and cannot see prose.
_posts_seen=0

# Whether the text about to be posted carries that line.
#
# The line is looked for in the whole call, and that is deliberate: the usual shape here writes
# the body with a heredoc and posts it in the same call, so the file `--body-file` names does not
# exist yet when this runs. Scoping the search to the argument would refuse the one shape a long
# comment is actually written in. Where the paths come from is a different question, and
# `body_files` answers it more carefully.
#
# Two things it therefore does not catch, both written down rather than left to be found: an
# unsigned comment in a call that says the line somewhere else - `... --body "no sig"; echo
# "Posted by ..."` - and one whose own body text names a path that happens to hold a signed file.
# Each is a sentence somebody had to write on purpose. This guard is here to stop a lapse rather
# than a decision, and every attempt to close the second cost more than it saved: scoping the
# search refused the heredoc shape, and skipping files once `--body` appeared refused an issue
# whose *title* mentioned `--body`. The count above closes the case that is a lapse.
#
# A path it cannot resolve is not a refusal. `--body-file "$BODY"` names a file whose name this
# does not know, so there is nothing to read and nothing to say about it; refusing there would
# block a shape that is doing everything right.
signed_off() {
    if printf '%s\n' "$posted_text" | grep -Eq "$ATTRIBUTION"; then
        return 0
    fi
    # `--body "$(cat notes.md)"` is refused, and that is the lesser of two wrongs. Treating a
    # body the shell has still to build as unreadable meant asking whether the value held a `$`,
    # and prose holds them all the time - "costs $5 a month", "echo exit=$?" - so the rule
    # switched itself off for half the comments this repository writes. A path is different:
    # `$` in one is a variable, not a sentence, which is why the check below keeps it.
    # Read a line at a time: a path may have a space in it, and splitting on whitespace tested
    # half of one and reported the file missing. The subshell says how it went by exiting, and
    # says so explicitly - an `if` whose condition is false returns 0, which would have made an
    # exhausted loop read as a signature found.
    body_files | {
        while IFS= read -r _file; do
            case $_file in
                *'$'*) exit 0 ;;
                *) : ;;
            esac
            if [ -f "$_file" ] && grep -Eq "$ATTRIBUTION" "$_file"; then
                exit 0
            fi
        done
        exit 1
    }
}

# Every file this call would take a body from, as a path that can be tested.
#
# Read from the command with the heredoc bodies dropped, not from the text: a path written inside
# a comment is prose, and reading it as an argument let an unsigned comment be signed by whatever
# file its own words happened to name.
#
# What a path arrives wrapped in is not part of it: `--body-file "/tmp/c.md"` keeps the shell's
# quotes, and was tested with them still attached - so a file that was there and signed read as
# a file that was not there at all. Every occurrence rather than the last one on a line, which is
# what the sed this replaced quietly did.
#
# Matched rather than split into fields, because a quoted path may have a space in it and fields
# end at one: `--body-file "/tmp/some dir/body.md"` was read as `"/tmp/some` and reported missing.
body_files() {
    printf '%s\n' "$command" | awk '
        {
            line = $0
            while (match(line, /(--body-file|-F)[=[:space:]]+("[^"]*"|\047[^\047]*\047|[^[:space:];|&]+)/)) {
                value = substr(line, RSTART, RLENGTH)
                line = substr(line, RSTART + RLENGTH)
                sub(/^(--body-file|-F)[=[:space:]]+/, "", value)
                # The separator first: a newline became a `;` when this was read, so the token
                # ends `"...";` and a quote stripped at end-of-string would find nothing there.
                sub(/[;|&].*$/, "", value)
                gsub(/^["\047]|["\047]$/, "", value)
                if (value != "") {
                    print value
                }
            }
        }
    '
}

# A comment goes up under somebody's account, and the account is the only signature on it
# unless the text says otherwise.
#
# Only where the session is supplying the text. `gh pr create` with no body flag opens an
# editor for a person to type in, and there is nothing here to check - refusing that would
# be the guard stopping a human from writing their own words.
check_gh() {
    shift
    # `gh --repo o/r issue comment ...` puts a flag where the noun was expected, and gh takes it.
    # Reading only the first two words after `gh` missed every one of those.
    while [ "$#" -gt 0 ]; do
        case ${1:-} in
            *=*) shift ;;
            # the flag and the word it takes: `--repo o/r issue comment` is three words before
            # the noun, and stopping at the first that is not a flag stopped on the repository
            -?*)
                shift
                [ "$#" -gt 0 ] && shift
                ;;
            *) break ;;
        esac
    done
    case "${1:-} ${2:-}" in
        # `gh api` reaches the same comments, and is the only way one is edited. It is not here:
        # writing that branch cost three of the five defects this rule was reviewed for, in a
        # guard whose whole difficulty is telling a command from text that looks like one. The
        # gap is real and is filed rather than left implied - see #809.
        'issue comment' | 'issue create' | 'issue edit' | 'pr comment' | 'pr create' | 'pr edit' | 'pr review') : ;;
        *) return 0 ;;
    esac
    # `-b` and `-F` take their value attached as well as apart - `-bFixed.`, `-b=Fixed.`,
    # `-F=body.md` are all read by gh, and matching only the spaced form let each of them past
    # without the rule ever running.
    case " $* " in
        *' --body '* | *' --body='* | *' --body-file '* | *' --body-file='* | *' -b'* | *' -F'*) : ;;
        *) return 0 ;;
    esac
    _posts_seen=$((_posts_seen + 1))
    if [ "$_posts_seen" -gt 1 ]; then
        refuse "This call posts more than one comment, and one signature cannot answer for two." \
            "Post them one at a time, each ending with its own:

  Posted by Claude on behalf of @<username>"
    fi
    if signed_off; then
        return 0
    fi
    refuse "\`gh ${1:-} ${2:-}\` is posting text with no line saying who wrote it." \
        "End the body with, on a line of its own:

  Posted by Claude on behalf of @<username>

AGENTS.md, \"Writing on an issue or a pull request\"."
}

check_part() {
    _stripped=$(strip_prefixes "$1")

    # A shell handed a command string is running that command: `sh -c 'git add -A'` is
    # `git add -A`, and reading only the first word would let every rule here be stepped
    # around by one wrapper. What follows -c goes back through the same reading, which
    # settles the nested case with it.
    _head=${_stripped%%[[:space:]]*}
    case ${_head##*/} in
        sh | bash | dash | ash | zsh | ksh)
            case $_stripped in
                *[[:space:]]-c[[:space:]]*)
                    _inner=${_stripped#*[[:space:]]-c[[:space:]]}
                    # The command string is quoted as a whole, and the quotes around it
                    # are the shell's rather than part of what runs.
                    case $_inner in
                        "'"*"'" | '"'*'"')
                            _inner=${_inner#?}
                            _inner=${_inner%?}
                            ;;
                        *) : ;;
                    esac
                    check_line "$_inner"
                    return 0
                    ;;
                *) : ;;
            esac
            ;;
        *) : ;;
    esac

    set -f
    # shellcheck disable=SC2086  # splitting the part into its words is the point
    set -- $_stripped
    set +f
    if [ "$#" -eq 0 ]; then
        return 0
    fi
    case ${1##*/} in
        git) check_git "$@" ;;
        gh) check_gh "$@" ;;
        docker | podman | buildah | nerdctl | wslc | wslc.exe | skopeo | crane | oras)
            check_image_push "$@"
            ;;
        *) : ;;
    esac
}

# Each command on the line is judged on its own: a `&&` chain is as much a way to run
# `git add -A` as running it alone. Splitting on the separators and reading the first
# word of each part gets that right.
#
# A separator inside quotes is not one: the string handed to a shell is a single argument,
# and `git commit -m "a && b"` has to survive as one part. Splitting through the quotes
# would cut it in half and read the message as a command.
#
# check_part calls back into this for the string a shell is handed, so the two recurse.
# It terminates because each round consumes at least the shell and its -c.
# The command split on `;`, `|` and `&`, one part per line, leaving a separator inside
# single or double quotes where it is. Walks the string a character at a time, which is
# slow for its length and irrelevant at the length of a command line.
split_on_separators() {
    _rest=$1
    _out=''
    _quote=''
    _newline='
'
    while [ -n "$_rest" ]; do
        _char=${_rest%"${_rest#?}"}
        _rest=${_rest#?}
        if [ -n "$_quote" ]; then
            if [ "$_char" = "$_quote" ]; then
                _quote=''
            fi
        else
            case $_char in
                "'" | '"') _quote=$_char ;;
                # `(` and `)` group commands, so they end one as surely as `;` does. Left in
                # place they welded the bracket onto the word beside it: `(git add -A)` was
                # one command whose first word was `(git`, which is not `git`, so the staging
                # rule never looked at it - and `(... && make e2e-typecheck)` was refused,
                # because `e2e-typecheck)` is not a target this repository has.
                ';' | '|' | '&' | '(' | ')') _char=$_newline ;;
                *) : ;;
            esac
        fi
        _out=$_out$_char
    done
    printf '%s' "$_out"
}


# The exit status of a pipeline is its LAST command's, so `cargo test | tail`
# reports tail's - which is 0 whether the check passed or failed. A suite that failed one
# test of many then reads as one that passed, and it is read that way, because the number
# is the thing a reader trusts over the text scrolling above it.
#
# Only cargo and make are guarded, and only when the pipeline ends in head or tail: those
# are the runs long enough to want to trim and slow enough that nobody runs them twice to
# check. A pipeline ending in grep is left alone - it discards the status too, but it is
# how half of every shell session is written, and a rule nobody can follow is switched off.
is_status_carrying_wrapper() {
    case $1 in
        cargo)
            case $2 in
                test | build | clippy | check | run | doc) return 0 ;;
                *) return 1 ;;
            esac
            ;;
        make)
            case $2 in
                check | test | clippy | fmt-check | guard-test | build | '') return 0 ;;
                *) return 1 ;;
            esac
            ;;
        *) return 1 ;;
    esac
}


# Whether a pipeline ending here threw away a status worth having, and refuse if it did.
# Reads $_pipeline and $_saw_wrapper, and clears both.
end_of_pipeline() {
    _cmd=$(strip_prefixes "$_pipeline")
    set -f
    # shellcheck disable=SC2086  # splitting the command into its words is the point
    set -- $_cmd
    set +f
    if [ "$_saw_wrapper" -eq 1 ] && [ "$#" -gt 0 ]; then
        case ${1##*/} in
            head | tail)
                refuse "A pipeline ending in \`${1##*/}\` reports \`${1##*/}\`'s exit status, not the command's - so a run that failed reads as one that passed." \
                    "Send it to a file and read the file:
  cargo test > /tmp/test.log 2>&1; echo \"exit=\$?\"
  tail -40 /tmp/test.log"
                ;;
            *) : ;;
        esac
    fi
    _saw_wrapper=0
    _pipeline=''
}

# One command in a pipeline has ended at a `|`. Remember whether it was a wrapper whose
# status is the answer, since by the end of the pipeline the text of it is long gone.
end_of_segment() {
    _cmd=$(strip_prefixes "$_pipeline")
    set -f
    # shellcheck disable=SC2086  # splitting the command into its words is the point
    set -- $_cmd
    set +f
    if [ "$#" -gt 0 ] && is_status_carrying_wrapper "${1##*/}" "${2:-}"; then
        _saw_wrapper=1
    fi
    _pipeline=''
}

check_pipeline() {
    # `|` continues a pipeline; `;`, `&&`, `||` and a bare `&` end one. Rebuilt here from
    # the raw line rather than taken from split_on_separators, which throws away which
    # separator it was - and that is the whole question this asks.
    #
    # The terminator appended to the line is what flushes the last pipeline, so that the
    # loop has one way out and one way of ending a pipeline rather than two of each.
    _rest="$1;"
    _pipeline=''
    _quote=''
    _saw_wrapper=0

    while [ -n "$_rest" ]; do
        _char=${_rest%"${_rest#?}"}
        _rest=${_rest#?}

        if [ -n "$_quote" ]; then
            [ "$_char" = "$_quote" ] && _quote=''
            _pipeline=$_pipeline$_char
            continue
        fi

        case $_char in
            "'" | '"')
                _quote=$_char
                _pipeline=$_pipeline$_char
                ;;
            '|')
                end_of_segment
                ;;
            '&')
                # `2>&1` and `>&2` are redirections, and `&>file` is bash's spelling of
                # one. None of them ends anything, and reading them as `&` split
                # `cargo test 2>&1 | tail` into two pipelines - which lost the
                # wrapper and let exactly the case this is for through.
                _last=${_pipeline#"${_pipeline%?}"}
                _next=${_rest%"${_rest#?}"}
                if [ "$_last" = '>' ] || [ "$_next" = '>' ]; then
                    _pipeline=$_pipeline$_char
                else
                    end_of_pipeline
                fi
                ;;
            ';')
                end_of_pipeline
                ;;
            *)
                _pipeline=$_pipeline$_char
                ;;
        esac
    done
}

check_line() {
    check_pipeline "$1"
    _parts=$(split_on_separators "$1")

    set -f
    _saved_ifs=$IFS
    IFS='
'
    # shellcheck disable=SC2086  # splitting on the separators is the point
    set -- $_parts
    IFS=$_saved_ifs
    set +f

    for _part in "$@"; do
        check_part "$_part"
    done
}

check_line "$command"

exit 0
