{ pkgs }:

let
  profiles = import ./profiles.nix { inherit pkgs; };
  graphicsAndSessionEnvironment = {
    NIX_CONFIG = "experimental-features = nix-command flakes";
    __EGL_VENDOR_LIBRARY_FILENAMES = "${pkgs.mesa}/share/glvnd/egl_vendor.d/50_mesa.json";
    LIBGL_DRIVERS_PATH = "${pkgs.mesa}/lib/dri";
    GBM_BACKENDS_PATH = "${pkgs.mesa}/lib/gbm";
  };
in
{
  default = pkgs.mkShell (
    graphicsAndSessionEnvironment
    // {
      packages = profiles.devToolsWithTools ++ profiles.nativeLibs ++ profiles.smokeTools;

      LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath profiles.nativeLibs;
      RUST_SRC_PATH = "${profiles.rustToolchainWithTools}/lib/rustlib/src/rust/library";
      RQUICKJS_SYS_NO_WASI_SDK = "1";
      CC_wasm32_wasip1 = "${profiles.wasi32Cc}/bin/wasm32-unknown-wasi-cc";
      AR_wasm32_wasip1 = "${profiles.wasi32Binutils}/bin/wasm32-unknown-wasi-ar";

      shellHook = ''
        export BROOD_DEV_SHELL=1
        echo "Brood dev shell: cargo, wasm32 target, Wayland/EGL libs, and smoke tools are available."
      '';
    }
  );

  full = pkgs.mkShell (
    graphicsAndSessionEnvironment
    // {
      packages =
        profiles.devToolsWithTools ++ profiles.fullDevExtras ++ profiles.nativeLibs ++ profiles.smokeTools;

      LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath profiles.nativeLibs;
      RUST_SRC_PATH = "${profiles.rustToolchainWithTools}/lib/rustlib/src/rust/library";
      RQUICKJS_SYS_NO_WASI_SDK = "1";
      CC_wasm32_wasip1 = "${profiles.wasi32Cc}/bin/wasm32-unknown-wasi-cc";
      AR_wasm32_wasip1 = "${profiles.wasi32Binutils}/bin/wasm32-unknown-wasi-ar";

      shellHook = ''
        export BROOD_DEV_SHELL=1
        echo "Brood full dev shell: cargo, wasm32 target, Wayland/EGL libs, smoke tools, Docker, Go, and Firefox are available."
      '';
    }
  );
}
