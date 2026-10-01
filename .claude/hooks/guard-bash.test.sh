#!/bin/sh
# What guard-bash.sh refuses, and what it must go on allowing.
#
# The second half is the half that matters. A guard that refuses too much is worse than
# no guard at all: it is discovered mid-task, worked around, and then switched off. Every
# allowed case below is a command this repository's own instructions ask for.
#
# Run by `make check`. Adapted from apache/tooling-agenda; the cases about keeping a
# toolchain inside a container are gone, since cargo runs on the host here.

set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
guard="$here/guard-bash.sh"

failures=0

# A JSON string, the way Claude Code would have written it: backslashes and quotes
# escaped, and a real newline sent as \n - which is the form the guard has to read a
# multi-line command out of.
as_json_string() {
    printf '%s' "$1" |
        sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' |
        awk 'NR > 1 { printf "\\n" } { printf "%s", $0 }'
}

payload_for() {
    printf '{"session_id":"t","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"%s","description":"%s"}}' \
        "$(as_json_string "$1")" "$(as_json_string "${2:-a command}")"
}

run_guard() {
    status=0
    output=$(payload_for "$1" "${2:-}" | sh "$guard" 2>&1) || status=$?
}

allow() {
    run_guard "$@"
    if [ "$status" -ne 0 ]; then
        printf 'refused a command it should allow: %s\n  %s\n' "$1" "$output" >&2
        failures=$((failures + 1))
    fi
}

refuse() {
    run_guard "$@"
    if [ "$status" -ne 2 ]; then
        printf 'allowed a command it should refuse: %s\n' "$1" >&2
        failures=$((failures + 1))
    fi
}

# A pipeline's exit status is its last command's, so trimming a long run with head or tail
# throws away the answer and reports 0 in its place.
refuse 'cargo test | tail -25'
refuse 'cargo test 2>&1 | tail -40'
refuse 'cargo clippy --workspace 2>&1 | grep -viE "warning" | tail -8'
refuse 'make check | head -30'
refuse 'make test | tail -12'
refuse 'cargo build --release 2>&1 | tail -5'
# ...but only for cargo and make, and only where the status is what was thrown away
allow 'git log --oneline | head -5'
allow 'ls crates/ | tail -20'
allow 'gh pr checks 7 | head'
allow 'cat PLAN.md | tail -40'
# grep is left alone: it discards the status too, but a rule against it is one nobody
# could work under, and an unfollowable rule gets the whole guard switched off
allow 'cargo test 2>&1 | grep -c "test result"'
# the fix the refusal asks for, which has to keep working
allow 'cargo test > /tmp/test.log 2>&1'
allow 'tail -40 /tmp/test.log'
# a wrapper in one pipeline does not condemn a head or tail in the next
allow 'make check > /tmp/c.log 2>&1; tail -5 /tmp/c.log'
# cargo subcommands whose output is the answer rather than their status
allow 'cargo tree | head -30'
allow 'cargo metadata --format-version 1 | head -c 400'

# A subshell ends a command the way a semicolon does. While `(` and `)` were ordinary
# characters, the bracket stuck to the word beside it: the first word of `(git add -A)` was
# `(git`, which no rule here is about, so the whole of it went through unread.
refuse '(git add -A)'
refuse '{ git add . ; }'
# ...and the other half: a command at the end of a subshell is still read as that command.
allow '(make fmt && make check)'
allow '(cargo build && cargo test)'
allow 'echo one && (git status)'

# Staging
refuse 'git add -A'
refuse 'git add .'
refuse 'git add --all'
refuse 'git add -u'
refuse 'git add -- .'
allow 'git add crates/core/src/lib.rs'
allow 'git add -- crates/core/src/lib.rs PLAN.md'
allow 'git add .claude/settings.json'

# The shared stash stack
refuse 'git stash'
refuse 'git stash pop'
refuse 'git stash save wip'
refuse 'git stash drop'
refuse 'git stash push -u'
allow 'git stash push -u -m podling-names-wip'
allow 'git stash list'
allow 'git stash apply 0d2e4a1'

# The pre-commit gate
refuse 'git commit --no-verify -m "fix: thing"'
refuse 'git commit -n -m "fix: thing"'
refuse 'git push --no-verify'
allow 'git commit -m "fix: read the office through a comma"'
allow 'git push -n origin HEAD'
allow 'git push origin HEAD'

