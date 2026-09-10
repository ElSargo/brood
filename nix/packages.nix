{
  pkgs,
  crane,
  sourceInfo ? { },
}:

let
  lib = pkgs.lib;
  profiles = import ./profiles.nix { inherit pkgs; };
  neovimWasm = import ./neovim-wasm.nix { inherit pkgs; };
  sourceRoot = toString ../.;

  appToolPath = lib.makeBinPath profiles.appTools;
  fullDevToolPath = lib.makeBinPath (
    profiles.devTools ++ profiles.fullDevExtras ++ profiles.smokeTools
  );
  nativeLibraryPath = lib.makeLibraryPath (profiles.nativeLibs ++ profiles.gtkRuntime);
  gtkTypelibPath = lib.makeSearchPath "lib/girepository-1.0" profiles.gtkRuntime;
  gtkDataDirs = lib.makeSearchPath "share" profiles.gtkRuntime;
  gsettingsSchemaDir = "${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}/glib-2.0/schemas";
  src = lib.cleanSourceWith {
    src = ../.;
    filter =
      path: type:
      let
        base = baseNameOf path;
        relative = lib.removePrefix "${sourceRoot}/" (toString path);
      in
      base != "target" && relative != "nix" && !lib.hasPrefix "nix/" relative;
  };
  # Host implementation and documentation edits must not rebuild every guest.
  guestSrc = lib.cleanSourceWith {
    inherit src;
    filter =
      path: type:
      let
        relative = lib.removePrefix "${sourceRoot}/" (toString path);
      in
      type == "directory"
      || builtins.elem relative [
        "Cargo.toml"
        "Cargo.lock"
        "tools/brood-build.sh"
        "tools/brood-build.mk"
      ]
      || lib.any (prefix: lib.hasPrefix prefix relative) [
        "plugins/"
        "examples/wasm-"
        "runtimes/"
        "packages/"
        "crates/brood-plugin-sdk/"
        "crates/brood-package-contract/"
        "crates/brood-graphics/"
        "crates/brood-protocol/"
        "crates/brood-keybindings/"
        "crates/brood-wasm/wit/"
      ];
  };
  nativePkgConfigPath = lib.concatStringsSep ":" [
    (lib.makeSearchPathOutput "dev" "lib/pkgconfig" profiles.nativeLibs)
    (lib.makeSearchPath "lib/pkgconfig" profiles.nativeLibs)
    (lib.makeSearchPathOutput "dev" "share/pkgconfig" profiles.nativeLibs)
    (lib.makeSearchPath "share/pkgconfig" profiles.nativeLibs)
  ];

  craneLib = (crane.mkLib pkgs).overrideToolchain profiles.rustToolchainWithTools;
  # The host bindings are generated from WIT/WITX at compile time. Crane's
  # default Cargo filter does not recognize those extensions, so retain them
  # explicitly in every sandboxed host build.
  craneSrc = lib.cleanSourceWith {
    inherit src;
    filter =
      path: type:
      craneLib.filterCargoSources path type
      || lib.hasSuffix ".wit" path
      || lib.hasSuffix ".witx" path
      || lib.hasSuffix ".md" path
      || lib.hasSuffix ".json" path
      || lib.hasSuffix "/.cargo/config.toml" path;
  };

  packageManifestSrc = pkgs.runCommand "brood-package-manifest-source" { } ''
    mkdir -p "$out"
    cp -R ${
      lib.cleanSourceWith {
        src = ../crates/brood-package-manifest;
        filter = craneLib.filterCargoSources;
      }
    }/. "$out/"
    chmod -R u+w "$out"
    cp -R ${
      lib.cleanSourceWith {
        src = ../crates/brood-package-contract;
        filter = craneLib.filterCargoSources;
      }
    } "$out/brood-package-contract"
    substituteInPlace "$out/Cargo.toml" \
      --replace-fail '../brood-package-contract' 'brood-package-contract' \
      --replace-fail '[workspace]' $'[workspace]\nexclude = ["brood-package-contract"]'
  '';

  packageManifestTool = craneLib.buildPackage {
    pname = "brood-package-manifest";
    version = "0.1.0";
    src = packageManifestSrc;
    # Cargo resolves metadata for the shared contract's optional host schema
    # dependency even when this standalone binary does not enable the feature.
    cargoVendorDir = craneLib.vendorMultipleCargoDeps {
      cargoLockList = [
        ../Cargo.lock
        ../crates/brood-package-manifest/Cargo.lock
      ];
    };
    strictDeps = true;
    cargoExtraArgs = "--locked";
    # The repository fixture lives outside this isolated workspace. Its full
    # test suite runs explicitly in CI and development checks.
    doCheck = false;
  };

  # The guest examples are independent Cargo workspaces. Vendor their lock
  # files explicitly so the derivation remains network-free in the Nix
  # sandbox. The conditional entries keep older source snapshots evaluable
  # before the new examples exist in the tree.
  wasmCargoVendorDir =
    assert lib.assertMsg (builtins.pathExists "${guestSrc}/plugins/keybindings/Cargo.lock")
      "plugins/keybindings/Cargo.lock is required by the default client init package";
    assert lib.assertMsg (builtins.pathExists "${guestSrc}/plugins/picker/Cargo.lock")
      "plugins/picker/Cargo.lock is required by the default client init package";
    assert lib.assertMsg (builtins.pathExists "${guestSrc}/plugins/diff-viewer/Cargo.lock")
      "plugins/diff-viewer/Cargo.lock is required by the default client init package";
    craneLib.vendorMultipleCargoDeps {
      cargoLockList = builtins.filter builtins.pathExists [
        "${guestSrc}/Cargo.lock"
        "${guestSrc}/examples/wasm-component-actor/Cargo.lock"
        "${guestSrc}/examples/wasm-facet-probe/Cargo.lock"
        "${guestSrc}/examples/wasm-paired-facet-probe/Cargo.lock"
        "${guestSrc}/examples/wasm-init-actor/Cargo.lock"
        "${guestSrc}/examples/wasm-init-child/Cargo.lock"
        "${guestSrc}/examples/wasm-plugin-graphics/Cargo.lock"
        "${guestSrc}/examples/wasm-preview1-worker/Cargo.lock"
        "${guestSrc}/plugins/component-workspace-watch/Cargo.lock"
        "${guestSrc}/plugins/component-runtime-io/Cargo.lock"
        "${guestSrc}/plugins/coding-agent/Cargo.lock"
        "${guestSrc}/plugins/default-workspace/Cargo.lock"
        "${guestSrc}/plugins/diff-viewer/Cargo.lock"
        "${guestSrc}/plugins/inbox/Cargo.lock"
        "${guestSrc}/plugins/keybindings/Cargo.lock"
        "${guestSrc}/plugins/neovim-client/Cargo.lock"
        "${guestSrc}/plugins/neovim-files/Cargo.lock"
        "${guestSrc}/plugins/picker/Cargo.lock"
        "${guestSrc}/plugins/resize-proof/Cargo.lock"
        "${guestSrc}/plugins/session-switcher/Cargo.lock"
        "${guestSrc}/plugins/status-bar/Cargo.lock"
        "${guestSrc}/plugins/worktree-manager/Cargo.lock"
        "${guestSrc}/plugins/terminal-session/Cargo.lock"
        "${guestSrc}/plugins/terminal-ui/Cargo.lock"
        "${guestSrc}/plugins/workspace-file-log/Cargo.lock"
        "${guestSrc}/runtimes/computer-use-js/Cargo.lock"
      ];
    };

  craneBaseArgs = {
    pname = "brood";
    version = "0.1.0";
    src = craneSrc;
    strictDeps = true;
    nativeBuildInputs = [
      pkgs.pkg-config
      pkgs.mold
    ];
    buildInputs = profiles.nativeLibs;
  };

  # Provenance belongs to the final artifact, not registry dependency caches.
  sourceProvenance = {
    # Flake content identity survives source filtering and the Git-free sandbox.
    BROOD_BUILD_SOURCE_INFO = builtins.toJSON {
      nar_hash = sourceInfo.narHash or null;
      revision = sourceInfo.rev or (sourceInfo.dirtyRev or null);
      modification_status =
        if sourceInfo ? rev then
          "clean"
        else if sourceInfo ? dirtyRev then
          "modified"
        else
          "unavailable";
    };
  };

  # buildDepsOnly intentionally compiles dummy versions of workspace crates.
  # That is enough to cache registry dependencies, but it means every real
  # workspace crate is rebuilt together in buildPackage. Build the local
  # library graph in a few topological layers as well, so edits only invalidate
  # the affected layer and its dependants.
  workspaceDummySrc = craneLib.mkDummySrc craneBaseArgs;

  mkMemberSrc =
    member:
    lib.cleanSourceWith {
      src = ../. + "/${member}";
      filter =
        path: type:
        craneLib.filterCargoSources path type
        || lib.hasSuffix ".wit" path
        || lib.hasSuffix ".witx" path
        || lib.hasSuffix ".md" path
        || lib.hasSuffix ".json" path;
    };

  mkWorkspaceSrc =
    _name: members:
    # Keep the unpack directory at /build/source. Cargo records absolute source
    # paths in fingerprints, so changing this name would invalidate artifacts
    # inherited from the preceding layer.
    pkgs.runCommand "source" { } ''
      mkdir -p "$out"
      cp -R ${workspaceDummySrc}/. "$out/"
      chmod -R u+w "$out"
      cp ${../Cargo.toml} "$out/Cargo.toml"
      ${lib.concatMapStringsSep "\n" (member: ''
        rm -rf "$out/${member}"
        mkdir -p "$out/${dirOf member}"
        cp -R ${mkMemberSrc member} "$out/${member}"
      '') members}
      ${lib.optionalString (builtins.elem "crates/brood-cli" members) ''
        ${lib.concatMapStringsSep "\n"
          (member: ''
            rm -rf "$out/${member}"
            mkdir -p "$out/${dirOf member}"
            cp -R ${mkMemberSrc member} "$out/${member}"
          '')
          [
            "docs"
            "crates/brood-plugin-sdk"
            "examples/wasm-init-child"
            "examples/package-init"
            "examples/self-describing-pair"
          ]
        }
        cp ${../CONTEXT.md} "$out/CONTEXT.md"
        ${lib.optionalString (!(builtins.elem "crates/broodd" members)) ''
          mkdir -p "$out/crates/broodd/src"
          cp ${../crates/broodd/src/config.rs} "$out/crates/broodd/src/config.rs"
        ''}
      ''}
    '';

  foundationMembers = [
    "crates/brood-launch"
    "crates/brood-package-contract"
    "crates/brood-computer-use"
    "crates/brood-dmabuf"
    "crates/brood-graphics"
    "crates/brood-theme"
    "crates/brood-keybindings"
    "crates/brood-layout"
    "crates/brood-profiler"
    "crates/brood-protocol"
    "crates/brood-wayland-drop"
  ];
  serviceMembers = foundationMembers ++ [
    "crates/brood-ipc"
    "crates/brood-transport"
    "crates/brood-wasm"
  ];
  commandMembers = serviceMembers ++ [
    "crates/bmux"
    "crates/brood-capsuled"
    "crates/brood-cli"
    "crates/brood-package-manifest"
  ];

  mkWorkspaceArtifactLayer =
    {
      name,
      members,
      cargoArtifacts,
      buildArgs ? "--locked -p broodd --bin brood",
    }:
    craneLib.cargoBuild (
      craneBaseArgs
      // {
        pname = "brood-${name}";
        src = mkWorkspaceSrc name members;
        inherit cargoArtifacts;
        # Keep the Cargo unit graph identical to buildDepsOnly. Building a
        # subset as roots changes feature unification and recompiles registry
        # crates even when their sources are cached.
        cargoExtraArgs = buildArgs;
      }
    );

  craneLintArgs = craneBaseArgs // {
    cargoExtraArgs = "--workspace --locked";
    env.CARGO_PROFILE = "";
  };

  cargoArtifacts = craneLib.buildDepsOnly (
    craneLintArgs
    // {
      cargoBuildExtraArgs = "--all-targets";
      cargoCheckExtraArgs = "--all-targets";
    }
  );

  clippyCheck = craneLib.cargoClippy (
    craneLintArgs
    // {
      inherit cargoArtifacts;
      cargoClippyExtraArgs = "--all-targets";
    }
  );

  softwareE2eCapture = pkgs.writeShellApplication {
    name = "brood-software-e2e-capture";
    runtimeInputs = [
      pkgs.bash
      pkgs.coreutils
      pkgs.dbus
      pkgs.findutils
      pkgs.grim
      pkgs.gnused
      pkgs.python3
      pkgs.ripgrep
      pkgs.sway
      profiles.westonWithSimpleClients
    ];
    text = ''
      export DBUS_SESSION_BUS_CONFIG=${pkgs.dbus}/share/dbus-1/session.conf
      export __EGL_VENDOR_LIBRARY_FILENAMES=${pkgs.mesa}/share/glvnd/egl_vendor.d/50_mesa.json
      export LIBGL_DRIVERS_PATH=${pkgs.mesa}/lib/dri
      export GBM_BACKENDS_PATH=${pkgs.mesa}/lib/gbm
      exec bash ${src}/tools/software-e2e-capture.sh \
        --brood-bin ${brood}/bin/brood \
        "$@"
    '';
  };

  brooddArtifacts = craneLib.buildDepsOnly (
    craneBaseArgs
    // {
      cargoBuildExtraArgs = "-p broodd --bin brood";
      cargoCheckExtraArgs = "-p broodd --bin brood";
    }
  );

  foundationArtifacts = mkWorkspaceArtifactLayer {
    name = "foundation-artifacts";
    members = foundationMembers;
    cargoArtifacts = brooddArtifacts;
  };

  serviceArtifacts = mkWorkspaceArtifactLayer {
    name = "service-artifacts";
    members = serviceMembers;
    cargoArtifacts = foundationArtifacts;
  };

  commandArtifacts = mkWorkspaceArtifactLayer {
    name = "command-artifacts";
    members = commandMembers;
    cargoArtifacts = serviceArtifacts;
  };

  brooddMembers = commandMembers ++ [
    "crates/brood-remote"
    "clients/brood-client"
    "crates/broodd"
  ];
  brooddLibArtifacts = mkWorkspaceArtifactLayer {
    name = "broodd-lib-artifacts";
    buildArgs = "--locked -p broodd --lib";
    members = brooddMembers;
    cargoArtifacts = commandArtifacts;
  };

  # Only the final executable is linked. Compatibility names are symlinks,
  # not separately compiled binaries.
  aliases = {
    brood-client = "client";
    brood-sessiond = "session";
    brood-remote = "remote";
    brood-capsuled = "capsule";
    brood-computer-use-mcp = "computer-use-mcp";
    brood-runner-exec = "runner-exec";
    brood-runner-nix-develop = "runner-nix-develop";
    bmux = "bmux";
    brood-profile = "profile";
  };
  serverAliases = builtins.removeAttrs aliases [ "brood-client" ];
  mkUnifiedPackage =
    name: features: artifacts:
    craneLib.buildPackage (
      craneBaseArgs
      // sourceProvenance
      // {
        pname = name;
        src = mkWorkspaceSrc name brooddMembers;
        cargoArtifacts = artifacts;
        cargoBuildExtraArgs = "-p broodd --bin brood ${features}";
        doCheck = false;
        doNotPostBuildInstallCargoBinaries = true;
        installPhaseCommand = ''
          install -Dm755 target/release/brood "$out/bin/brood"
          ${lib.concatMapStringsSep "\n" (alias: ''
            ln -s brood "$out/bin/${alias}"
          '') (builtins.attrNames (if features == "" then aliases else serverAliases))}
        '';
      }
    );
  broodd = mkUnifiedPackage "brood-full" "" brooddLibArtifacts;
  serverFeatures = "--no-default-features --features server";
  serverArtifacts = craneLib.buildDepsOnly (
    craneBaseArgs
    // {
      cargoBuildExtraArgs = "-p broodd --bin brood ${serverFeatures}";
      cargoCheckExtraArgs = "-p broodd --bin brood ${serverFeatures}";
    }
  );
  server = mkUnifiedPackage "brood-server" serverFeatures serverArtifacts;

  shellCheck = pkgs.runCommand "brood-shellcheck" { nativeBuildInputs = [ pkgs.shellcheck ]; } ''
    cp -R ${src}/. source
    chmod -R u+w source
    cd source

    mapfile -d "" shell_files < <(
      find . \
        \( -path "./.git" -o -path "./.git/*" \
        -o -path "./.brood" -o -path "./.brood/*" \
        -o -path "*/.brood" -o -path "*/.brood/*" \
        -o -path "./target" -o -path "./target/*" \
        -o -path "./reference" -o -path "./reference/*" \
        -o -path "./result" -o -path "./result/*" \) -prune \
        -o \( -name "*.sh" -o -name "*.bash" \) -type f -print0
    )

    if ((''${#shell_files[@]} > 0)); then
      echo "lint=shell command=shellcheck files=''${#shell_files[@]}"
      shellcheck -e SC2016 -e SC2034 "''${shell_files[@]}"
    fi

    mkdir -p "$out"
    touch "$out/passed"
  '';

  wasmModules = pkgs.stdenvNoCC.mkDerivation {
    name = "brood-wasm-modules";
    src = guestSrc;
    cargoVendorDir = wasmCargoVendorDir;
    nativeBuildInputs = [
      craneLib.configureCargoVendoredDepsHook
      pkgs.gnumake
      packageManifestTool
      profiles.rustToolchain
    ]
    ++ profiles.wasmTools;

    preConfigure = ''
      export CARGO_HOME="$TMPDIR/cargo-home"
      export HOME="$TMPDIR/home"
      mkdir -p "$CARGO_HOME" "$HOME"
    '';

    buildPhase = ''
      runHook preBuild

      export CARGO_NET_OFFLINE=true
      export RQUICKJS_SYS_NO_WASI_SDK=1
      export CC_wasm32_wasip1=${profiles.wasi32Cc}/bin/wasm32-unknown-wasi-cc
      export AR_wasm32_wasip1=${profiles.wasi32Binutils}/bin/wasm32-unknown-wasi-ar

      jobs="''${NIX_BUILD_CORES:-1}"
      if [[ "$jobs" == "0" ]]; then
        jobs=1
      fi
      bash tools/brood-build.sh --wasm-only --jobs "$jobs"

      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall

      mkdir -p "$out"
      while IFS= read -r wasm; do
        # The raw core module is an intermediate input to the Component Model
        # encoder. Only its validated component is a runnable plugin artifact.
        if [[ "$wasm" == "examples/wasm-component-actor/target/wasm32-unknown-unknown/release/brood_component_actor_example.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "examples/wasm-facet-probe/target/wasm32-unknown-unknown/release/brood_component_facet_probe.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "examples/wasm-paired-facet-probe/target/wasm32-unknown-unknown/release/brood_paired_facet_probe.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "examples/wasm-plugin-graphics/target/wasm32-unknown-unknown/release/brood_plugin_graphics_example.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "examples/wasm-init-actor/target/wasm32-unknown-unknown/release/brood_wasm_init_actor_example.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/component-workspace-watch/target/wasm32-unknown-unknown/release/brood_component_workspace_watch_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/component-runtime-io/target/wasm32-unknown-unknown/release/brood_component_runtime_io_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_session_switcher_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/neovim-client/target/wasm32-unknown-unknown/release/brood_neovim_client_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/neovim-files/target/wasm32-unknown-unknown/release/brood_neovim_files_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/worktree-manager/target/wasm32-unknown-unknown/release/brood_worktree_manager_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/coding-agent/target/wasm32-unknown-unknown/release/brood_coding_agent_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/picker/target/wasm32-unknown-unknown/release/brood_picker_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/keybindings/target/wasm32-unknown-unknown/release/brood_keybindings_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/diff-viewer/target/wasm32-unknown-unknown/release/brood_diff_viewer_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/inbox/target/wasm32-unknown-unknown/release/brood_inbox_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/status-bar/target/wasm32-unknown-unknown/release/brood_status_bar_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/terminal-session/target/wasm32-unknown-unknown/release/brood_terminal_session_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/terminal-ui/target/wasm32-unknown-unknown/release/brood_terminal_ui_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/resize-proof/target/wasm32-unknown-unknown/release/brood_resize_proof_plugin.wasm" ]]; then
          continue
        fi
        install -D "$wasm" "$out/$wasm"
      done < <(
        {
          find examples -path '*/target/wasm32-*/release/*.wasm' ! -path '*/release/deps/*' -print
          find plugins -path '*/target/wasm32-*/release/*.wasm' ! -path '*/release/deps/*' -print
          find runtimes -path '*/target/wasm32-*/release/*.wasm' ! -path '*/release/deps/*' -print
        } | sort
      )

      brood-package-manifest assemble \
        packages/default-client.brood-package.toml \
        "$PWD" \
        "$out/packages/org.brood.default-client"

      runHook postInstall
    '';
  };

  runtimeFiles =
    pkgs.runCommand "brood-runtime-files" { nativeBuildInputs = [ pkgs.makeWrapper ]; }
      ''
        mkdir -p "$out"

        install -D ${src}/brood.toml "$out/brood.toml"

        default_client_package="packages/org.brood.default-client"
        if [[ -d ${wasmModules}/$default_client_package ]]; then
          cp -R ${wasmModules}/packages "$out/packages"
          chmod -R u+w "$out/packages"
        else
          echo "ERR packaged WASM modules did not include $default_client_package" >&2
          exit 2
        fi
        client_root="$(<"$out/$default_client_package/brood-package-root")"
        substituteInPlace "$out/brood.toml" \
          --replace-fail \
          'client = "plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_session_switcher_plugin.component.wasm"' \
          "client = \"$default_client_package/$client_root\""

        mkdir -p "$out/apps"
        cp -R ${src}/apps/. "$out/apps/"
        chmod -R u+w "$out/apps"
        rm -rf "$out/apps/__pycache__"

        if [[ -d ${wasmModules}/examples ]]; then
          cp -R ${wasmModules}/examples "$out/examples"
        else
          echo "ERR packaged WASM modules did not include an examples directory" >&2
          exit 2
        fi

        if [[ -d ${wasmModules}/plugins ]]; then
          cp -R ${wasmModules}/plugins "$out/plugins"
          chmod u+w "$out/plugins"
        else
          echo "ERR packaged WASM modules did not include a plugins directory" >&2
          exit 2
        fi

        install -D \
          ${wasmModules}/runtimes/computer-use-js/target/wasm32-wasip1/release/brood_computer_use_js_runtime.wasm \
          "$out/runtimes/computer-use-js.wasm"

        agent_runtime="$out/share/brood-agent"
        mkdir -p "$agent_runtime/src"
        install -m 0644 ${src}/agents/brood-agent/src/*.mjs "$agent_runtime/src/"
        install -m 0644 ${src}/agents/brood-agent/agent.example.json "$agent_runtime/agent.example.json"
        install -m 0644 ${src}/agents/brood-agent/LICENSE "$agent_runtime/LICENSE"
        install -m 0644 ${src}/agents/brood-agent/THIRD_PARTY_NOTICES.md "$agent_runtime/THIRD_PARTY_NOTICES.md"
        makeWrapper ${pkgs.nodejs}/bin/node "$out/bin/brood-agent" \
          --prefix PATH : "${
            lib.makeBinPath [
              pkgs.bash
              pkgs.util-linux
            ]
          }" \
          --add-flags "$agent_runtime/src/cli.mjs"

        # The editor runtime belongs to the local client package only. Keep it
        # outside wasmModules so consumers of a single backend component (notably
        # the SSH bundle) do not inherit Node or the client-side Neovim runtime.
        session_switcher_root="$out/$default_client_package"
        runtime_dir="$session_switcher_root/share/brood/neovim-wasm"
        mkdir -p "$runtime_dir"
        cp -R ${neovimWasm}/share/brood/neovim-wasm/. "$runtime_dir/"

      '';

  brood = pkgs.symlinkJoin {
    name = "brood";
    paths = [
      broodd
      runtimeFiles
    ];
    nativeBuildInputs = [ pkgs.makeWrapper ];
    postBuild = ''
      for program in brood brood-agent; do
        wrapProgram "$out/bin/$program" \
          --prefix PATH : "$out/bin:${appToolPath}" \
          --prefix LD_LIBRARY_PATH : "${nativeLibraryPath}" \
          --prefix GI_TYPELIB_PATH : "${gtkTypelibPath}" \
          --prefix XDG_DATA_DIRS : "${gtkDataDirs}" \
          --set BROOD_SSH_BACKEND "${fullSshBackend}" \
          --set BROOD_SSH_BACKEND_SYSTEM "${pkgs.stdenv.hostPlatform.system}" \
          --set BROOD_CONTAINER_BACKEND "${fullContainerBackend}" \
          --set BROOD_CONTAINER_BACKEND_SYSTEM "${pkgs.stdenv.hostPlatform.system}" \
          --set BROOD_COMPUTER_USE_JS_RUNTIME "$out/runtimes/computer-use-js.wasm" \
          --set BROOD_PODMAN_REAL "${pkgs.podman}/bin/podman" \
          --run 'export GSETTINGS_SCHEMA_DIR="''${GSETTINGS_SCHEMA_DIR:-${gsettingsSchemaDir}}"' \
          --run 'export BROOD_CONFIG="''${BROOD_CONFIG:-'"$out"'/brood.toml}"'
      done
      ${lib.concatStringsSep "\n" (
        lib.mapAttrsToList (alias: role: ''
          rm "$out/bin/${alias}"
          makeWrapper "$out/bin/brood" "$out/bin/${alias}" --add-flags ${role}
        '') aliases
      )}
    '';
  };

  sshBackend = mkSshBackend server;
  fullSshBackend = mkSshBackend broodd;
  mkSshBackend =
    native:
    let
      dynamicLinker =
        if pkgs.stdenv.hostPlatform.isx86_64 then
          "/lib64/ld-linux-x86-64.so.2"
        else if pkgs.stdenv.hostPlatform.isAarch64 then
          "/lib/ld-linux-aarch64.so.1"
        else
          throw "Brood SSH deployment does not support ${pkgs.stdenv.hostPlatform.system}";
    in
    pkgs.runCommand "brood-ssh-backend"
      {
        nativeBuildInputs = [ pkgs.patchelf ];
      }
      ''
        mkdir -p \
          "$out/bin" \
          "$out/lib" \
          "$out/libexec" \
          "$out/plugins/worktree-manager/target/wasm32-unknown-unknown/release"
        for program in brood; do
          cp ${native}/bin/"$program" "$out/bin/$program"
          chmod u+w "$out/bin/$program"
          patchelf \
            --set-interpreter ${lib.escapeShellArg dynamicLinker} \
            --remove-rpath \
            "$out/bin/$program"
        done

        for alias in brood-capsuled brood-remote brood-sessiond brood-runner-exec; do
          ln -s brood "$out/bin/$alias"
        done
        install -m 0755 ${../tools/brood-ssh-backend-session.sh} \
          "$out/libexec/brood-ssh-backend-session"
        cat >"$out/bin/brood-ssh-backend-session" <<'EOF'
        #!/bin/sh
        set -eu
        case "$0" in
          */*) script_dir=''${0%/*} ;;
          *) script_dir=. ;;
        esac
        root="$(CDPATH= cd -- "$script_dir/.." && pwd)"
        export PATH="$root/bin:''${PATH:-/usr/bin:/bin}"
        exec "$root/libexec/brood-ssh-backend-session" "$@"
        EOF
        chmod 0755 "$out/bin/brood-ssh-backend-session"

        install -m 0555 \
          ${wasmModules}/plugins/worktree-manager/target/wasm32-unknown-unknown/release/brood_worktree_manager_plugin.component.wasm \
          "$out/plugins/worktree-manager/target/wasm32-unknown-unknown/release/brood_worktree_manager_plugin.component.wasm"
        printf '%s\n' \
          '[init]' \
          'backend = "plugins/worktree-manager/target/wasm32-unknown-unknown/release/brood_worktree_manager_plugin.component.wasm"' \
          > "$out/brood.toml"
      '';

  containerBackend = mkContainerBackend server;
  fullContainerBackend = mkContainerBackend broodd;
  mkContainerBackend =
    native:
    let
      dynamicLinker =
        if pkgs.stdenv.hostPlatform.isx86_64 then
          "ld-linux-x86-64.so.2"
        else if pkgs.stdenv.hostPlatform.isAarch64 then
          "ld-linux-aarch64.so.1"
        else
          throw "Brood container deployment does not support ${pkgs.stdenv.hostPlatform.system}";
      dependencyPath = lib.makeLibraryPath (
        profiles.nativeLibs
        ++ [
          pkgs.dbus
          pkgs.expat
          pkgs.glibc
          pkgs.stdenv.cc.cc.lib
        ]
      );
    in
    pkgs.runCommand "brood-container-backend"
      {
        nativeBuildInputs = [
          pkgs.binutils
          pkgs.findutils
          pkgs.gawk
          pkgs.patchelf
        ];
      }
      ''
        mkdir -p \
          "$out/bin" \
          "$out/lib" \
          "$out/plugins/worktree-manager/target/wasm32-unknown-unknown/release" \
          "$out/share/dbus-1" \
          "$out/share/glvnd/egl_vendor.d" \
          "$out/share/X11"

        for program in brood; do
          cp ${native}/bin/"$program" "$out/bin/$program"
          chmod u+w "$out/bin/$program"
        done
        for alias in brood-capsuled brood-remote brood-sessiond brood-runner-exec; do
          ln -s brood "$out/bin/$alias"
        done
        cp ${pkgs.dbus}/bin/dbus-daemon "$out/bin/dbus-daemon"
        chmod u+w "$out/bin/dbus-daemon"
        cp ${pkgs.dbus}/share/dbus-1/session.conf "$out/share/dbus-1/session.conf"
        cp -RL ${pkgs.xkeyboard_config}/etc/X11/xkb "$out/share/X11/xkb"

        export LD_LIBRARY_PATH=${dependencyPath}
        for executable in "$out"/bin/*; do
          while IFS= read -r library; do
            [[ -n "$library" && -f "$library" ]] || continue
            cp -Lf "$library" "$out/lib/$(basename "$library")"
          done < <(
            ldd "$executable" \
              | awk '/=> \// { print $3 } /^\// { print $1 }'
          )
        done
        cp -Lf ${pkgs.glibc}/lib/${dynamicLinker} "$out/lib/${dynamicLinker}"
        cp -Lf ${pkgs.wayland}/lib/libwayland*.so* "$out/lib/"
        cp -Lf ${pkgs.libffi}/lib/libffi.so* "$out/lib/"
        cp -R ${pkgs.libglvnd}/lib/. "$out/lib/"
        cp -R ${pkgs.mesa}/lib/. "$out/lib/"
        cat >"$out/share/glvnd/egl_vendor.d/50_mesa.json" <<'EOF'
        {
          "file_format_version": "1.0.0",
          "ICD": { "library_path": "/brood/lib/libEGL_mesa.so.0" }
        }
        EOF
        while IFS= read -r library; do
          while IFS= read -r dependency; do
            [[ -n "$dependency" && -f "$dependency" ]] || continue
            cp -Lf "$dependency" "$out/lib/$(basename "$dependency")"
          done < <(
            ldd "$library" \
              | awk '/=> \// { print $3 } /^\// { print $1 }'
          )
        done < <(find "$out/lib" -type f -name '*.so*' -print)
        cp -Lf ${pkgs.glibc}/lib/${dynamicLinker} "$out/lib/${dynamicLinker}"

        for executable in "$out"/bin/*; do
          patchelf \
            --set-interpreter "/brood/lib/${dynamicLinker}" \
            --set-rpath /brood/lib \
            "$executable"
        done
        find "$out/lib" -type f -name '*.so*' ! -name 'ld-linux-*' -print0 \
          | while IFS= read -r -d "" library; do
              if patchelf --print-rpath "$library" >/dev/null 2>&1; then
                chmod u+w "$library"
                patchelf --set-rpath /brood/lib "$library"
              fi
            done
        install -m 0555 \
          ${wasmModules}/plugins/worktree-manager/target/wasm32-unknown-unknown/release/brood_worktree_manager_plugin.component.wasm \
          "$out/plugins/worktree-manager/target/wasm32-unknown-unknown/release/brood_worktree_manager_plugin.component.wasm"
        printf '%s\n' \
          '[init]' \
          'backend = "plugins/worktree-manager/target/wasm32-unknown-unknown/release/brood_worktree_manager_plugin.component.wasm"' \
          > "$out/brood.toml"
      '';

  podmanWithBackend = pkgs.writeShellApplication {
    name = "podman";
    runtimeInputs = [ brood ];
    text = ''
      export BROOD_PODMAN_REAL=${pkgs.podman}/bin/podman
      export BROOD_CONTAINER_BACKEND=${containerBackend}
      exec brood-remote podman "$@"
    '';
  };

  desktopActions = pkgs.writeShellApplication {
    name = "brood-desktop";
    runtimeInputs = [ pkgs.coreutils ];
    text = ''
      set -euo pipefail

      action="''${1:-show}"
      brood_bin="${brood}/bin/brood"

      export LD_LIBRARY_PATH="${nativeLibraryPath}:''${LD_LIBRARY_PATH:-}"
      export GI_TYPELIB_PATH="${gtkTypelibPath}:''${GI_TYPELIB_PATH:-}"
      export XDG_DATA_DIRS="${gtkDataDirs}:''${XDG_DATA_DIRS:-}"
      export GSETTINGS_SCHEMA_DIR="${gsettingsSchemaDir}"
      export BROOD_CONFIG="''${BROOD_CONFIG:-${brood}/brood.toml}"

      case "$action" in
        show)
          exec "$brood_bin" show --last-focused
          ;;
        new-session)
          response="$("$brood_bin" ctl surface-session dir-new "$PWD" default)"
          case "$response" in
            "OK surface-session active="*)
              session_id="''${response#* active=}"
              session_id="''${session_id%% *}"
              exec "$brood_bin" show --id "$session_id" --set-default
              ;;
            *)
              printf '%s\n' "$response" >&2
              exit 1
              ;;
          esac
          ;;
        picker)
          "$brood_bin" show >/dev/null
          exec "${profiles.sessionSwitcherPython}/bin/python3" \
            "${runtimeFiles}/apps/brood-session-switcher.py" \
            --width 360 \
            --close-after-activate
          ;;
        quit)
          exec "$brood_bin" ctl quit
          ;;
        *)
          echo "usage: brood-desktop [show|new-session|picker|quit]" >&2
          exit 2
          ;;
      esac
    '';
  };

  desktopIntegration = pkgs.runCommand "brood-desktop-integration" { } ''
    install -D -m 0644 ${../assets/icons/hicolor/512x512/apps/brood.png} \
      "$out/share/icons/hicolor/512x512/apps/brood.png"
    install -D -m 0644 ${../assets/icons/hicolor/512x512/apps/brood.png} \
      "$out/share/pixmaps/brood.png"

    install -D -m 0644 /dev/stdin "$out/share/applications/brood.desktop" <<EOF
    [Desktop Entry]
    Type=Application
    Name=Brood
    Comment=Show the Brood workspace
    Exec=${desktopActions}/bin/brood-desktop show
    Icon=brood
    Terminal=false
    Categories=Development;
    StartupNotify=true
    Actions=NewSession;OpenPicker;Quit;

    [Desktop Action NewSession]
    Name=New Session
    Exec=${desktopActions}/bin/brood-desktop new-session

    [Desktop Action OpenPicker]
    Name=Open Picker
    Exec=${desktopActions}/bin/brood-desktop picker

    [Desktop Action Quit]
    Name=Quit
    Exec=${desktopActions}/bin/brood-desktop quit
    EOF

    install -D -m 0644 /dev/stdin "$out/share/applications/org.brood.SessionSwitcher.desktop" <<EOF
    [Desktop Entry]
    Type=Application
    Name=Brood Session Picker
    Comment=Pick a Brood session
    Exec=${desktopActions}/bin/brood-desktop picker
    Icon=brood
    Terminal=false
    Categories=Development;
    StartupNotify=true
    NoDisplay=true
    EOF
  '';

  broodWithDesktop = pkgs.symlinkJoin {
    name = "brood";
    paths = [
      brood
      desktopActions
      desktopIntegration
    ];
  };

  devEnv = pkgs.buildEnv {
    name = "brood-dev-env";
    paths = profiles.devTools ++ profiles.nativeLibs ++ profiles.gtkRuntime;
    ignoreCollisions = true;
  };

  devEnvFull = pkgs.buildEnv {
    name = "brood-dev-env-full";
    paths =
      profiles.devTools
      ++ profiles.fullDevExtras
      ++ profiles.smokeTools
      ++ profiles.nativeLibs
      ++ profiles.gtkRuntime;
    ignoreCollisions = true;
  };

  ciTools = pkgs.buildEnv {
    name = "brood-ci-tools";
    paths = [
      pkgs.act
      pkgs.actionlint
    ];
  };

  localCi = pkgs.writeShellApplication {
    name = "brood-ci";
    runtimeInputs = [
      pkgs.actionlint
      pkgs.coreutils
      pkgs.git
      pkgs.nix
    ];
    text = ''
      set -euo pipefail

      ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
      cd "$ROOT"

      echo "ci=workflow command=actionlint"
      actionlint .github/workflows/ci.yml

      tmpdir="$(mktemp -d)"
      cleanup() {
        rm -rf "$tmpdir"
      }
      trap cleanup EXIT

      system="$(nix eval --raw --impure --expr builtins.currentSystem)"

      run_job() {
        local name="$1"
        shift

        {
          echo "ci=job name=$name status=started"
          "$@"
          echo "ci=job name=$name status=passed"
        } >"$tmpdir/$name.log" 2>&1
      }

      run_job lint nix build --print-build-logs ".#checks.$system.lint" &
      lint_pid="$!"
      run_job build nix build --print-build-logs .#default .#dev-workflow .#wasm-examples &
      build_pid="$!"

      status=0

      if ! wait "$lint_pid"; then
        status=1
        echo "ci=job name=lint status=failed"
      fi
      cat "$tmpdir/lint.log"

      if ! wait "$build_pid"; then
        status=1
        echo "ci=job name=build status=failed"
      fi
      cat "$tmpdir/build.log"

      if ((status != 0)); then
        exit "$status"
      fi

      echo "ci=all status=passed"
    '';
  };

  cacheCi = pkgs.writeShellApplication {
    name = "brood-cache-ci";
    runtimeInputs = [
      pkgs.cachix
      pkgs.coreutils
      pkgs.git
      pkgs.nix
    ];
    text = ''
      set -euo pipefail

      ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
      cd "$ROOT"

      cache="''${CACHIX_CACHE:-}"
      if (($# > 0)); then
        cache="$1"
        shift
      fi

      if (($# > 0)) || [[ -z "$cache" ]]; then
        echo "usage: CACHIX_CACHE=<cache-name> nix run .#cache-ci [cachix-cache-name]" >&2
        exit 2
      fi

      system="$(nix eval --raw --impure --expr builtins.currentSystem)"
      attrs=(
        .#default
        .#ci
        ".#checks.$system.lint"
        .#cargoArtifacts
        .#clippyCheck
        .#shellCheck
        .#brooddArtifacts
      )

      echo "cache-ci=build attrs=''${attrs[*]}"
      mapfile -t output_paths < <(nix build --no-link --print-out-paths "''${attrs[@]}")

      echo "cache-ci=push cache=$cache paths=''${#output_paths[@]}"
      printf "cache-ci=path %s\n" "''${output_paths[@]}"
      cachix push "$cache" "''${output_paths[@]}"
      echo "cache-ci=status passed"
    '';
  };

  primeCargoTarget = pkgs.writeShellApplication {
    name = "brood-prime-cargo-target";
    runtimeInputs = [
      pkgs.coreutils
      pkgs.git
      pkgs.gnutar
      pkgs.nix
      pkgs.zstd
    ];
    text = ''
      set -euo pipefail

      ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
      cd "$ROOT"

      artifact="$(nix build --no-link --print-out-paths .#cargoArtifacts | tail -n1)"
      archive="$artifact/target.tar.zst"

      if [[ ! -f "$archive" ]]; then
        echo "prime-cargo-target=error missing-archive path=$archive" >&2
        exit 1
      fi

      mkdir -p target
      echo "prime-cargo-target=extract archive=$archive destination=$ROOT/target"
      tar --zstd -xf "$archive" -C target
      echo "prime-cargo-target=status passed"
    '';
  };

  formatter = pkgs.writeShellApplication {
    name = "brood-format";
    runtimeInputs = [
      pkgs.git
      pkgs.nixfmt
      pkgs.shfmt
      profiles.rustToolchainWithTools
    ];
    text = ''
      set -euo pipefail

      ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
      cd "$ROOT"

      paths=("$@")
      if ((''${#paths[@]} == 0)); then
        paths=(.)
      fi

      source_files=()
      while IFS= read -r -d "" file; do
        source_files+=("$file")
      done < <(git ls-files -z --cached --others --exclude-standard -- "''${paths[@]}")

      nix_files=()
      shell_files=()
      for file in "''${source_files[@]}"; do
        case "$file" in
          .brood/* | */.brood/* | reference/*)
            continue
            ;;
          *.nix)
            nix_files+=("$file")
            ;;
          *.sh | *.bash)
            shell_files+=("$file")
            ;;
        esac
      done

      echo "format=rust command=cargo-fmt"
      cargo fmt --all

      if ((''${#nix_files[@]} > 0)); then
        echo "format=nix command=nixfmt files=''${#nix_files[@]}"
        nixfmt "''${nix_files[@]}"
      fi

      if ((''${#shell_files[@]} > 0)); then
        echo "format=shell command=shfmt files=''${#shell_files[@]}"
        shfmt -w -i 2 -ci -sr "''${shell_files[@]}"
      fi
    '';
  };

  linter = pkgs.writeShellApplication {
    name = "brood-lint";
    runtimeInputs = [
      pkgs.findutils
      pkgs.git
      pkgs.pkg-config
      pkgs.shellcheck
      pkgs.stdenv.cc
      profiles.rustToolchainWithTools
    ];
    text = ''
      set -euo pipefail

      if ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"; then
        cd "$ROOT"
        in_git_repo=1
      else
        in_git_repo=0
      fi

      export LD_LIBRARY_PATH="${nativeLibraryPath}:''${LD_LIBRARY_PATH:-}"
      export PKG_CONFIG_PATH="${nativePkgConfigPath}:''${PKG_CONFIG_PATH:-}"

      paths=("$@")
      if ((''${#paths[@]} == 0)); then
        paths=(.)
      fi

      source_files=()
      if [[ "$in_git_repo" == "1" ]]; then
        while IFS= read -r -d "" file; do
          source_files+=("$file")
        done < <(git ls-files -z --cached --others --exclude-standard -- "''${paths[@]}")
      else
        while IFS= read -r -d "" file; do
          source_files+=("''${file#./}")
        done < <(
          find "''${paths[@]}" \
            \( -path "./.git" -o -path "./.git/*" \
            -o -path "./.brood" -o -path "./.brood/*" \
            -o -path "./target" -o -path "./target/*" \
            -o -path "./reference" -o -path "./reference/*" \
            -o -path "./result" -o -path "./result/*" \) -prune \
            -o -type f -print0
        )
      fi

      shell_files=()
      for file in "''${source_files[@]}"; do
        case "$file" in
          .brood/* | */.brood/* | reference/*)
            continue
            ;;
          *.sh | *.bash)
            shell_files+=("$file")
            ;;
        esac
      done

      echo "lint=rust command=cargo-clippy"
      cargo clippy --workspace --all-targets --locked

      echo "lint=rust command=cargo-check"
      cargo check --workspace --all-targets --locked

      if ((''${#shell_files[@]} > 0)); then
        echo "lint=shell command=shellcheck files=''${#shell_files[@]}"
        shellcheck -e SC2016 -e SC2034 "''${shell_files[@]}"
      fi
    '';
  };

  agentCheck =
    pkgs.runCommand "brood-agent-check"
      {
        nativeBuildInputs = [
          pkgs.nodejs
          pkgs.bash
          pkgs.coreutils
          pkgs.util-linux
        ];
      }
      ''
        cp -R ${src}/agents/brood-agent "$TMPDIR/brood-agent"
        chmod -R u+w "$TMPDIR/brood-agent"
        cd "$TMPDIR/brood-agent"
        node --test test/*.test.mjs
        mkdir -p "$out"
        touch "$out/passed"
      '';

  lintCheck = pkgs.runCommand "brood-lint-check" { } ''
    echo "lint=rust command=cargo-check artifacts=${cargoArtifacts}"
    echo "lint=rust command=cargo-clippy check=${clippyCheck}"
    echo "lint=shell check=${shellCheck}"
    echo "test=agent check=${agentCheck}"

    mkdir -p "$out"
    touch "$out/passed"
  '';

  mkDevWorkflow = import ./scripts/dev-workflow.nix;

  mkWorkflow =
    {
      name,
      path,
      exportDevVars,
    }:
    mkDevWorkflow {
      inherit
        exportDevVars
        gsettingsSchemaDir
        gtkDataDirs
        gtkTypelibPath
        lib
        broodd
        name
        nativeLibraryPath
        nativePkgConfigPath
        path
        pkgs
        runtimeFiles
        ;
      rustToolchain = profiles.rustToolchain;
      sessionSwitcherPython = profiles.sessionSwitcherPython;
    };

  devWorkflow = mkWorkflow {
    name = "brood-dev-workflow";
    path = appToolPath;
    exportDevVars = false;
  };

  devWorkflowFull = mkWorkflow {
    name = "brood-dev-workflow-full";
    path = fullDevToolPath;
    exportDevVars = true;
  };

  devWorkflowWithRuntime = pkgs.symlinkJoin {
    name = "brood-dev-workflow";
    paths = [
      devWorkflow
      runtimeFiles
      desktopActions
      desktopIntegration
    ];
  };

  devWorkflowFullWithRuntime = pkgs.symlinkJoin {
    name = "brood-dev-workflow-full";
    paths = [
      devWorkflowFull
      runtimeFiles
      desktopActions
      desktopIntegration
    ];
  };
in
{
  default = broodWithDesktop;
  inherit server;
  full = brood;
  "neovim-wasm" = neovimWasm;
  "dev-workflow" = devWorkflowWithRuntime;
  "dev-workflow-full" = devWorkflowFullWithRuntime;
  "dev-env" = devEnv;
  "dev-env-full" = devEnvFull;
  ci = localCi;
  "cache-ci" = cacheCi;
  "ci-tools" = ciTools;
  lint = linter;
  "prime-cargo-target" = primeCargoTarget;
  "software-e2e-capture" = softwareE2eCapture;
  "ssh-backend" = sshBackend;
  "container-backend" = containerBackend;
  "podman" = podmanWithBackend;
  "runtime-files" = runtimeFiles;
  "package-manifest" = packageManifestTool;
  "wasm-modules" = wasmModules;
  "wasm-examples" = wasmModules;
  inherit formatter linter;

  inherit
    cargoArtifacts
    clippyCheck
    devEnv
    devEnvFull
    cacheCi
    ciTools
    primeCargoTarget
    localCi
    neovimWasm
    packageManifestTool
    devWorkflow
    devWorkflowFullWithRuntime
    devWorkflowWithRuntime
    devWorkflowFull
    desktopActions
    desktopIntegration
    broodWithDesktop
    brood
    broodd
    brooddArtifacts
    lintCheck
    agentCheck
    runtimeFiles
    shellCheck
    softwareE2eCapture
    sshBackend
    containerBackend
    podmanWithBackend
    wasmModules
    ;
}
