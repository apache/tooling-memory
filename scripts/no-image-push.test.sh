#!/bin/sh
# What no-image-push.sh refuses, and what it must go on allowing. A wrong pass lets an
# image out, so the refusals are the cases that matter; the allowed ones keep it from
# being switched off for refusing the repository's own builds. The last check runs it over
# the repository as it stands, which must pass.
#
# Run by `make check`.

set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
script="$here/no-image-push.sh"
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

failures=0
n=0

# check yes|no NAME LINE: a file NAME holding LINE, and whether the script lets it through
check() {
    want=$1 name=$2 line=$3
    n=$((n + 1))
    mkdir -p "$dir/$n"
    file="$dir/$n/$name"
    printf '%s\n' "$line" > "$file"
    status=0
    sh "$script" "$file" 2>/dev/null || status=$?
    if [ "$want" = pass ] && [ "$status" -ne 0 ]; then
        printf 'refused, should pass: %s: %s\n' "$name" "$line" >&2
        failures=$((failures + 1))
    elif [ "$want" = refuse ] && [ "$status" -eq 0 ]; then
        printf 'passed, should be refused: %s: %s\n' "$name" "$line" >&2
        failures=$((failures + 1))
    fi
}

# a push, by each tool that can make one, as a script, a recipe or a step would run it
check refuse run.sh 'podman push durable-memory/harness:dev ghcr.io/x/harness:dev'
check refuse run.sh 'docker push ghcr.io/x/harness'
check refuse Makefile '	wslc.exe push $(IMAGE)'
check refuse Makefile '	wslc push $(IMAGE)'
check refuse run.sh 'buildah push harness docker://ghcr.io/x/harness'
check refuse run.sh 'nerdctl push x/harness'
check refuse run.sh 'docker image push ghcr.io/x/harness'
check refuse run.sh 'podman manifest push harness docker://ghcr.io/x/harness'
check refuse run.sh 'cd images && podman push x'
check refuse run.sh 'PODMAN PUSH x'
check refuse ci.yml '        run: docker buildx build --push -t ghcr.io/x/h images/harness'
check refuse ci.yml '      - uses: docker/build-push-action@v6'
check refuse ci.yml '      - uses: redhat-actions/push-to-registry@v2'
check refuse ci.yml '  packages: write'
check refuse ci.yml '        run: docker buildx build --output type=registry .'
check refuse ci.yml '        run: docker buildx build --output type=image,push=true .'
check refuse run.sh 'skopeo copy containers-storage:x docker://ghcr.io/x/h'
check refuse run.sh 'crane push image.tar ghcr.io/x/h'
check refuse run.sh 'oras push ghcr.io/x/h:1 file'
check refuse run.sh 'ko publish ./cmd/x'
# a login is the step before a push, and nothing here needs one
check refuse run.sh 'echo "$T" | podman login ghcr.io -u x --password-stdin'
check refuse ci.yml '        run: docker login -u x -p "$T"'
# Rust that runs a push
check refuse main.rs 'Command::new("podman").arg("push").arg(&tag)'
check refuse main.rs 'Command::new(wslc).args(["push", &tag])'
check refuse main.rs 'let c = cmd.args( [ "login", "ghcr.io" ]);'

# what the repository does and must go on doing
check pass Makefile '	else podman build -t $(IMAGE) images/harness; fi'
check pass Makefile '		wslc.exe build -t $(IMAGE) "$$(wslpath -w images/harness)"; \'
check pass ci.yml '        run: docker build --pull -t harness:${{ github.sha }} images/harness'
check pass ci.yml '          git push -f origin bot/harness-pins'
check pass ci.yml '  contents: write'
check pass ci.yml '  packages: none'
check pass ci.yml '  packages: read'
check pass run.sh 'podman pull docker.io/library/node:24'
check pass run.sh 'podman run --rm -v "$PWD":/r:Z -w /r docker.io/library/node:24 sh x.sh'
check pass run.sh 'podman save harness | tar -x'
check pass run.sh 'git push origin HEAD'
check pass run.sh 'ssh-copy-id x'
check pass main.rs 'v.push(item);'
check pass main.rs 'Command::new("git").arg("pushd")'
check pass main.rs 'let s = "push";'
check pass run.sh 'pushd /tmp && popd'
check pass run.sh 'docker-push-helper --help'

# a file it cannot read is a failure, not a pass
n=$((n + 1))
status=0
sh "$script" "$dir/missing" 2>/dev/null || status=$?
if [ "$status" -eq 0 ]; then
    echo 'a missing file passed, should fail' >&2
    failures=$((failures + 1))
fi

# the repository as it stands
if ! sh "$script"; then
    echo 'the repository itself does not pass' >&2
    failures=$((failures + 1))
fi

if [ "$failures" -ne 0 ]; then
    echo "no-image-push.test.sh: $failures of $n cases failed" >&2
    exit 1
fi
echo "no-image-push.sh: every case holds"