# What a -m carries is a message. Read as flags, it refused sentences this repository has
# every reason to write: the first two are refused by the guard on main, and the third by
# the -a rule below had the message not been skipped first.
allow 'git commit -m "docs: --no-verify is never passed here"'
allow 'git commit -m "chore: -n is not an escape hatch"'
allow 'git commit -m "guard: refuse git commit -a"'
allow 'git commit --amend -m "fix: thing"'

# `git commit -a` stages every modified file, which is `git add -u` under another name -
# and that one is refused a few cases above.
refuse 'git commit -a -m "fix: thing"'
refuse 'git commit --all -m "fix: thing"'
refuse 'git commit -am "fix: thing"'
# git bundles short options, so `a` is not always the first letter of the cluster.
refuse 'git commit -va -m "fix: thing"'
refuse 'git commit -qva -m "fix: thing"'
# ...but a cluster stops at the first option that takes a value, since what follows it is
# that value: `-ma` is `-m a` and `-qma` is `-q -m a`, and in both the `a` is the message.
# One case per letter that takes a value, because they are not otherwise checked: dropping
# any of the other six from the list left all 206 cases passing.
allow 'git commit -ma'
allow 'git commit -qma'
allow 'git commit -Fa'
allow 'git commit -ca'
allow 'git commit -Ca'
allow 'git commit -ta'
allow 'git commit -ua'
allow 'git commit -Sa'

# git's own options come before the subcommand. Reading the word after `git` as the
# subcommand took `-C` for one, and every rule above was one `-C` away from not applying.
# The directory is a word to parse past, not a place: nothing here resolves it, and no
# such worktree need exist.
refuse 'git -C ../durable-memory-topic add -A'
refuse 'git -C../durable-memory-topic add -A'
refuse 'git -C ../durable-memory-topic commit --no-verify -m "fix: thing"'
refuse 'git -C ../durable-memory-topic commit -am "fix: thing"'
refuse 'git -c user.name=x -C ../durable-memory-topic add -A'
refuse 'git --no-pager -C ../durable-memory-topic stash pop'
refuse 'git --git-dir=../topic/.git --work-tree=../topic add .'
refuse 'git --git-dir ../topic/.git add -A'
refuse 'git --namespace ns add -A'
allow 'git -C ../durable-memory-topic add crates/core/src/lib.rs'
allow 'git -C ../durable-memory-topic log --oneline -5'

# The subcommand is the first word that is NOT an option. Stopping the walk at the first
# unrecognised one made the fix a single flag wide: git has more options of its own than
# any list here will hold, and each was a way past every rule above.
refuse 'git --no-optional-locks add -A'
refuse 'git --glob-pathspecs add -A'
refuse 'git --icase-pathspecs commit --no-verify -m "fix: thing"'
refuse 'git -p stash pop'
refuse 'git --no-advice --literal-pathspecs add .'
refuse 'git --attr-source=HEAD add -A'
# ...and one whose value is the next word must skip that word too, or the value becomes
# the subcommand. `git --attr-source HEAD status` runs status; `git --exec-path add -A`
# prints the exec path and stages nothing, which is why that one takes no separated value.
refuse 'git --attr-source HEAD add -A'
refuse 'git --config-env user.name=X add -A'
# --shallow-file is one of these and is in no list of options that take a path. It is in
# handle_options() in git.c, which is where the list had to be read from, and
# `git --shallow-file "" add -A` does stage everything.
refuse 'git --shallow-file /dev/null add -A'
refuse 'git --shallow-file "" commit --no-verify -m "fix: thing"'

# A quoted value with a space in it is one value. Read as two words it moved the
# subcommand along by one, and the whole call went unchecked.
refuse 'git -C "../my worktree" add -A'
refuse "git -C '../my worktree' commit --no-verify -m 'fix: thing'"
refuse 'git --git-dir "../my repo/.git" add -A'

