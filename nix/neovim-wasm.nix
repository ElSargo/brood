{ pkgs }:

let
  lib = pkgs.lib;
  zig = pkgs.zig_0_16;
  emscripten = pkgs.emscripten;
  runtimeSource = ../plugins/neovim-runtime;
  version = "0.13.0-dev-635acc7";
  neovimRevision = "635acc7dc848853fe4a5db2f0c85dc52e8eabed0";

  neovimSource = pkgs.fetchFromGitHub {
    owner = "neovim";
    repo = "neovim";
    rev = neovimRevision;
    hash = "sha256-E2JF5SZgSu02yToT7qF6w2+nn9fmT7BjHk4b0sZ+eYg=";
  };

  # Fetch the dependency graph selected by the actual wasm build. Fetching the
  # root manifest alone misses lazy nested sources, while `--fetch=all` also
  # traverses tree-sitter's unused Windows Wasmtime archives.
  selectedZigDeps =
    pkgs.runCommand "brood-neovim-wasm-${version}-zig-deps"
      {
        src = neovimSource;
        nativeBuildInputs = [
          emscripten
          zig
        ];
        outputHashAlgo = null;
        outputHashMode = "recursive";
        outputHash = "sha256-GYvP4iMGb9qa/iVHpvWhx/xqk2eGA6XoofYDk6GZL8Q=";
      }
      ''
        export ZIG_GLOBAL_CACHE_DIR="$(mktemp -d)"

        runHook unpackPhase
        cd "$sourceRoot"
        zig build nvim_bin \
          -Dtarget=wasm32-emscripten \
          -Dhost=native \
          -Doptimize=ReleaseFast \
          -Demscripten-sysroot=${emscripten}/share/emscripten/cache/sysroot \
          --fetch

        # Zig's fetch mode intentionally stops at root-manifest dependencies.
        # Fetch the lazy packages selected when the wasm build graph is
        # evaluated; their content hashes remain enforced by build.zig.zon.
        zig fetch "https://www.lua.org/ftp/lua-5.1.5.tar.gz"
        zig fetch "https://github.com/neovim/deps/raw/d495ee6f79e7962a53ad79670cb92488abe0b9b4/opt/lpeg-1.1.0.tar.gz"
        zig fetch "git+https://github.com/luvit/luv?ref=1.52.1-0#f65fe9d7616f9fbcd20227fc7a73b38d1c5c180d"
        zig fetch "https://github.com/lunarmodules/lua-compat-5.3/archive/v0.13.tar.gz"
        zig fetch "git+https://github.com/tree-sitter/tree-sitter#4d0f126996baf50c99a15746c582cc423bcc3910"
        zig fetch "git+https://github.com/allyourcodebase/libuv.git#c1ccce24e1077ae7780857c89fb4b3aeb9080315"
        zig fetch "git+https://github.com/libuv/libuv?ref=v1.52.1#a19ceeb13a3689cfd20bf96b283136cff08a0dc6"
        zig fetch "git+https://github.com/juliastrings/utf8proc?ref=v2.11.3#e5e799221b45bbb90f5fdc5c69b6b8dfbf017e78"
        zig fetch "git+https://github.com/neovim/unibilium?ref=v2.1.2#bfcb0350129dd76893bc90399cf37c45812268a2"

        mv "$ZIG_GLOBAL_CACHE_DIR/p" "$out"
      '';

  # This expensive output deliberately has no reference to runtimeSource.
  # Editing the Node/Lua bridge rebuilds only the lightweight wrapper below.
  neovimWasmCore = pkgs.stdenv.mkDerivation {
    pname = "brood-neovim-wasm-core";
    inherit version;

    src = neovimSource;
    zigDeps = selectedZigDeps;

    # The fixed core hash also lets a previously built editor survive harmless
    # derivation refactors. It is filled from a verified three-artifact output.
    outputHashAlgo = null;
    outputHashMode = "recursive";
    outputHash = "sha256-b6UyjUEWijCM2pJNzyQDyyy9O5N7sTmkYkU0Gugt9X8=";

    strictDeps = true;
    enableParallelBuilding = true;
    nativeBuildInputs = [
      emscripten
      zig
    ];

    postPatch = ''
      # zlua's translator does not discover Nix's non-FHS libc headers. Pass
      # target and host include roots explicitly through its existing
      # additional_system_headers dependency option.
      substituteInPlace build.zig \
        --replace-fail \
          '    const target_host = if (cross_compiling) b.resolveTargetQuery(try std.Build.parseTargetQuery(.{ .arch_os_abi = host })) else target;' \
          '    const target_host = if (cross_compiling) b.resolveTargetQuery(try std.Build.parseTargetQuery(.{ .arch_os_abi = host })) else target;
          const host_include = b.option([]const u8, "host-include", "path to host libc headers") orelse
              @panic("-Dhost-include required for cross compilation");'
      sed -i \
        '0,/        \.system_lua = sys_opts\.lua,$/s//&\n        .additional_system_headers = @constCast(\&[_]std.Build.LazyPath{emscripten_include.?})[0..],/' \
        build.zig
      sed -i \
        '/const ziglua_host =/,/) else ziglua;/s/        \.shared = false,$/&\n        .additional_system_headers = @constCast(\&[_]std.Build.LazyPath{std.Build.LazyPath{ .cwd_relative = host_include }})[0..],/' \
        build.zig
      test "$(grep -c additional_system_headers build.zig)" -eq 2

      # Generated Tree-sitter lexers are synchronous CPU-only code. Indirect
      # calls otherwise make whole-program Asyncify conservatively instrument
      # them, and Binaryen's local-liveness analysis becomes pathological on
      # the largest generated lexer. Keep Asyncify and indirect-call analysis
      # intact while excluding only these known-synchronous function families.
      substituteInPlace build.zig \
        --replace-fail \
          '            "-sASYNCIFY=1",' \
          '            "-sASYNCIFY=1",
                  "-sASYNCIFY_REMOVE=[\"ts_lex*\"]",'
    '';

    configurePhase = ''
      runHook preConfigure

      export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-global-cache"
      mkdir -p "$ZIG_GLOBAL_CACHE_DIR"
      cp -rLT ${selectedZigDeps} "$ZIG_GLOBAL_CACHE_DIR/p"
      chmod -R u+w "$ZIG_GLOBAL_CACHE_DIR/p"

      runHook postConfigure
    '';

    buildPhase = ''
      runHook preBuild

      export HOME="$TMPDIR/neovim-wasm-home"
      mkdir -p "$HOME"
      zig build nvim_bin \
        -j"$NIX_BUILD_CORES" \
        -Dtarget=wasm32-emscripten \
        -Dhost=native \
        -Dhost-include=${pkgs.glibc.dev}/include \
        -Doptimize=ReleaseFast \
        -Demscripten-sysroot=${emscripten}/share/emscripten/cache/sysroot \
        --prefix "$PWD/zig-out"

      for artifact in nvim.js nvim.wasm nvim.data; do
        if [[ ! -s "zig-out/bin/$artifact" ]]; then
          echo "missing Neovim WebAssembly artifact: zig-out/bin/$artifact" >&2
          exit 1
        fi
      done

      runHook postBuild
    '';

    doCheck = false;

    installPhase = ''
      runHook preInstall

      runtime="$out/share/brood/neovim-wasm"
      mkdir -p "$runtime"
      install -m0644 zig-out/bin/nvim.js "$runtime/nvim.js"
      sed -E -i \
        -e 's#/build/tmp[^[:space:]]+\.js#/build/emscripten-pre.js#g' \
        -e 's#/build/source/\.zig-cache/o/[[:alnum:]]+/nvim\.data#nvim.data#g' \
        "$runtime/nvim.js"
      if grep -Eq '/build/tmp|/build/source/\.zig-cache' "$runtime/nvim.js"; then
        echo "nondeterministic Emscripten build path remained in nvim.js" >&2
        exit 1
      fi
      chmod 0444 "$runtime/nvim.js"
      install -m0444 zig-out/bin/nvim.wasm "$runtime/nvim.wasm"
      install -m0444 zig-out/bin/nvim.data "$runtime/nvim.data"

      runHook postInstall
    '';

    passthru = {
      inherit neovimRevision;
    };

    meta = {
      description = "Pinned Neovim WebAssembly core";
      homepage = "https://github.com/neovim/neovim";
      license = lib.licenses.asl20;
      platforms = lib.platforms.linux;
    };
  };
