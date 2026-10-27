#!/bin/sh
# What no-image-push.sh refuses, and what it must go on allowing. A wrong pass lets an
# image out, so the refusals are the cases that matter; the allowed ones keep it from
# being switched off for refusing the repository's own builds. The whole-repository run is
# tested on a repository made here, with a push in a workflow and in a file whose name git
# would quote, and then run over this repository as it stands, which must pass.
#
# Run by `make check`.

set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
script="$here/no-image-push.sh"
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

failures=0
n=0

# check refuse|pass NAME TEXT: a file NAME holding TEXT, and whether the script lets it
# through. TEXT may hold several lines.
check() {
    want=$1 name=$2 text=$3
    n=$((n + 1))
    mkdir -p "$dir/$n"
    file="$dir/$n/$name"
    printf '%s\n' "$text" > "$file"
    status=0
    sh "$script" "$file" 2>/dev/null || status=$?
    if [ "$want" = pass ] && [ "$status" -ne 0 ]; then
        printf 'refused, should pass: %s: %s\n' "$name" "$text" >&2
        failures=$((failures + 1))
    elif [ "$want" = refuse ] && [ "$status" -ne 1 ]; then
        printf 'not refused (exit %s), should be: %s: %s\n' "$status" "$name" "$text" >&2
        failures=$((failures + 1))
    fi
}

# a push, by each tool that can make one
check refuse run.sh 'podman push durable-memory/harness:dev ghcr.io/x/harness:dev'
check refuse run.sh 'docker push ghcr.io/x/harness'
check refuse Makefile '	wslc.exe push $(IMAGE)'
check refuse Makefile '	wslc push $(IMAGE)'
check refuse run.sh 'buildah push harness docker://ghcr.io/x/harness'
check refuse run.sh 'nerdctl push x/harness'
check refuse run.sh 'podman-remote push x'
check refuse run.sh '/usr/bin/podman push x'
check refuse run.sh 'docker image push ghcr.io/x/harness'
check refuse run.sh 'podman manifest push harness docker://ghcr.io/x/harness'
check refuse run.sh 'podman artifact push x'
check refuse run.sh 'docker compose push'
check refuse run.sh 'docker-compose push'
check refuse run.sh 'podman-compose push'
check refuse run.sh 'ctr images push ghcr.io/x/h'
check refuse run.sh 'helm push chart.tgz oci://ghcr.io/x'
check refuse run.sh 'helm registry login ghcr.io'
check refuse run.sh 'podman farm build -t ghcr.io/x/h .'
# past the options, through the words in front, and across a continued line
check refuse run.sh 'docker -H tcp://builder:2375 push ghcr.io/x/h'
check refuse run.sh 'podman --remote push x'
check refuse run.sh 'cd images && podman push x'
check refuse run.sh 'sudo -E podman push x'
check refuse run.sh 'env -i PATH=/usr/bin podman push x'
check refuse run.sh 'echo x | xargs podman push'
check refuse run.sh 'if true; then podman push x; fi'
check refuse run.sh 'for t in a b; do podman push "$t"; done'
check refuse run.sh '! podman push x'
check refuse run.sh "sh -c 'podman push x'"
check refuse run.sh "bash -lc 'podman push x'"
check refuse run.sh 'podman \
  push x'
check refuse run.sh 'docker buildx build \
  --push -t ghcr.io/x/h .'