# A backslash is the other way to write that space, and the way a shell's own completion
# writes it. Emitting the space it escaped split the value back into two words, which moved
# the subcommand along by one and left the call unchecked - the hole the quoted spelling
# above closes, reopened by the other spelling of the same path.
refuse 'git -C ../my\ worktree add -A'
refuse 'git -C ../my\ worktree commit --no-verify -m "fix: thing"'
refuse 'git -C ../my\ worktree stash pop'
refuse 'git --git-dir ../my\ repo/.git add -A'
# ...and the same split, costing the other way round: a value that came apart into words
# was read as flags, so a message spelled without quotes was refused for the sentence in it.
allow 'git commit -m fix:\ refuse\ -a\ everywhere'
allow 'git commit -m docs:\ --no-verify\ is\ never\ passed\ here'
allow 'git add docs/my\ notes.md'
allow 'git stash push -u -m podling\ names\ wip'

# ...but a quoted single word is just a quoted word. `git add '*'` is quoted precisely so
# that git sees the glob rather than the shell, and it is the shape the rule is for.
refuse "git add '*'"
refuse 'git add "."'
refuse "git commit '--no-verify' -m 'fix: thing'"

# A quote nobody closed is a command no shell will run, but it must not be a way past
# either: what it holds is read by the same rule as what a closed one holds.
refuse "git commit '-a"
allow 'git commit -m "fix: an unterminated -a'

# An empty pair of quotes is dropped by a shell, so the guard has to drop it too. Standing
# in for it with a placeholder broke the flag either side of it into a word that matched
# nothing.
refuse "git add ''-A"
refuse 'git add ""-A'
refuse 'git add -A""'
refuse "git commit --no-ver''ify -m 'fix: thing'"
# Empty quotes at the end of a word, where keeping the placeholder would append it to the
# flag: `--no-verify_` is not `--no-verify`, and the rule is matched on the whole word.
refuse "git commit --no-verify'' -m 'fix: thing'"
refuse 'git commit --all"" -m "fix: thing"'
# ...but a word that is nothing but empty quotes is an argument and keeps its place, or
# the option before it takes the subcommand as its value instead.
refuse "git --shallow-file '' add -A"
refuse 'git --git-dir "" add -A'

# git bundles short options for `add` as well as for `commit`, and -A is the flag this
# whole rule is named after. Reading a cluster on one side and whole words on the other
# left the shorter spelling of the older rule open.
refuse 'git add -vA'
refuse 'git add -Af'
refuse 'git add -fA'
refuse 'git add -vu'
allow 'git add -p'
allow 'git add -n crates/core/src/lib.rs'
allow 'git add -f target/generated.rs'

# A stash message attached to its option is still a message. Only the spaced spellings were
# looked for, so these were refused for having none.
allow 'git stash push -u -m"podling-names-wip"'
allow 'git stash push -u --message="podling-names-wip"'

# Every way a message carries what looks like a flag. These five were refused by the
# option-skipping this replaced, and the refusal named an `-a` the session never wrote -
# which on a branch whose commits are about that very flag leaves no wording that
# satisfies the guard.
allow "git commit -m \$'fix: the guard refuses -a now'"
allow 'git commit -m"fix: refuse -a everywhere"'
allow 'git commit --message="fix: refuse -a everywhere"'
allow 'git commit -m "fix: the \"guard\" refuses -a"'
allow 'git commit --author="Sander -a Striker <s@example.invalid>" -m "fix: thing"'

# An apostrophe written the way a shell takes one. Nothing before this refused it; it is
# here because the `\'` is what the outside-quotes backslash rule is for, and without that
# rule the rest of the sentence reads as unquoted words.
allow "git commit -m 'don'\''t refuse this -a'"
# The escaped character is the word, so dropping it takes the flag with it: `git add \-A`
# is `git add -A` to a shell, and `A` to a guard that read the backslash and forgot what
# came after it.
refuse 'git add \-A'
refuse 'git commit \--no-verify -m "fix: thing"'
# ...and a plain single-quoted run is the one place a backslash escapes nothing. `'don\'t
# -a'` closes at that quote, which leaves `-a` a bare word - so reading it the way `$'...'`
# below is read would swallow a flag into a sentence that never held one.
refuse "git commit -m 'don\\'t -a'"

