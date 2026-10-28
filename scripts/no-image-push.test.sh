#!/bin/sh
# What no-image-push.sh refuses, and what it must go on allowing. A wrong pass lets an
# image out, so the refusals are the cases that matter; the allowed ones keep it from
# being switched off for refusing the repository's own builds. The whole-repository run is
# tested on a repository made here, with a push in a workflow and in a file whose name git
# would quote, and then run over this repository as it stands, which must pass.
#
# Run by `make check`.

set -eu

# A pre-commit hook in a linked worktree runs with GIT_DIR and GIT_INDEX_FILE set, and
# `git -C <dir>` does not override them: the scratch repositories below would be made in,
# and staged into, the real one. None of these may reach the cases.
unset GIT_DIR GIT_INDEX_FILE GIT_WORK_TREE GIT_PREFIX GIT_COMMON_DIR GIT_OBJECT_DIRECTORY \
    GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_NAMESPACE GIT_CEILING_DIRECTORIES

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
check refuse run.sh 'docker buildx build --cache-to type=registry,ref=ghcr.io/x/c .'
check refuse run.sh 'skopeo sync --src dir x --dest docker ghcr.io/x'
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
check refuse run.sh 'docker compose -f local/compose.yml push'
check refuse run.sh 'podman compose -f a.yml -f b.yml push'
check refuse run.sh 'docker-compose -f a.yml -f b.yml push'
check refuse run.sh 'ctr -n k8s.io images push x'
check refuse run.sh 'docker --context remote image push x'
check refuse run.sh 'sudo -u root podman push x'
check refuse run.sh 'nice -n 5 podman push x'
check refuse run.sh 'xargs -n 1 podman push < tags'
check refuse run.sh 'timeout 600 docker push x'
check refuse run.sh 'timeout -s KILL 600 docker push x'
check refuse run.sh 'doas podman push x'
check refuse run.sh 'time podman push x'
check refuse run.sh 'nohup podman push x'
check refuse run.sh 'exec podman push x'
check refuse run.sh 'command podman push x'
check refuse run.sh 'ionice -c 3 podman push x'
check refuse run.sh 'eval podman push x'
check refuse run.sh 'if a; then b; else podman push x; fi'
check refuse run.sh 'while true; do podman push x; done'
check refuse run.sh 'until false; do podman push x; done'
check refuse run.sh 'if podman push x; then :; fi'
check refuse run.sh 'while podman push x; do :; done'
check refuse run.sh 'until podman push x; do :; done'
check refuse run.sh "ksh -c 'podman push x'"
check refuse run.sh "zsh -c 'podman push x'"
check refuse run.sh "dash -c 'podman push x'"
check refuse run.sh "bash -o pipefail -c 'podman push x'"
check refuse run.sh 'buildah commit ctr docker://ghcr.io/x/h'
check refuse run.sh 'gh release edit v1 --draft=false'
check refuse run.sh 'gh api repos/apache/x/releases -f tag_name=v1'
check refuse run.sh 'podman push x
'
# a login is the step before a push, and nothing here needs one
check refuse run.sh 'echo "$T" | podman login ghcr.io -u x --password-stdin'
check refuse ci.yml '        run: docker login -u x -p "$T"'
check refuse run.sh 'skopeo login ghcr.io'
check refuse run.sh 'oras login ghcr.io'
check refuse run.sh 'crane auth login ghcr.io'
check refuse ci.yml '      - run: podman push x'
check refuse ci.yml '      deploy: podman push x'
# workflows
check refuse ci.yml '      - uses: ncipollo/release-action@v1'
check refuse ci.yml '      - uses: actions/create-release@v1'
check refuse ci.yml '      - uses: goreleaser/goreleaser-action@v6'
check refuse ci.yml '      - uses: googleapis/release-please-action@v4'
check refuse ci.yml '      - uses: aws-actions/amazon-ecr-login@v2'
check refuse ci.yml '      - uses: azure/docker-login@v2'
check refuse ci.yml '      - uses: mr-smithers-excellent/docker-build-push@v6'
check refuse ci.yml '      - uses: macbre/push-to-ghcr@v13'
check refuse ci.yml '      - uses: actions/deploy-pages@v5'
check refuse ci.yml '  pages: write'
check refuse ci.yml '          push: true'
check refuse docker-bake.hcl '  output = ["type=registry"]'
check refuse docker-bake.json '{"target": {"h": {"push": true}}}'
check refuse Dockerfile 'CMD ["podman", "push", "x"]'
check refuse compose.yml '    command: ["docker","login","ghcr.io"]'
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
check refuse main.rs 'let rt = "podman";
let cmd = format!("{rt} push {tag}");'
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
check pass run.sh 'docker buildx imagetools inspect node:24'
check pass run.sh 'docker --debug logs push'
check pass run.sh 'podman --remote run --rm --name login alpine true'
check pass run.sh 'skopeo copy docker://docker.io/library/node:24 oci:node'
check pass run.sh 'ko build --local ./cmd/x'
check pass run.sh '$GIT push origin main'
check pass run.sh '"$git" push origin HEAD'
check pass run.sh 'bash -o pipefail -c "cargo test 2>&1 | tee t.log"'
check pass run.sh 'bash --login -c "make check"'
check pass run.sh 'sudo apt-get install -y podman'
check pass run.sh 'env FOO=1 cargo test'
check pass run.sh 'nice -n 5 make check'
check pass run.sh 'ls | xargs -0 grep -l x'
# an escaped backslash ends the line, so the push on the next one runs
check refuse run.sh 'echo a \\
podman push x'
check pass main.rs 'return Err("a login is the harness own".into());'
check pass ci.yml '      - uses: actions/checkout@v7'
check pass ci.yml '      - uses: 1password/load-secrets-action@v2'
check pass ci.yml '      - uses: aws-actions/aws-secretsmanager-get-secrets@v2'
check pass run.sh 'GIT_PUSH=true make release-notes'
check pass run.sh 'echo "push: true" > settings.yml'
check pass run.sh '"$SHELL" -c "git push origin main"'
check pass run.sh '$CARGO login'
check pass run.sh '"$PY" scripts/conformance.py --push'
check pass run.sh 'sh "$script" push'
check pass run.sh 'bash "$f" login'
check pass Makefile '	$(MAKE) push'
check pass Makefile '	$(MAKE) -C docs login'
check pass run.sh 'docker buildx build --cache-from type=registry,ref=ghcr.io/x/c --load .'
check pass run.sh 'docker buildx build --cache-from=type=registry,ref=ghcr.io/x/c --load .'
check pass run.sh 'docker run -e auto_push=true img'
check pass run.sh 'podman run --rm img mytool --push'
check pass run.sh 'skopeo sync --src docker --dest dir docker.io/library/alpine /tmp/mirror'
check pass run.sh 'skopeo copy docker://node:24 dir:/tmp/node --retry-times 3'
check pass run.sh 'ko build -L ./cmd/app'
check pass run.sh 'crane auth get index.docker.io'
check pass run.sh 'oras copy x --to-oci-layout ./dir'
check pass ci.yml '      - uses: Swatinem/rust-cache@v2'

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
# untracked files are read too, so nothing is staged
git -C "$repo" init -q
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
# CR line endings do not break a continued line
n=$((n + 1))
printf 'podman \\\r\npush x\r\n' > "$dir/crlf.sh"
status=0
sh "$script" "$dir/crlf.sh" 2>/dev/null || status=$?
if [ "$status" -ne 1 ]; then
    echo 'a push across a CRLF continuation passed, should be refused' >&2
    failures=$((failures + 1))
