# Spike A-2: the harness image from Nix

The same harnesses as `images/harness/Dockerfile`, taken from
[numtide/llm-agents.nix](https://github.com/numtide/llm-agents.nix) and laid into an OCI image
by [nix2container](https://github.com/nlewo/nix2container), as `docs/harness-containers.md`
(A7) proposes. Nothing here is built by the workspace.

| File | What it is |
|---|---|
| `flake.nix` | the image: all nine harnesses, the tools the Dockerfile installs with apt, Python and Node 24, the launcher's fixed paths and uid 1000, and a stand-in `memory` binary |
| `flake.lock` | llm-agents.nix, its nixpkgs, and nix2container, each pinned by commit and `narHash` |

The harness versions are whatever the pinned llm-agents.nix commit carries, not ours: a bump
is `nix flake update llm-agents`, which moves all nine at once. The harnesses come prebuilt
from numtide's binary cache (`cache.numtide.com`, key in `flake.nix`); nix2container's own
tools are built from source.

## Building

No Nix on the host; Nix runs in a container, with its store in a named volume so a rebuild
reuses it.

```sh
cd spikes/harness-containers/image-nix
out=$(mktemp -d)
podman volume create nix-spike-store
podman run --rm -v nix-spike-store:/nix -v "$PWD":/src:Z -v "$out":/out:Z -w /src \
  docker.io/nixos/nix sh -c '
    printf "experimental-features = nix-command flakes\naccept-flake-config = true\nsandbox = false\n" >> /etc/nix/nix.conf
    nix run .#image.copyTo -- oci-archive:/out/harness-nix.tar'
# the archive carries no name, so podman loads it untagged and prints the id
id=$(podman load -q -i "$out/harness-nix.tar" | sed -n 's/^Loaded image: //p')
podman tag "$id" localhost/harness-nix:spike
rm -rf "$out"
```

`sandbox = false`: the spike did not try Nix's build sandbox inside the rootless container.
The derivations that are built here (nix2container, its patched skopeo, the
image's JSON) are the only ones not fetched from a cache. `flake.lock` is written on the
first run if it is missing. Then, from
`spikes/harness-containers`:

```sh
IMAGE=localhost/harness-nix:spike ./run-podman.sh
IMAGE=localhost/harness-nix:spike ./run-podman.sh sh /work/harness-smoke.sh
```

`podman volume rm nix-spike-store` drops the store.