# `$'...'` is the other place an apostrophe gets written, and there the backslash escapes
# it from inside the quotes rather than between two runs of them. Read as a plain single
# quote, the `\'` closed the run early and the rest of the sentence became bare words - so
# a message saying what this branch is about was refused for the flags in it.
allow "git commit -m \$'don\\'t refuse this -a'"
allow "git commit -m \$'don\\'t skip --no-verify here'"
allow "git commit -m \$'fix: thing\nthe body mentions -a'"
# ...and the same reading let a flag hide in one. `$'-a'` is the word `-a`, and leaving the
# `\$` in front of it made a word that matched no rule here.
refuse "git commit \$'-a' -m 'fix: thing'"
refuse "git commit \$'--no-verify' -m 'fix: thing'"
refuse "git add \$'-A'"
# An empty one is an empty argument, the same as `''`, and has to keep its place or the
# option before it takes the subcommand for its value.
refuse "git --shallow-file \$'' add -A"

# -S and --gpg-sign take a value only when it is attached, so reading the next word as
# theirs swallowed the flag after them - and `git commit -S -a -m x` is an ordinary
# command that walked past both the new rule and the one already on main.
refuse 'git commit -S -a -m "fix: thing"'
refuse 'git commit -S --no-verify -m "fix: thing"'
refuse 'git commit --gpg-sign --no-verify -m "fix: thing"'
refuse 'git commit --gpg-sign -a -m "fix: thing"'
allow 'git commit -S -m "fix: thing"'

# Everything else this session does all day
allow 'ls -la crates/core'
allow 'git log --oneline -5 | head'
allow 'git status -sb'
allow 'gh pr view 621 --json body'
allow 'grep -rn Frontmatter crates'

# The description is not the command. Claude writes prose there, and prose about
# running pytest on the host must not be read as running pytest on the host.
allow 'ls crates' 'List the crates before running cargo test on the host'

# A heredoc body is not commands either. Writing an issue, a pull request description or a
# commit message means putting prose into a file, and prose about this repository names the
# very tools the guard refuses. The shell runs none of it - a quoted delimiter is not even
# expanded - so neither may this. Written with a heredoc of its own, since quoting a
# multi-line command inline is exactly the awkwardness these cases are about.
allow "$(cat <<'CASE'
cat > body.md <<'MD'
Measured over app, scripts and server.py with:

    uv run ruff check --select G004

MD
CASE
)"
allow "$(cat <<'CASE'
cat > body.md <<'MD'
Every one of these is prose about a command, not a command:

yarn build
npm install
git add -A
git stash pop
git commit --no-verify
pytest tests/api -q
make test

Posted by Claude on behalf of @sstriker
MD
gh api -X POST repos/:owner/:repo/issues -F body=@body.md
CASE
)"
allow "$(cat <<'CASE'
git commit -F - <<'MSG'
fix: the guard reads a heredoc body as commands

The body below was refused as uv on the host:

uv run pytest tests/api -q
MSG
CASE
)"
# every spelling of the operator: bare, double-quoted, and the tab-stripping form
allow "$(cat <<'CASE'
cat <<EOF > note.txt
pytest tests/api -q
EOF
CASE
)"
allow "$(cat <<'CASE'
cat <<"EOF" > note.txt
make test
EOF
CASE
)"
allow "$(printf 'cat <<-EOF > note.txt\n\tyarn test\n\tEOF\n')"

# The half that matters: the guard still sees the real commands around a heredoc.
refuse "$(cat <<'CASE'
git add -A
cat > body.md <<'MD'
prose
MD
CASE
)"
refuse "$(cat <<'CASE'
cat > body.md <<'MD'
prose
MD
git stash pop
CASE
)"
refuse "$(cat <<'CASE'
cat > body.md <<'MD'
prose
MD
git add -A
CASE
)"
# The terminator is a line of its own, so a body naming it in passing is still a body.
refuse "$(cat <<'CASE'
cat > body.md <<'MD'
the MD in this line ends nothing
MD
git add .
CASE
)"
# `<<<` is a here-string: it feeds one word and opens no body.
refuse "$(cat <<'CASE'
grep -q x <<<"marker"
git add -A
CASE
)"
# A `<<` inside quotes opens nothing: the shell parses no redirection there. Without that,
# an invented delimiter that some later line happens to match takes every command between
# the two out of the guard's view - which is the one way this change could hide a command
# rather than merely read too few.
refuse "$(cat <<'CASE'
git log --format="%h<<%s" -1
git add -A
CASE
)"