fi
# a tracked file deleted from the working tree is skipped; a name git lists that is not a
# file, a link to a directory here, fails the run
n=$((n + 1))
repo2="$dir/repo2"
mkdir -p "$repo2/scripts"
cp "$script" "$repo2/scripts/no-image-push.sh"
printf 'podman build -t x .\n' > "$repo2/gone.sh"
git -C "$repo2" init -q
git -C "$repo2" add gone.sh
rm "$repo2/gone.sh"
status=0
sh "$repo2/scripts/no-image-push.sh" 2>/dev/null || status=$?
if [ "$status" -ne 0 ]; then
    echo 'a tracked file deleted from the working tree failed the run, should be skipped' >&2
    failures=$((failures + 1))
fi
n=$((n + 1))
ln -s "$dir" "$repo2/elsewhere"
status=0
sh "$repo2/scripts/no-image-push.sh" 2>/dev/null || status=$?
if [ "$status" -eq 0 ]; then
    echo 'a listed name that is not a file passed, should fail' >&2
    failures=$((failures + 1))
fi
# an untracked binary file, an image tarball say, is not read; once added, it is
n=$((n + 1))
repo4="$dir/repo4"
mkdir -p "$repo4/scripts"
cp "$script" "$repo4/scripts/no-image-push.sh"
printf 'layer\000\npodman push x\n' > "$repo4/harness.tar"
git -C "$repo4" init -q
status=0
sh "$repo4/scripts/no-image-push.sh" 2>/dev/null || status=$?
if [ "$status" -ne 0 ]; then
    echo 'an untracked binary file was read, should be skipped' >&2
    failures=$((failures + 1))
fi
n=$((n + 1))
git -C "$repo4" add harness.tar
status=0
sh "$repo4/scripts/no-image-push.sh" 2>/dev/null || status=$?
if [ "$status" -ne 1 ]; then
    echo 'a binary file added to the index was not read, should be' >&2
    failures=$((failures + 1))
fi
# a repository where git lists nothing: nothing was checked, so it fails
n=$((n + 1))
repo3="$dir/repo3"
mkdir -p "$repo3/scripts"
cp "$script" "$repo3/scripts/no-image-push.sh"
printf '*\n' > "$repo3/.gitignore"
git -C "$repo3" init -q
status=0
sh "$repo3/scripts/no-image-push.sh" 2>/dev/null || status=$?
if [ "$status" -eq 0 ]; then
    echo 'a run where git listed no files passed, should fail' >&2
    failures=$((failures + 1))
fi
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
