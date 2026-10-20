{
  description = "Spike A-2: the harness image from numtide/llm-agents.nix, built with nix2container";

  # The harness packages come prebuilt from numtide's cache; the key is the one llm-agents.nix
  # publishes in its README and its own flake.nix.
  nixConfig = {
    extra-substituters = [ "https://cache.numtide.com" ];
    extra-trusted-public-keys = [ "niks3.numtide.com-1:DTx8wZduET09hRmMtKdQDxNNthLQETkc/yaX7M4qK0g=" ];
  };

  inputs = {
    # Pinned by commit here and again, with a narHash, in flake.lock.
    llm-agents.url = "github:numtide/llm-agents.nix/06667f1e155921a30427e4277c42ede088521396";
    nixpkgs.follows = "llm-agents/nixpkgs";
    nix2container.url = "github:nlewo/nix2container/535166ecac3fcdff43787aa958dd7e954770581d";
    nix2container.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, llm-agents, nix2container, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      agents = llm-agents.packages.${system};
      n2c = nix2container.packages.${system}.nix2container;

      harnesses = with agents; [
        claude-code codex gemini-cli copilot-cli opencode qwen-code grok pi muse-code
      ];
      # What today's Dockerfile installs beside the harnesses (apt list), plus Node for the
      # smoke's relay and the shell the smoke runs under.
      tools = with pkgs; [
        bashInteractive coreutils findutils gnugrep gnused gawk git ripgrep procps less curl
        xz python3 nodejs_24
      ];

      # The launcher's fixed layout and uid 1000, which the Dockerfile makes with useradd and
      # mkdir. The image holds no policy; the runtime mounts over these. `perms` below
      # applies to this derivation only, so it shares no directory with the others
      # (nix2container refuses a directory two entries give different perms): /opt comes
      # from memoryStandin and /etc from etcFiles.
      layout = pkgs.runCommand "harness-layout" { } ''
        mkdir -p $out/home/user $out/work $out/run/memory $out/memory $out/host-home $out/tmp
      '';

      etcFiles = pkgs.runCommand "harness-etc" { } ''
        mkdir -p $out/etc
        cat > $out/etc/passwd <<'EOF'
        root:x:0:0:root:/root:/bin/sh
        user:x:1000:1000:user:/home/user:/bin/bash
        nobody:x:65534:65534:nobody:/nonexistent:/bin/false
        EOF
        cat > $out/etc/group <<'EOF'
        root:x:0:
        user:x:1000:
        nobody:x:65534:
        EOF
        sed -i 's/^ *//' $out/etc/passwd $out/etc/group
      '';

      # The memory binary is copied in by the build, as `make image` does; this stand-in
      # prints a version so the image is complete without a release build.
      memoryStandin = pkgs.writeTextFile {
        name = "memory-standin";
        destination = "/opt/memory/memory";
        executable = true;
        text = ''
          #!/bin/sh
          echo "memory 0.0.0-standin"
        '';
      };
    in
    {
      packages.${system} = {
        image = n2c.buildImage {
          name = "localhost/harness-nix";
          tag = "spike";
          maxLayers = 60;
          copyToRoot = [
            (pkgs.buildEnv {
              name = "harness-root";
              paths = harnesses ++ tools;
              pathsToLink = [ "/bin" "/share" ];
            })
            pkgs.dockerTools.usrBinEnv
            pkgs.dockerTools.caCertificates
            layout
            etcFiles
            memoryStandin
          ];
          perms = [
            { path = layout; regex = "/home/user|/work"; mode = "0755"; uid = 1000; gid = 1000; uname = "user"; gname = "user"; }
            { path = layout; regex = "/tmp"; mode = "1777"; }
          ];
          config = {
            User = "1000:1000";
            WorkingDir = "/work";
            Env = [
              "HOME=/home/user"
              "PATH=/bin:/usr/bin"
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              "MUSE_NO_AUTO_UPDATE=1"
            ];
            Labels = {
              "org.opencontainers.image.title" = "durable-memory harness (Nix spike)";
            };
          };
        };
        default = pkgs.symlinkJoin { name = "harnesses"; paths = harnesses; };
      };
    };
}