refuse "$(cat <<'CASE'
echo "a << EOF is not a heredoc"
git add -A
EOF
CASE
)"
refuse "$(cat <<'CASE'
grep -n 'x << MD' notes.txt
git commit --no-verify -m "fix: thing"
MD
CASE
)"

# A `#` starts a comment, and the rest of that line is data too - so a `<<` in one opens no
# body either, and cannot be used to hide the lines after it.
refuse "$(cat <<'CASE'
echo hi # <<EOF
git add -A
EOF
CASE
)"
# but `#` starts one only at the start of a word: the fragment below belongs to the url, so
# the heredoc after it is real and its body is still a body
allow "$(cat <<'CASE'
curl -s https://example.org/page#section <<EOF
git add -A
EOF
CASE
)"

# Anything posted to an issue or a pull request says who wrote it.
refuse 'gh issue comment 773 --body "Fixed in the next release."'
refuse 'gh pr comment 800 --body "Rebased."'
refuse 'gh pr create --title "feat: a thing" --body "It does the thing."'
refuse 'gh pr review 800 --request-changes --body "This needs a test."'
refuse 'gh issue edit 773 --body "Updated."'
# a body written to a file moments earlier, which is the shape a long comment takes
refuse "$(cat <<'CASE'
cat > /tmp/guard-case-body.md <<MD
Fixed in the next release.
MD
gh issue comment 773 --body-file /tmp/guard-case-body.md
CASE
)"

# The line makes each of those fine.
allow "$(cat <<'CASE'
gh issue comment 773 --body "Fixed in the next release.

Posted by Claude on behalf of @sstriker"
CASE
)"
# A line of its own is the whole of the rule: a signature inside a sentence is refused the
# same as none at all, because it is not what a reader scanning the foot of a comment sees.
refuse 'gh pr comment 800 --body "Done. Posted by Claude on behalf of @sstriker, thanks."'
allow "$(cat <<'CASE'
cat > /tmp/guard-case-body.md <<MD
Fixed in the next release.

Posted by Claude on behalf of @sstriker
MD
gh issue comment 773 --body-file /tmp/guard-case-body.md
CASE
)"

# Two posts in one call: one signature cannot answer for two, and the second carrying it
# would otherwise satisfy the first.
refuse "$(cat <<'CASE'
gh issue comment 773 --body "First, unsigned."
gh issue comment 796 --body "Second.

Posted by Claude on behalf of @sstriker"
CASE
)"
# even when both carry it, because the count is what makes the search answerable
refuse "$(cat <<'CASE'
gh issue comment 773 --body "First.

Posted by Claude on behalf of @sstriker"
gh issue comment 796 --body "Second.

Posted by Claude on behalf of @sstriker"
CASE
)"

# A path is not the punctuation it arrives wrapped in. Each of these names a file that is there
# and signed, and each was read with its wrapping still attached - so a signed comment was
# refused for a file that "was not there".
signed=$(mktemp) || exit 1
unsigned=$(mktemp) || exit 1
printf 'Fixed.\n\nPosted by Claude on behalf of @sstriker\n' > "$signed"
printf 'Fixed.\n' > "$unsigned"
spaced="$(mktemp -d)/with space"
mkdir -p "$spaced"
cp "$signed" "$spaced/body.md"
trap 'rm -rf "$signed" "$unsigned" "$spaced"' EXIT

allow "gh pr comment 800 --body-file $signed"
allow "gh pr comment 800 --body-file \"$signed\""
refuse "gh pr comment 800 --body-file $unsigned"
# gh takes a flag's value attached as well as apart, and a flag before the noun. Each of these
# reaches the api with a body, and each used to skip the rule without it ever running.
refuse 'gh issue comment 773 -b"Fixed."'
refuse 'gh issue comment 773 -b="Fixed."'
refuse 'gh --repo o/r issue comment 1 -b "Fixed."'
refuse 'gh -R o/r pr comment 1 --body "Fixed."'
# prose is full of dollars, and asking whether a body held one to spot `$(cat file)` switched the
# rule off for most of what this repository writes
# shellcheck disable=SC2016  # the literal dollars are the point
refuse 'gh issue comment 773 --body "The fix was in $PATH handling. No signature."'
# shellcheck disable=SC2016  # the literal dollars are the point
refuse 'gh pr comment 800 --body "Costs $5 a month."'