check refuse run.sh 'PODMAN PUSH x'
# quotes and variables
check refuse run.sh '"podman" push x'
check refuse run.sh "podman 'push' x"
check refuse run.sh '"$runtime" push "$image" ghcr.io/x/h'
check refuse run.sh '$ENGINE push x'
check refuse run.sh '${DOCKER:-docker} push x'
check refuse Makefile '	$(ENGINE) push $(IMAGE)'
check refuse Makefile '	@podman push $(IMAGE)'
# the other ways an image goes out
check refuse run.sh 'docker build --push -t ghcr.io/x/h .'
check refuse ci.yml '        run: docker buildx build --push -t ghcr.io/x/h images/harness'
check refuse ci.yml '        run: docker buildx build --output type=registry .'
check refuse ci.yml '        run: docker buildx build --output type=image,push=true .'
check refuse run.sh 'docker buildx imagetools create -t ghcr.io/x/h a b'
check refuse run.sh 'skopeo copy containers-storage:x docker://ghcr.io/x/h'
check refuse run.sh 'skopeo sync --src dir x --dest docker ghcr.io/x'
check refuse run.sh 'crane push image.tar ghcr.io/x/h'
check refuse run.sh 'crane cp a ghcr.io/x/h'
check refuse run.sh 'crane append -b a -f l.tar -t ghcr.io/x/h'
check refuse run.sh 'gcrane push x'
check refuse run.sh 'oras push ghcr.io/x/h:1 file'
check refuse run.sh 'oras cp a ghcr.io/x/h'
check refuse run.sh 'regctl image copy a ghcr.io/x/h'
check refuse run.sh 'ko publish ./cmd/x'
check refuse run.sh 'gh release upload v1 harness.tar'
check refuse run.sh 'gh release create v1 harness.tar'
# a login is the step before a push, and nothing here needs one
check refuse run.sh 'echo "$T" | podman login ghcr.io -u x --password-stdin'
check refuse ci.yml '        run: docker login -u x -p "$T"'
check refuse run.sh 'skopeo login ghcr.io'
check refuse run.sh 'oras login ghcr.io'
check refuse run.sh 'crane auth login ghcr.io'
# workflows
check refuse ci.yml '      - uses: docker/build-push-action@v6'
check refuse ci.yml '      - uses: redhat-actions/push-to-registry@v2'
check refuse ci.yml '      - uses: docker/login-action@v3'
check refuse ci.yml '      - uses: redhat-actions/podman-login@v1'
check refuse ci.yml '      - uses: docker/bake-action@v5'
check refuse ci.yml '      - uses: softprops/action-gh-release@v2'
check refuse ci.yml '  packages: write'
check refuse ci.yml '  packages: "write"'
check refuse ci.yml '  packages : write'
check refuse ci.yml 'permissions: write-all'
check refuse ci.yml '      - run: |
          docker build -t x .
          docker push x'
check refuse Dockerfile 'RUN podman push x'
# code that runs a push, however the arguments are passed
check refuse main.rs 'Command::new("podman").arg("push").arg(&tag)'
check refuse main.rs 'Command::new(wslc).args(["push", &tag])'
check refuse main.rs 'run(rt, &["push", image])'
check refuse main.rs 'run_ok(&s(&["push", tag]))'
check refuse main.rs 'let args = vec![
    "push",
    tag,
];'
check refuse main.rs 'const VERB: &str = "push";'
check refuse main.rs 'cmd.arg("podman push x")'
check refuse main.rs 'Command::new("podman").args(["login", "ghcr.io"])'
check refuse x.py 'subprocess.run(["podman", "push", tag])'
check refuse x.py "os.system('docker push x')"
check refuse x.mjs 'spawn("podman", ["push", t])'
check refuse x.ts "execFile('docker', ['login'])"

# what the repository does and must go on doing
check pass Makefile '	else podman build -t $(IMAGE) images/harness; fi'
check pass Makefile '		wslc.exe build -t $(IMAGE) "$$(wslpath -w images/harness)"; \
	elif command -v wslc >/dev/null 2>&1; then wslc build -t $(IMAGE) images/harness; \
	else podman build -t $(IMAGE) images/harness; fi'
check pass ci.yml '        run: docker build --pull -t harness:${{ github.sha }} images/harness'
check pass ci.yml '          git push -f origin bot/harness-pins'
check pass ci.yml '          gh pr create --fill --base main --head bot/harness-pins'
check pass ci.yml '  contents: write'
check pass ci.yml '  pull-requests: write'
check pass ci.yml '  packages: none'
check pass ci.yml '  packages: read'
check pass ci.yml '      - uses: actions/upload-artifact@v7'
check pass run.sh 'podman pull docker.io/library/node:24'
check pass run.sh 'podman run --rm -v "$PWD":/r:Z -w /r docker.io/library/node:24 sh x.sh'
check pass run.sh 'podman run --rm --name login alpine true'
check pass run.sh 'podman inspect login'
check pass run.sh 'podman logs push'
check pass run.sh 'podman save harness | tar -x'
check pass run.sh 'podman load -i harness.tar'
check pass run.sh 'podman export "$(podman create x)" | tar -x -C rootfs'
check pass run.sh 'podman compose up -d'
check pass run.sh 'exec "$runtime" run --rm -v "$root":/r x'
check pass run.sh '"$ENGINE" save x -o x.tar'
check pass run.sh 'skopeo inspect docker://docker.io/library/node:24'
check pass run.sh 'gh release download release-1 -R google/gvisor -p x.tar.bz2'
check pass run.sh 'git push origin HEAD'
check pass run.sh 'echo "$x" push'
check pass run.sh 'pushd /tmp && popd'
check pass run.sh 'docker-push-helper --help'
check pass Dockerfile 'RUN npm ci --no-fund --no-audit'
check pass main.rs 'v.push(item);'
check pass main.rs 'Command::new("git").arg("pushd")'
check pass main.rs '(_, AuthMode::Subscription) => (!f.logged_in.contains(&h)).then(|| "login".to_string()),'
check pass x.ts 'items.push(x); const pushed = 1;'

