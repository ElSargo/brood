{ pkgs }:

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
      base != "Cargo.nix" && base != "target";
  };
  nativePkgConfigPath = lib.concatStringsSep ":" [
    (lib.makeSearchPathOutput "dev" "lib/pkgconfig" profiles.nativeLibs)
    (lib.makeSearchPath "lib/pkgconfig" profiles.nativeLibs)
    (lib.makeSearchPathOutput "dev" "share/pkgconfig" profiles.nativeLibs)
    (lib.makeSearchPath "share/pkgconfig" profiles.nativeLibs)
  ];

  cargoVendor = pkgs.rustPlatform.importCargoLock {
    lockFile = ../Cargo.lock;
  };

  generatedCargoNix =
    pkgs.runCommand "manifold-generated-cargo-nix"
      {
        nativeBuildInputs = [ pkgs.crate2nix ];
      }
      ''
        export HOME="$TMPDIR/home"
        mkdir -p "$HOME" "$out"

        cp -R ${src}/. "$out/"
        chmod -R u+w "$out"

        cd "$out"
        mkdir -p .cargo
        cat > .cargo/config.toml <<EOF
        [source.crates-io]
        replace-with = "vendored-sources"

        [source.vendored-sources]
        directory = "${cargoVendor}"

        [net]
        offline = true
        EOF

        export CARGO_NET_OFFLINE=true
        crate2nix generate
      '';

  cargoNix = pkgs.callPackage "${generatedCargoNix}/Cargo.nix" {
    defaultCrateOverrides = pkgs.defaultCrateOverrides // {
      "av-scenechange" = attrs: {
        CARGO_ENCODED_RUSTFLAGS = attrs.CARGO_ENCODED_RUSTFLAGS or "";
      };
      rav1e = attrs: {
        CARGO_ENCODED_RUSTFLAGS = attrs.CARGO_ENCODED_RUSTFLAGS or "";
      };
      manifoldd = attrs: {
        nativeBuildInputs = (attrs.nativeBuildInputs or [ ]) ++ [ pkgs.pkg-config ];
        buildInputs = (attrs.buildInputs or [ ]) ++ profiles.nativeLibs;
      };
    };
  };

  manifoldd = cargoNix.workspaceMembers."manifoldd".build.override {
    runTests = false;
  };

  wasmExamples = pkgs.stdenvNoCC.mkDerivation {
    name = "manifold-wasm-examples";
    inherit src;
    nativeBuildInputs = [ profiles.rustToolchain ];

    buildPhase = ''
      runHook preBuild

      export CARGO_HOME="$TMPDIR/cargo-home"
      export HOME="$TMPDIR/home"
      mkdir -p "$CARGO_HOME" "$HOME"

      found=0
      while IFS= read -r manifest; do
        found=1
        crate_dir="$(dirname "$manifest")"
        echo "building_wasm=$manifest"
        CARGO_TARGET_DIR="$PWD/$crate_dir/target" \
          cargo build --manifest-path "$manifest" --target wasm32-unknown-unknown --release --locked
      done < <(find examples -mindepth 2 -maxdepth 2 -path 'examples/wasm-*/Cargo.toml' -print | sort)

      if [[ "$found" != "1" ]]; then
        echo "ERR no WASM example Cargo.toml files found under examples" >&2
        exit 2
      fi

      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall

      mkdir -p "$out"
      while IFS= read -r wasm; do
        install -D "$wasm" "$out/$wasm"
      done < <(find examples -path '*/target/wasm32-unknown-unknown/release/*.wasm' -print | sort)

      runHook postInstall
    '';
  };

  runtimeFiles = pkgs.runCommand "manifold-runtime-files" { } ''
    mkdir -p "$out"

    install -D ${src}/manifold.toml "$out/manifold.toml"

    mkdir -p "$out/apps"
    cp -R ${src}/apps/. "$out/apps/"
    rm -rf "$out/apps/__pycache__"

    if [[ -d ${wasmExamples}/examples ]]; then
      cp -R ${wasmExamples}/examples "$out/examples"
    else
      echo "ERR packaged WASM examples did not include an examples directory" >&2
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

      run_job() {
        local name="$1"
        shift

        {
          echo "ci=job name=$name status=started"
          "$@"
          echo "ci=job name=$name status=passed"
        } >"$tmpdir/$name.log" 2>&1
      }

      run_job lint nix run .#lint &
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
      nix develop .#full -c cargo test -p manifoldd --locked
      echo "ci=job name=test status=passed"
      echo "ci=all status=passed"
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

  lintCheck =
    pkgs.runCommand "manifold-lint-check"
      {
        nativeBuildInputs = [ linter ];
      }
      ''
        export HOME="$TMPDIR/home"
        export CARGO_HOME="$TMPDIR/cargo-home"
        export CARGO_TARGET_DIR="$TMPDIR/target"
        export CARGO_NET_OFFLINE=true
        mkdir -p "$HOME" "$CARGO_HOME" source

        cp -R ${src}/. source/
        chmod -R u+w source
        cd source

        mkdir -p .cargo
        cat > .cargo/config.toml <<EOF
        [source.crates-io]
        replace-with = "vendored-sources"

        [source.vendored-sources]
        directory = "${cargoVendor}"

        [net]
        offline = true
        EOF

        manifold-lint

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
  "ci-tools" = ciTools;
  lint = linter;
  "runtime-files" = runtimeFiles;
  "wasm-examples" = wasmExamples;
  inherit formatter linter;

  inherit
    devEnv
    devEnvFull
    ciTools
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
    lintCheck
    runtimeFiles
    wasmExamples
    ;
}