# The signature starts its line; what the shell puts after it is the shell's business. Requiring
# it to end the line refused every command that carries a flag after the body, which is ordinary.
allow "$(cat <<'CASE'
gh issue create --title "feat: a thing" --body "It does the thing.

Posted by Claude on behalf of @sstriker" --label enhancement
CASE
)"
allow "$(cat <<'CASE'
gh pr comment 800 --body "Done.

Posted by Claude on behalf of @sstriker" && echo posted
CASE
)"

# An unsigned comment whose own text names a path that holds a signed file is allowed, and this
# records the trade rather than hiding it. Closing it meant skipping files whenever `--body`
# appeared, and `--body` appears in the title of any issue written about the flag: an issue
# titled "document the --body flag" with a signed --body-file was refused. A guard that refuses
# somebody doing it right is one they switch off, and this is a nudge, not a boundary.
allow "gh issue comment 809 --body \"The guard reads --body-file $signed when you pass one. No signature here.\""
# ...and the shapes that trade bought back
allow "gh issue create --title \"document the --body flag\" --body-file $signed"
allow "gh pr comment 800 --body-file \"$spaced/body.md\""
# shellcheck disable=SC2016  # the literal is the point: a path the shell has still to expand
allow 'gh pr comment 800 --body-file "$BODY_FILE"'
# a body the shell has still to build is refused: asking whether the value held a `$` to spot
# one switched the rule off for "costs $5 a month" and "echo exit=$?", which is most of the
# prose here. The narrower wrong is refusing this shape.
refuse "gh pr comment 800 --body \"\$(cat $signed)\""
# `gh api` is not covered, and these record that rather than leaving it to be discovered. It
# reaches the same comments and is the only way one is edited, so the gap is real - it is filed
# rather than closed here, because writing that branch cost three of the five defects this rule
# was reviewed for. See #809.
allow "gh api -X PATCH repos/a/b/issues/comments/1 -F body=@$signed"
allow "gh api -X PATCH repos/a/b/issues/comments/1 -F body=@$unsigned"
# reading through those same paths is most of what the api is used for here, and stays allowed
# whatever happens to the gap above
allow 'gh api repos/sstriker/durable-memory/pulls/8/comments'
allow 'gh api repos/a/b/pulls/1/reviews --jq .state'
# Prose about posting is not posting, even inside the body of a post. A signed comment
# explaining how to reply - quoting the command on a line of its own - was counted as a second
# post and refused whole. Counted where the parts are dispatched, it cannot see prose.
allow "$(cat <<'CASE'
gh issue comment 773 --body "To reply, run:

gh issue comment 774 --body ...

Posted by Claude on behalf of @sstriker"
CASE
)"
# ...and two comments in one call are refused whichever gh subcommand posts them
refuse "$(cat <<'CASE'
cat > c.md <<MD
Fixed.

Posted by Claude on behalf of @sstriker
MD
gh pr comment 1 --body-file c.md
gh pr comment 2 --body-file c.md
CASE
)"

# a bare gh reaches the rule with nothing to read, and must not abort the guard
allow 'gh'
allow 'echo one; gh'

# Reading is not posting, and neither is anything else gh does. Every one of these is a
# call this repository asks for, and a guard that refused them would be turned off.
allow 'gh issue view 773'
allow 'gh issue list --state open --limit 100'
allow 'gh pr view 800 --json body'
allow 'gh pr diff 800'
allow 'gh pr checks 800'
allow 'gh run view 123 --log-failed'
allow 'gh api repos/sstriker/durable-memory/pulls/8/comments'
allow 'gh pr edit 800 --add-label ready'
# no body flag: the editor opens and a person types it themselves
allow 'gh pr create --fill'
allow 'gh issue create'
# writing about the rule is not posting under it
allow "$(cat <<'CASE'
cat > /tmp/notes.md <<MD
Run gh issue comment 773 --body "..." to reply.
MD
CASE
)"

if [ "$failures" -ne 0 ]; then
    printf '\n%s guard case(s) failed\n' "$failures" >&2
    exit 1
fi

printf 'guard-bash.sh: every case holds\n'