# a file it cannot read, and a directory, fail; they do not pass
n=$((n + 1))
status=0
sh "$script" "$dir/missing" 2>/dev/null || status=$?
if [ "$status" -eq 0 ]; then
    echo 'a missing file passed, should fail' >&2
    failures=$((failures + 1))
fi
n=$((n + 1))
status=0
sh "$script" "$dir" 2>/dev/null || status=$?
if [ "$status" -eq 0 ]; then
    echo 'a directory passed, should fail' >&2
    failures=$((failures + 1))
fi
# an awk that fails is a failure, not a pass
n=$((n + 1))
mkdir -p "$dir/badbin"
printf '#!/bin/sh\nexit 2\n' > "$dir/badbin/awk"
chmod +x "$dir/badbin/awk"
printf 'podman build -t x .\n' > "$dir/ok.sh"
status=0
PATH="$dir/badbin:$PATH" sh "$script" "$dir/ok.sh" 2>/dev/null || status=$?
if [ "$status" -eq 0 ]; then
    echo 'a run where awk failed passed, should fail' >&2
    failures=$((failures + 1))
fi
# NUL bytes, and UTF-16 text, do not hide a push
n=$((n + 1))
printf 'podman push x\n\000\n' > "$dir/nul.sh"
printf 'p\000o\000d\000m\000a\000n\000 \000p\000u\000s\000h\000 \000x\000\n\000' > "$dir/utf16.ps1"
for f in "$dir/nul.sh" "$dir/utf16.ps1"; do
    status=0
    sh "$script" "$f" 2>/dev/null || status=$?
    if [ "$status" -ne 1 ]; then
        echo "a push in $f passed, should be refused" >&2
        failures=$((failures + 1))
    fi
done

# the whole-repository run: a repository with a push in a workflow and in a file whose
# name git quotes, and Markdown that mentions one, must fail and name both files
n=$((n + 1))
repo="$dir/repo"
mkdir -p "$repo/scripts" "$repo/.github/workflows"
cp "$script" "$repo/scripts/no-image-push.sh"
printf '      - run: docker push x\n' > "$repo/.github/workflows/x.yml"
printf 'podman push x\n' > "$repo/scripts/d\303\251ploy.sh"
printf 'podman push x\n' > "$repo/scripts/tab	x.sh"
printf 'Never run `podman push`.\n' > "$repo/README.md"
git -C "$repo" init -q
git -C "$repo" add -A
status=0
err=$(sh "$repo/scripts/no-image-push.sh" 2>&1) || status=$?
case $status:$err in
    1:*x.yml*déploy.sh*tab*x.sh* | 1:*x.yml*tab*x.sh*déploy.sh* | 1:*déploy.sh*x.yml*) : ;;
    *)
        printf 'the whole-repository run missed a push (exit %s): %s\n' "$status" "$err" >&2
        failures=$((failures + 1))
        ;;
esac
case $err in
    *déploy.sh*) : ;;
    *) echo 'the whole-repository run skipped a file whose name git quotes' >&2; failures=$((failures + 1)) ;;
esac
case $err in
    *tab*) : ;;
    *) echo 'the whole-repository run skipped a file with a tab in its name' >&2; failures=$((failures + 1)) ;;
esac
# outside a repository, git cannot list: that fails, it does not pass
n=$((n + 1))
mkdir -p "$dir/norepo/scripts"
cp "$script" "$dir/norepo/scripts/no-image-push.sh"
status=0
GIT_CEILING_DIRECTORIES="$dir" sh "$dir/norepo/scripts/no-image-push.sh" 2>/dev/null || status=$?
if [ "$status" -eq 0 ]; then
    echo 'a run where git could not list the files passed, should fail' >&2
    failures=$((failures + 1))
fi

# this repository as it stands
n=$((n + 1))
if ! sh "$script"; then
    echo 'the repository itself does not pass' >&2
    failures=$((failures + 1))
fi

if [ "$failures" -ne 0 ]; then
    echo "no-image-push.test.sh: $failures failures in $n cases" >&2
    exit 1
fi
echo "no-image-push.sh: every case holds"
