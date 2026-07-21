{ pkgs, crane }:

let
  lib = pkgs.lib;
  profiles = import ./profiles.nix { inherit pkgs; };

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
      in
      base != "target";
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
      craneLib.filterCargoSources path type || lib.hasSuffix ".wit" path || lib.hasSuffix ".witx" path;
  };

  # The guest examples are independent Cargo workspaces. Vendor their lock
  # files explicitly so the derivation remains network-free in the Nix
  # sandbox. The conditional entries keep older source snapshots evaluable
  # before the new examples exist in the tree.
  wasmCargoVendorDir = craneLib.vendorMultipleCargoDeps {
    cargoLockList = builtins.filter builtins.pathExists [
      "${src}/examples/wasm-component-actor/Cargo.lock"
      "${src}/examples/wasm-plugin-graphics/Cargo.lock"
      "${src}/examples/wasm-preview1-worker/Cargo.lock"
      "${src}/plugins/component-workspace-watch/Cargo.lock"
      "${src}/plugins/component-runtime-io/Cargo.lock"
      "${src}/plugins/workspace-file-log/Cargo.lock"
    ];
  };

  craneBaseArgs = {
    pname = "manifold";
    version = "0.1.0";
    src = craneSrc;
    strictDeps = true;
    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = profiles.nativeLibs;
  };

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

  cargoTestCheck = craneLib.cargoTest (
    craneBaseArgs
    // {
      inherit cargoArtifacts;
      # Coordination and worker lifecycle invariants live in manifold-wasm;
      # running only the daemon package silently skipped those unit tests.
      cargoExtraArgs = "--locked --workspace";
      env = {
        CARGO_PROFILE = "";
        LD_LIBRARY_PATH = nativeLibraryPath;
      };
      preCheck = ''
        export XDG_RUNTIME_DIR="$TMPDIR/xdg-runtime"
        mkdir -p "$XDG_RUNTIME_DIR"
        chmod 700 "$XDG_RUNTIME_DIR"
      '';
    }
  );

  softwareE2eCapture = pkgs.writeShellApplication {
    name = "manifold-software-e2e-capture";
    runtimeInputs = [
      pkgs.bash
      pkgs.coreutils
      pkgs.dbus
      pkgs.gnused
      pkgs.python3
      profiles.westonWithSimpleClients
    ];
    text = ''
      export DBUS_SESSION_BUS_CONFIG=${pkgs.dbus}/share/dbus-1/session.conf
      export __EGL_VENDOR_LIBRARY_FILENAMES=${pkgs.mesa}/share/glvnd/egl_vendor.d/50_mesa.json
      export LIBGL_DRIVERS_PATH=${pkgs.mesa}/lib/dri
      export GBM_BACKENDS_PATH=${pkgs.mesa}/lib/gbm
      exec bash ${src}/tools/software-e2e-capture.sh \
        --manifold-bin ${manifold}/bin/manifold \
        "$@"
    '';
  };

  softwareE2eCheck =
    pkgs.runCommand "manifold-software-e2e-check" { nativeBuildInputs = [ softwareE2eCapture ]; }
      ''
        run_dir="$TMPDIR/software-e2e"
        mkdir -p "$run_dir"

        manifold-software-e2e-capture --out-dir "$run_dir"

        mkdir -p "$out"
        cp -R "$run_dir"/inner-dump "$out/inner-dump"
        cp -R "$run_dir"/outer-dump "$out/outer-dump"
        cp "$run_dir"/*.json "$out/"
        cp "$run_dir"/*.log "$out/"
        cp "$run_dir"/summary.txt "$out/"
        touch "$out/passed"
      '';

  softwareE2eArtifacts =
    pkgs.runCommand "manifold-software-e2e-artifacts"
      {
        nativeBuildInputs = [
          pkgs.python3
          softwareE2eCapture
        ];
      }
      ''
        run_dir="$TMPDIR/software-e2e"
        mkdir -p "$run_dir"

        manifold-software-e2e-capture --out-dir "$run_dir"
        python3 ${src}/tools/detect-surface-markers.py \
          "$run_dir/inner-dump/composed.png" \
          "$run_dir/inner-dump/state.json" \
          "$run_dir/detected-layout.json"

        mkdir -p "$out"
        cp "$run_dir/inner-dump/composed.png" "$out/frame.png"
        cp "$run_dir/inner-dump/state.json" "$out/layout-state.json"
        cp "$run_dir/detected-layout.json" "$out/detected-layout.json"
        cp "$run_dir/summary.txt" "$out/summary.txt"
        cat > "$out/README.txt" <<EOF
        frame.png              inner composed frame used for pixel detection
        layout-state.json      Manifold debug-dump layout/window state
        detected-layout.json   marker IDs and positions decoded from frame.png, matched to layout-state.json
        summary.txt            capture command summary
        EOF
      '';

  testCheck = pkgs.runCommand "manifold-test-check" { } ''
    echo "test=rust cargo=${cargoTestCheck}"
    echo "test=wasm-io check=${wasmIoCheck}"
    echo "test=wasm-io-rube check=${wasmIoRubeQuickCheck}"
    echo "test=software-e2e check=${softwareE2eCheck}"
    mkdir -p "$out"
    ln -s ${wasmIoCheck} "$out/wasm-io"
    ln -s ${wasmIoRubeQuickCheck} "$out/wasm-io-rube"
    ln -s ${softwareE2eCheck} "$out/software-e2e"
    touch "$out/passed"
  '';

  manifolddArtifacts = craneLib.buildDepsOnly (
    craneBaseArgs
    // {
      cargoBuildExtraArgs = "-p manifoldd -p manifold-client --bins";
      cargoCheckExtraArgs = "-p manifoldd -p manifold-client --all-targets";
      cargoTestExtraArgs = "-p manifoldd -p manifold-client --no-run";
    }
  );

  manifoldd = craneLib.buildPackage (
    craneBaseArgs
    // {
      cargoArtifacts = manifolddArtifacts;
      cargoBuildExtraArgs = "-p manifoldd -p manifold-client --bins";
      doCheck = false;
    }
  );

  shellCheck = pkgs.runCommand "manifold-shellcheck" { nativeBuildInputs = [ pkgs.shellcheck ]; } ''
    cp -R ${src}/. source
    chmod -R u+w source
    cd source

    mapfile -d "" shell_files < <(
      find . \
        \( -path "./.git" -o -path "./.git/*" \
        -o -path "./.manifold" -o -path "./.manifold/*" \
        -o -path "*/.manifold" -o -path "*/.manifold/*" \
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
    name = "manifold-wasm-modules";
    inherit src;
    cargoVendorDir = wasmCargoVendorDir;
    nativeBuildInputs = [
      craneLib.configureCargoVendoredDepsHook
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

      found=0
      while IFS= read -r manifest; do
        found=1
        crate_dir="$(dirname "$manifest")"
        case "$crate_dir" in
          examples/wasm-preview1-worker|plugins/workspace-file-log)
            wasm_target=wasm32-wasip1-threads
            ;;
          *)
            wasm_target=wasm32-unknown-unknown
            ;;
        esac
        echo "building_wasm=$manifest target=$wasm_target"
        (
          cd "$crate_dir"
          CARGO_TARGET_DIR="$PWD/target" \
            cargo build --manifest-path Cargo.toml --target "$wasm_target" --release --locked
        )
      done < <(
        {
          find examples -mindepth 2 -maxdepth 2 -path 'examples/wasm-*/Cargo.toml' -print
          find plugins -mindepth 2 -maxdepth 2 -path 'plugins/*/Cargo.toml' -print
        } | sort
      )

      if [[ "$found" != "1" ]]; then
        echo "ERR no WASM module Cargo.toml files found under examples or plugins" >&2
        exit 2
      fi

      if [[ -f examples/wasm-component-actor/Cargo.toml ]]; then
        component_core="examples/wasm-component-actor/target/wasm32-unknown-unknown/release/manifold_component_actor_example.wasm"
        component_output="examples/wasm-component-actor/target/wasm32-unknown-unknown/release/manifold_component_actor_example.component.wasm"
        if [[ ! -s "$component_core" ]]; then
          echo "ERR component actor core module was not built at $component_core" >&2
          exit 2
        fi
        echo "componentizing_wasm=$component_core output=$component_output"
        wasm-tools component new "$component_core" --output "$component_output"
        wasm-tools validate "$component_output"
        wasm-tools component wit "$component_output" >/dev/null
      fi

      if [[ -f examples/wasm-plugin-graphics/Cargo.toml ]]; then
        component_core="examples/wasm-plugin-graphics/target/wasm32-unknown-unknown/release/manifold_plugin_graphics_example.wasm"
        component_output="examples/wasm-plugin-graphics/target/wasm32-unknown-unknown/release/manifold_plugin_graphics_example.component.wasm"
        if [[ ! -s "$component_core" ]]; then
          echo "ERR plugin graphics core module was not built at $component_core" >&2
          exit 2
        fi
        echo "componentizing_wasm=$component_core output=$component_output"
        wasm-tools component new "$component_core" --output "$component_output"
        wasm-tools validate "$component_output"
        wasm-tools component wit "$component_output" >/dev/null
      fi

      if [[ -f plugins/component-workspace-watch/Cargo.toml ]]; then
        component_core="plugins/component-workspace-watch/target/wasm32-unknown-unknown/release/manifold_component_workspace_watch_plugin.wasm"
        component_output="plugins/component-workspace-watch/target/wasm32-unknown-unknown/release/manifold_component_workspace_watch_plugin.component.wasm"
        if [[ ! -s "$component_core" ]]; then
          echo "ERR Component workspace watch core module was not built at $component_core" >&2
          exit 2
        fi
        echo "componentizing_wasm=$component_core output=$component_output"
        wasm-tools component new "$component_core" --output "$component_output"
        wasm-tools validate "$component_output"
        wasm-tools component wit "$component_output" >/dev/null
      fi

      if [[ -f plugins/component-runtime-io/Cargo.toml ]]; then
        component_core="plugins/component-runtime-io/target/wasm32-unknown-unknown/release/manifold_component_runtime_io_plugin.wasm"
        component_output="plugins/component-runtime-io/target/wasm32-unknown-unknown/release/manifold_component_runtime_io_plugin.component.wasm"
        if [[ ! -s "$component_core" ]]; then
          echo "ERR Component runtime I/O core module was not built at $component_core" >&2
          exit 2
        fi
        echo "componentizing_wasm=$component_core output=$component_output"
        wasm-tools component new "$component_core" --output "$component_output"
        wasm-tools validate "$component_output"
        wasm-tools component wit "$component_output" >/dev/null
      fi

      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall

      mkdir -p "$out"
      while IFS= read -r wasm; do
        # The raw core module is an intermediate input to the Component Model
        # encoder. Only its validated component is a runnable plugin artifact.
        if [[ "$wasm" == "examples/wasm-component-actor/target/wasm32-unknown-unknown/release/manifold_component_actor_example.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "examples/wasm-plugin-graphics/target/wasm32-unknown-unknown/release/manifold_plugin_graphics_example.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/component-workspace-watch/target/wasm32-unknown-unknown/release/manifold_component_workspace_watch_plugin.wasm" ]]; then
          continue
        fi
        if [[ "$wasm" == "plugins/component-runtime-io/target/wasm32-unknown-unknown/release/manifold_component_runtime_io_plugin.wasm" ]]; then
          continue
        fi
        install -D "$wasm" "$out/$wasm"
      done < <(
        {
          find examples -path '*/target/wasm32-*/release/*.wasm' ! -path '*/release/deps/*' -print
          find plugins -path '*/target/wasm32-*/release/*.wasm' ! -path '*/release/deps/*' -print
        } | sort
      )

      runHook postInstall
    '';
  };

  wasmIoWorkloads = craneLib.buildPackage (
    craneBaseArgs
    // {
      inherit cargoArtifacts;
      pname = "manifold-wasm-io-workloads";
      cargoBuildExtraArgs = "-p manifold-wasm --examples";
      doCheck = false;
      installPhase = ''
        runHook preInstall
        mkdir -p "$out/bin"
        install -m755 target/release/examples/actor_worker_workload \
          "$out/bin/manifold-component-io-workload"
        install -m755 target/release/examples/preview1_worker_workload \
          "$out/bin/manifold-preview1-io-workload"
        install -m755 target/release/examples/io_rube_goldberg \
          "$out/bin/manifold-wasm-io-rube-goldberg-host"
        runHook postInstall
      '';
    }
  );

  wasmIoCheck =
    pkgs.runCommand "manifold-wasm-io-check"
      {
        nativeBuildInputs = [ wasmIoWorkloads ];
      }
      ''
        manifold-component-io-workload \
          ${wasmModules}/examples/wasm-component-actor/target/wasm32-unknown-unknown/release/manifold_component_actor_example.component.wasm
        manifold-preview1-io-workload \
          ${wasmModules}/examples/wasm-preview1-worker/target/wasm32-wasip1-threads/release/manifold_preview1_worker_example.wasm

        mkdir -p "$out"
        touch "$out/passed"
      '';

  wasmIoRube = pkgs.writeShellApplication {
    name = "manifold-wasm-io-rube-goldberg";
    runtimeInputs = [ wasmIoWorkloads ];
    text = ''
      exec manifold-wasm-io-rube-goldberg-host \
        ${wasmModules}/examples/wasm-component-actor/target/wasm32-unknown-unknown/release/manifold_component_actor_example.component.wasm \
        ${wasmModules}/examples/wasm-preview1-worker/target/wasm32-wasip1-threads/release/manifold_preview1_worker_example.wasm \
        "$@"
    '';
  };

  wasmIoRubeQuickCheck =
    pkgs.runCommand "manifold-wasm-io-rube-goldberg-quick-check"
      {
        nativeBuildInputs = [
          pkgs.coreutils
          pkgs.python3
          wasmIoRube
        ];
      }
      ''
        artifacts="$out/artifacts"
        mkdir -p "$artifacts"

        timeout --kill-after=10s 180s \
          manifold-wasm-io-rube-goldberg \
          --profile quick \
          --seed 5566816691025824836 \
          --out-dir "$artifacts"

        test -s "$artifacts/trace.jsonl"
        test -s "$artifacts/manifest.json"
        test -s "$artifacts/resource-samples.jsonl"
        test -s "$artifacts/summary.json"
        test ! -e "$artifacts/failure.json"
        python3 - \
          "$artifacts/manifest.json" \
          "$artifacts/trace.jsonl" \
          "$artifacts/resource-samples.jsonl" \
          "$artifacts/summary.json" <<'PY'
        import json
        import sys

        def load_json(path):
            with open(path, encoding="utf-8") as stream:
                return json.load(stream)

        def load_jsonl(path):
            with open(path, encoding="utf-8") as stream:
                records = [json.loads(line) for line in stream if line.strip()]
            assert records, path
            return records

        manifest = load_json(sys.argv[1])
        trace = load_jsonl(sys.argv[2])
        resources = load_jsonl(sys.argv[3])
        summary = load_json(sys.argv[4])

        assert manifest["schema"] == "manifold.io-rube-goldberg.manifest.v1", manifest
        assert manifest["plan_version"] == 1, manifest
        assert manifest["profile"] == "quick", manifest
        assert manifest["seed"] == 5566816691025824836, manifest
        assert manifest["topology"] == {
            "actors": 32,
            "component_actors": 16,
            "preview1_actors": 16,
            "workers_per_actor": 4,
        }, manifest
        assert manifest["work"]["steps_per_epoch"] == 1536, manifest
        assert manifest["work"]["epochs"] == 1, manifest

        assert summary["status"] == "ok", summary
        assert summary["plan_version"] == manifest["plan_version"], summary
        assert summary["profile"] == "quick", summary
        assert summary["seed"] == manifest["seed"], summary
        assert summary["actors"] == 32, summary
        assert summary["workers_per_actor"] == 4, summary
        assert summary["epochs_completed"] == 1, summary
        assert summary["transitions_planned_per_epoch"] == 1536, summary
        assert summary["transitions_completed"] == 1536, summary
        assert summary["terminal_receipts"] == (
            summary["high_volume_terminal_receipts"]
            + summary["replacement_receipts"]
        ), summary
        assert summary["mailbox_pressure_accepted"] > 0, summary
        assert summary["mailbox_pressure_rejected"] > 0, summary
        pressure = summary["mailbox_pressure_by_backend"]
        assert all(pressure[k]["accepted"] > 0 for k in ("component", "preview1")), pressure
        assert all(pressure[k]["rejected"] > 0 for k in ("component", "preview1")), pressure
        assert summary["mailbox_pressure_accepted"] == sum(
            pressure[k]["accepted"] for k in ("component", "preview1")
        ), pressure
        assert summary["mailbox_pressure_rejected"] == sum(
            pressure[k]["rejected"] for k in ("component", "preview1")
        ), pressure
        assert summary["duplicates"] == 0, summary
        assert summary["lost"] == 0, summary
        assert summary["stale_deliveries"] == 0, summary
        assert summary["stale_operations_rejected"] > 0, summary
        assert all(value == 0 for value in summary["coordination_after_shutdown"].values()), summary

        assert trace[0]["kind"] == "header", trace[0]
        transitions = [record for record in trace if record.get("kind") == "transition"]
        assert len(transitions) == summary["transitions_completed"], len(transitions)
        assert [record["id"] for record in transitions] == list(range(1536))

        assert resources[-1]["phase"] == "after-shutdown", resources[-1]
        pressure_resources = next(
            record for record in resources if record["phase"] == "after-mailbox-backpressure"
        )
        assert any(actor["mailbox_dropped"] > 0 for actor in pressure_resources["preview1"])
        for actor in resources[-1]["component"]:
            assert actor["completion_alive"] is False, actor
            assert actor["completion_outstanding"] == 0, actor
            assert actor["notifications"] == 0, actor
            assert actor["pollables"] == 0, actor
            assert actor["wait_registrations"] == 0, actor
            assert actor["pending_actor_events"] == 0, actor
        for actor in resources[-1]["preview1"]:
            assert actor["workers_active"] == 0, actor
            assert actor["notifications"] == 0, actor
            assert actor["pollables"] == 0, actor
            assert actor["wait_registrations"] == 0, actor
            assert actor["pending_actor_events"] == 0, actor
        PY
        touch "$out/passed"
      '';

  runtimeFiles = pkgs.runCommand "manifold-runtime-files" { } ''
    mkdir -p "$out"

    install -D ${src}/manifold.toml "$out/manifold.toml"

    mkdir -p "$out/apps"
    cp -R ${src}/apps/. "$out/apps/"
    rm -rf "$out/apps/__pycache__"

    if [[ -d ${wasmModules}/examples ]]; then
      cp -R ${wasmModules}/examples "$out/examples"
    else
      echo "ERR packaged WASM modules did not include an examples directory" >&2
      exit 2
    fi

    if [[ -d ${wasmModules}/plugins ]]; then
      cp -R ${wasmModules}/plugins "$out/plugins"
    else
      echo "ERR packaged WASM modules did not include a plugins directory" >&2
      exit 2
    fi
  '';

  manifold = pkgs.symlinkJoin {
    name = "manifold";
    paths = [
      manifoldd
      runtimeFiles
    ];
    nativeBuildInputs = [ pkgs.makeWrapper ];
    postBuild = ''
      for program in manifold manifold-sessiond manifold-runner-exec manifold-runner-nix-develop; do
        wrapProgram "$out/bin/$program" \
          --prefix PATH : "${appToolPath}" \
          --prefix LD_LIBRARY_PATH : "${nativeLibraryPath}" \
          --prefix GI_TYPELIB_PATH : "${gtkTypelibPath}" \
          --prefix XDG_DATA_DIRS : "${gtkDataDirs}" \
          --run 'export GSETTINGS_SCHEMA_DIR="''${GSETTINGS_SCHEMA_DIR:-${gsettingsSchemaDir}}"' \
          --run 'export MANIFOLD_CONFIG="''${MANIFOLD_CONFIG:-'"$out"'/manifold.toml}"'
      done
    '';
  };

  desktopActions = pkgs.writeShellApplication {
    name = "manifold-desktop";
    runtimeInputs = [ pkgs.coreutils ];
    text = ''
      set -euo pipefail

      action="''${1:-show}"
      manifold_bin="${manifold}/bin/manifold"

      export LD_LIBRARY_PATH="${nativeLibraryPath}:''${LD_LIBRARY_PATH:-}"
      export GI_TYPELIB_PATH="${gtkTypelibPath}:''${GI_TYPELIB_PATH:-}"
      export XDG_DATA_DIRS="${gtkDataDirs}:''${XDG_DATA_DIRS:-}"
      export GSETTINGS_SCHEMA_DIR="${gsettingsSchemaDir}"
      export MANIFOLD_CONFIG="''${MANIFOLD_CONFIG:-${manifold}/manifold.toml}"

      case "$action" in
        show)
          exec "$manifold_bin" show --last-focused
          ;;
        new-session)
          response="$("$manifold_bin" ctl surface-session dir-new "$PWD" default)"
          case "$response" in
            "OK surface-session active="*)
              session_id="''${response#* active=}"
              session_id="''${session_id%% *}"
              exec "$manifold_bin" show --id "$session_id" --set-default
              ;;
            *)
              printf '%s\n' "$response" >&2
              exit 1
              ;;
          esac
          ;;
        picker)
          "$manifold_bin" show >/dev/null
          exec "${profiles.sessionSwitcherPython}/bin/python3" \
            "${runtimeFiles}/apps/manifold-session-switcher.py" \
            --width 360 \
            --close-after-activate
          ;;
        quit)
          exec "$manifold_bin" ctl quit
          ;;
        *)
          echo "usage: manifold-desktop [show|new-session|picker|quit]" >&2
          exit 2
          ;;
      esac
    '';
  };

  desktopIntegration = pkgs.runCommand "manifold-desktop-integration" { } ''
    install -D -m 0644 ${../assets/icons/hicolor/512x512/apps/manifold.png} \
      "$out/share/icons/hicolor/512x512/apps/manifold.png"
    install -D -m 0644 ${../assets/icons/hicolor/512x512/apps/manifold.png} \
      "$out/share/pixmaps/manifold.png"

    install -D -m 0644 /dev/stdin "$out/share/applications/manifold.desktop" <<EOF
    [Desktop Entry]
    Type=Application
    Name=Manifold
    Comment=Show the Manifold workspace
    Exec=${desktopActions}/bin/manifold-desktop show
    Icon=manifold
    Terminal=false
    Categories=Development;
    StartupNotify=true
    Actions=NewSession;OpenPicker;Quit;

    [Desktop Action NewSession]
    Name=New Session
    Exec=${desktopActions}/bin/manifold-desktop new-session

    [Desktop Action OpenPicker]
    Name=Open Picker
    Exec=${desktopActions}/bin/manifold-desktop picker

    [Desktop Action Quit]
    Name=Quit
    Exec=${desktopActions}/bin/manifold-desktop quit
    EOF

    install -D -m 0644 /dev/stdin "$out/share/applications/org.manifold.SessionSwitcher.desktop" <<EOF
    [Desktop Entry]
    Type=Application
    Name=Manifold Session Picker
    Comment=Pick a Manifold session
    Exec=${desktopActions}/bin/manifold-desktop picker
    Icon=manifold
    Terminal=false
    Categories=Development;
    StartupNotify=true
    NoDisplay=true
    EOF
  '';

  manifoldWithDesktop = pkgs.symlinkJoin {
    name = "manifold";
    paths = [
      manifold
      desktopActions
      desktopIntegration
    ];
  };

  devEnv = pkgs.buildEnv {
    name = "manifold-dev-env";
    paths = profiles.devTools ++ profiles.nativeLibs ++ profiles.gtkRuntime;
    ignoreCollisions = true;
  };

  devEnvFull = pkgs.buildEnv {
    name = "manifold-dev-env-full";
    paths =
      profiles.devTools
      ++ profiles.fullDevExtras
      ++ profiles.smokeTools
      ++ profiles.nativeLibs
      ++ profiles.gtkRuntime;
    ignoreCollisions = true;
  };

  ciTools = pkgs.buildEnv {
    name = "manifold-ci-tools";
    paths = [
      pkgs.act
      pkgs.actionlint
    ];
  };

  localCi = pkgs.writeShellApplication {
    name = "manifold-ci";
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
        echo "ci=stage name=test status=skipped"
        exit "$status"
      fi

      echo "ci=job name=test status=started"
      nix build --print-build-logs ".#checks.$system.test" ".#checks.$system.software-e2e"
      echo "ci=job name=test status=passed"
      echo "ci=all status=passed"
    '';
  };

  cacheCi = pkgs.writeShellApplication {
    name = "manifold-cache-ci";
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

      cache="''${CACHIX_CACHE:-manifold-platform}"
      if (($# > 0)); then
        cache="$1"
        shift
      fi

      if (($# > 0)); then
        echo "usage: nix run .#cache-ci [cachix-cache-name]" >&2
        exit 2
      fi

      system="$(nix eval --raw --impure --expr builtins.currentSystem)"
      attrs=(
        .#default
        .#ci
        ".#checks.$system.lint"
        ".#checks.$system.test"
        ".#checks.$system.software-e2e"
        .#cargoArtifacts
        .#clippyCheck
        .#shellCheck
        .#manifolddArtifacts
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
    name = "manifold-prime-cargo-target";
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
    name = "manifold-format";
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
          .manifold/* | */.manifold/* | reference/*)
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
    name = "manifold-lint";
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
            -o -path "./.manifold" -o -path "./.manifold/*" \
            -o -path "./target" -o -path "./target/*" \
            -o -path "./reference" -o -path "./reference/*" \
            -o -path "./result" -o -path "./result/*" \) -prune \
            -o -type f -print0
        )
      fi

      shell_files=()
      for file in "''${source_files[@]}"; do
        case "$file" in
          .manifold/* | */.manifold/* | reference/*)
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

  lintCheck = pkgs.runCommand "manifold-lint-check" { } ''
    echo "lint=rust command=cargo-check artifacts=${cargoArtifacts}"
    echo "lint=rust command=cargo-clippy check=${clippyCheck}"
    echo "lint=shell check=${shellCheck}"

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
        manifoldd
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
    name = "manifold-dev-workflow";
    path = appToolPath;
    exportDevVars = false;
  };

  devWorkflowFull = mkWorkflow {
    name = "manifold-dev-workflow-full";
    path = fullDevToolPath;
    exportDevVars = true;
  };

  devWorkflowWithRuntime = pkgs.symlinkJoin {
    name = "manifold-dev-workflow";
    paths = [
      devWorkflow
      runtimeFiles
      desktopActions
      desktopIntegration
    ];
  };

  devWorkflowFullWithRuntime = pkgs.symlinkJoin {
    name = "manifold-dev-workflow-full";
    paths = [
      devWorkflowFull
      runtimeFiles
      desktopActions
      desktopIntegration
    ];
  };
in
{
  default = manifoldWithDesktop;
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
  "software-e2e-artifacts" = softwareE2eArtifacts;
  "runtime-files" = runtimeFiles;
  "wasm-modules" = wasmModules;
  "wasm-examples" = wasmModules;
  "wasm-io-check" = wasmIoCheck;
  "wasm-io-rube" = wasmIoRube;
  "wasm-io-rube-quick-check" = wasmIoRubeQuickCheck;
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
    devWorkflow
    devWorkflowFullWithRuntime
    devWorkflowWithRuntime
    devWorkflowFull
    desktopActions
    desktopIntegration
    manifoldWithDesktop
    manifold
    manifoldd
    manifolddArtifacts
    lintCheck
    runtimeFiles
    shellCheck
    softwareE2eArtifacts
    softwareE2eCapture
    softwareE2eCheck
    testCheck
    wasmIoCheck
    wasmIoRube
    wasmIoRubeQuickCheck
    wasmIoWorkloads
    wasmModules
    ;
}
