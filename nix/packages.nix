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
  craneSrc = craneLib.cleanCargoSource src;

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
      cargoExtraArgs = "--locked -p manifoldd";
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
    echo "test=software-e2e check=${softwareE2eCheck}"
    mkdir -p "$out"
    ln -s ${softwareE2eCheck} "$out/software-e2e"
    touch "$out/passed"
  '';

  manifolddArtifacts = craneLib.buildDepsOnly (
    craneBaseArgs
    // {
      cargoBuildExtraArgs = "-p manifoldd --bins";
      cargoCheckExtraArgs = "-p manifoldd --all-targets";
      cargoTestExtraArgs = "-p manifoldd --no-run";
    }
  );

  manifoldd = craneLib.buildPackage (
    craneBaseArgs
    // {
      cargoArtifacts = manifolddArtifacts;
      cargoBuildExtraArgs = "-p manifoldd --bins";
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
  "wasm-examples" = wasmExamples;
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
    wasmExamples
    ;
}
