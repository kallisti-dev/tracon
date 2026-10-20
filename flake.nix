{
  description = "Terminal approach control for coding agents";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      eachSystem = nixpkgs.lib.genAttrs systems;
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          inherit (pkgs) lib;
          inherit (pkgs.stdenv.hostPlatform) isDarwin;
          packages = {
            inherit (pkgs.callPackage ./packages.nix { })
              tracon
              tmux-plugin
              claude-plugin
              ;
          };
          inherit
            (import nixpkgs {
              inherit system;
              config.allowUnfreePredicate = pkg: lib.getName pkg == "claude-code";
            })
            claude-code
            ;
          qualityTools = with pkgs; [
            actionlint
            jq
            nixfmt
            rustfmt
            shellcheck
            shfmt
          ];
          # The tests against both plugins as packaged, in place of the
          # plugins assembled from the source.
          testPlugins = ''
            export TEST_TMUX_PLUGIN=${packages.tmux-plugin}/share/tmux-plugins/tracon
            export TEST_CLAUDE_PLUGIN=${packages.claude-plugin}
          '';
          # The crate's tests, which start real tmux servers.
          tests =
            name: preCheck:
            packages.tracon.overrideAttrs (old: {
              pname = "tracon-${name}";
              doCheck = true;
              nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.bash ];
              nativeCheckInputs = [ pkgs.tmux ];
              # Darwin's build sandbox cannot execute /usr/bin/env. Only the
              # disposable test copy is patched; the plugin stays portable.
              postPatch = "patchShebangs --build tracon.tmux";
              inherit preCheck;
            });
          check =
            name: deps: command:
            pkgs.runCommand name { nativeBuildInputs = deps; } ''
              cp -R ${./.} project
              chmod -R u+w project
              cd project
              ${command}
              touch "$out"
            '';
          individualChecks = {
            formatting = check "formatting" qualityTools ''
              nixfmt --check *.nix
              rustfmt --check --edition 2024 src/main.rs src/lib.rs tests/*.rs
              shfmt -d -i 2 -ci tracon.tmux
            '';
            lint = check "lint" qualityTools ''
              shellcheck tracon.tmux
              jq -e . .claude-plugin/plugin.json hooks/hooks.json >/dev/null
              actionlint .github/workflows/*.yml
            '';
            clippy = packages.tracon.overrideAttrs (old: {
              pname = "tracon-clippy";
              nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
              buildPhase = "cargo clippy --all-targets --offline -- -D warnings";
              installPhase = "touch $out";
            });
            behavior = tests "behavior" "";
            packaging = tests "packaging" testPlugins;
            # Tests with the real Claude Code. It fails to format numbers
            # inside the Darwin sandbox, which nixpkgs also disables to
            # build it there.
            agents =
              (tests "agents" ''
                ${testPlugins}
                export HOME="$TMPDIR/home" CLAUDE_CONFIG_DIR="$TMPDIR/claude"
                export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
                mkdir -p "$HOME"
                ${lib.getExe claude-code} plugin validate --strict .
                ${lib.getExe claude-code} plugin validate --strict ${packages.claude-plugin}
                export CLAUDE_BINARY=${lib.getExe claude-code}
              '').overrideAttrs
                {
                  checkFlags = [ "real_" ];
                  __noChroot = isDarwin;
                };
            nonflake =
              let
                pinned = import ./default.nix { inherit system; };
                supplied = import ./default.nix { inherit pkgs; };
                drvPaths = lib.mapAttrs (_: package: package.drvPath);
              in
              assert lib.assertMsg (
                drvPaths pinned == drvPaths packages
              ) "default.nix and the flake must build the same pinned packages";
              assert lib.assertMsg (
                drvPaths supplied == drvPaths packages
              ) "default.nix must support a caller-supplied pkgs";
              pkgs.linkFarm "nonflake-packages" (lib.mapAttrsToList (name: path: { inherit name path; }) pinned);
          };
        in
        {
          packages = packages // {
            default = packages.tracon;
          };
          checks = individualChecks // {
            all = pkgs.linkFarm "checks-${system}" (
              lib.mapAttrsToList (name: path: { inherit name path; }) individualChecks
            );
          };
          devShells.default = pkgs.mkShell {
            packages =
              qualityTools
              ++ (with pkgs; [
                cargo
                clippy
                rust-analyzer
                rustc
                tmux
                nix
                direnv
              ]);
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          };
          formatter = pkgs.nixfmt;
        };
    in
    {
      packages = eachSystem (system: (forSystem system).packages);
      checks = eachSystem (system: (forSystem system).checks);
      devShells = eachSystem (system: (forSystem system).devShells);
      formatter = eachSystem (system: (forSystem system).formatter);
    };
}
