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
  nativePkgConfigPath = lib.concatStringsSep ":" [
    (lib.makeSearchPathOutput "dev" "lib/pkgconfig" profiles.nativeLibs)
    (lib.makeSearchPath "lib/pkgconfig" profiles.nativeLibs)
    (lib.makeSearchPathOutput "dev" "share/pkgconfig" profiles.nativeLibs)
    (lib.makeSearchPath "share/pkgconfig" profiles.nativeLibs)
  ];

  cargoNix = pkgs.callPackage ../Cargo.nix {
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
    src = ../.;
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
        wasmExamples
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
in
{
  default = manifoldd;
  "dev-workflow" = devWorkflow;
  "dev-workflow-full" = devWorkflowFull;
  "dev-env" = devEnv;
  "dev-env-full" = devEnvFull;
  "wasm-examples" = wasmExamples;

  inherit
    devEnv
    devEnvFull
    devWorkflow
    devWorkflowFull
    manifoldd
    wasmExamples
    ;
}
