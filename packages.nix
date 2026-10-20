{
  lib,
  rustPlatform,
  stdenvNoCC,
  tmuxPlugins,
  makeWrapper,
  bash,
  tmux,
}:
let
  inherit (lib.importJSON ./.claude-plugin/plugin.json) version;
  cargoVersion = (lib.importTOML ./Cargo.toml).package.version;
  source =
    files:
    lib.fileset.toSource {
      root = ./.;
      fileset = lib.fileset.unions ([ ./LICENSE.md ] ++ files);
    };
  meta = {
    homepage = "https://github.com/kallisti-dev/tracon";
    license = lib.licenses.bsd3;
    platforms = [
      "aarch64-darwin"
      "aarch64-linux"
      "x86_64-linux"
    ];
  };

  tracon =
    assert lib.assertMsg (
      cargoVersion == version
    ) "Cargo.toml version ${cargoVersion} differs from .claude-plugin/plugin.json version ${version}";
    rustPlatform.buildRustPackage {
      pname = "tracon";
      inherit version;
      # The tests read both plugins' sources.
      src = source [
        ./Cargo.toml
        ./Cargo.lock
        ./src
        ./tests
        ./tracon.tmux
        ./.claude-plugin
        ./hooks
        ./skills
        ./docs
      ];
      cargoLock.lockFile = ./Cargo.lock;
      # Hooks and tmux run the binary with their own PATH.
      env.TRACON_TMUX = lib.getExe tmux;
      # The tests start tmux servers; the checks run them.
      doCheck = false;
      meta = meta // {
        description = "Terminal approach control for coding agents";
        mainProgram = "tracon";
      };
    };
in
{
  inherit tracon;

  tmux-plugin = tmuxPlugins.mkTmuxPlugin {
    pluginName = "tracon";
    rtpFilePath = "tracon.tmux";
    inherit version;
    src = source [
      ./tracon.tmux
      ./docs
    ];
    nativeBuildInputs = [ makeWrapper ];
    # mkTmuxPlugin sets strictDeps, so patchShebangs finds bash only here.
    buildInputs = [ bash ];
    postInstall = ''
      plugin="$out/share/tmux-plugins/tracon"
      mkdir -p "$plugin/bin"
      ln -s ${lib.getExe tracon} "$plugin/bin/tracon"
      wrapProgram "$plugin/tracon.tmux" --prefix PATH : ${lib.makeBinPath [ tmux ]}
    '';
    meta = meta // {
      description = "tmux plugin for tracon: the agent picker and focus tracking";
    };
  };

  claude-plugin = stdenvNoCC.mkDerivation {
    pname = "tracon-claude-plugin";
    inherit version;
    src = source [
      ./.claude-plugin
      ./hooks
      ./skills
      ./docs
    ];
    dontBuild = true;
    installPhase = ''
      runHook preInstall
      cp -R . "$out"
      mkdir -p "$out/bin"
      ln -s ${lib.getExe tracon} "$out/bin/tracon"
      runHook postInstall
    '';
    meta = meta // {
      description = "Claude Code plugin with the state hooks for tracon";
    };
  };
}