in
pkgs.stdenvNoCC.mkDerivation {
  pname = "brood-neovim-wasm";
  inherit version;

  dontUnpack = true;
  strictDeps = true;
  nativeBuildInputs = [
    pkgs.makeWrapper
    pkgs.nodejs
  ];

  doCheck = true;
  checkPhase = ''
    runHook preCheck

    node --check ${runtimeSource}/runner.mjs
    node ${runtimeSource}/smoke-test.mjs \
      --runner ${runtimeSource}/runner.mjs \
      --module-dir ${neovimWasmCore}/share/brood/neovim-wasm

    runHook postCheck
  '';

  installPhase = ''
    runHook preInstall

    runtime="$out/share/brood/neovim-wasm"
    mkdir -p "$runtime" "$out/bin"
    cp -a ${neovimWasmCore}/share/brood/neovim-wasm/. "$runtime/"
    chmod u+w "$runtime"
    install -m0555 ${runtimeSource}/runner.mjs "$runtime/runner.mjs"
    install -m0444 ${runtimeSource}/brood.lua "$runtime/brood.lua"

    makeWrapper ${pkgs.nodejs}/bin/node "$out/bin/brood-neovim-wasm" \
      --add-flags "$runtime/runner.mjs"

    runHook postInstall
  '';

  passthru = {
    core = neovimWasmCore;
    inherit neovimRevision;
    protocolVersion = 1;
    runtimeRelativePath = "share/brood/neovim-wasm";
  };

  meta = {
    description = "Pinned Neovim WebAssembly runtime and Brood JSONL bridge";
    homepage = "https://github.com/neovim/neovim";
    license = lib.licenses.asl20;
    platforms = lib.platforms.linux;
  };
}
