{ pkgs }:

let
  profiles = import ./profiles.nix { inherit pkgs; };
in
{
  default = pkgs.mkShell {
    packages = profiles.devToolsWithTools ++ profiles.nativeLibs ++ profiles.smokeTools;

    LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath profiles.nativeLibs;
    RUST_SRC_PATH = "${profiles.rustToolchainWithTools}/lib/rustlib/src/rust/library";

    shellHook = ''
      export MANIFOLD_DEV_SHELL=1
      echo "Manifold dev shell: cargo, wasm32 target, Wayland/EGL libs, and smoke tools are available."
    '';
  };

  full = pkgs.mkShell {
    packages =
      profiles.devToolsWithTools ++ profiles.fullDevExtras ++ profiles.nativeLibs ++ profiles.smokeTools;

    LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath profiles.nativeLibs;
    RUST_SRC_PATH = "${profiles.rustToolchainWithTools}/lib/rustlib/src/rust/library";

    shellHook = ''
      export MANIFOLD_DEV_SHELL=1
      echo "Manifold full dev shell: cargo, wasm32 target, Wayland/EGL libs, smoke tools, Docker, Go, and Firefox are available."
    '';
  };
}
