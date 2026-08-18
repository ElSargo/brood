{ pkgs }:

let
  rustToolchain = pkgs.rust-bin.stable.latest.default.override {
    targets = [
      "wasm32-unknown-unknown"
      # Rust 1.96 knows the wasip3 target specification, but the upstream
      # distribution does not ship rust-std for it yet. P1/P2 are retained as
      # buildable compatibility/tooling paths while the target ABI moves to P3.
      "wasm32-wasip1"
      "wasm32-wasip1-threads"
      "wasm32-wasip2"
    ];
  };

  rustToolchainWithTools = pkgs.rust-bin.stable.latest.default.override {
    extensions = [
      "clippy"
      "rust-analyzer"
      "rust-src"
      "rustfmt"
    ];
    targets = [
      "wasm32-unknown-unknown"
      "wasm32-wasip1"
      "wasm32-wasip1-threads"
      "wasm32-wasip2"
    ];
  };

  nativeLibs = with pkgs; [
    cairo
    gtk4
    libadwaita
    libGL
    libgbm
    libinput
    libxkbcommon
    mesa
    seatd
    systemdLibs
    vulkan-loader
    wayland
  ];

  gtkRuntime = with pkgs; [
    adwaita-icon-theme
    cairo
    gdk-pixbuf
    glib
    gnome-themes-extra
    gobject-introspection-unwrapped
    graphene
    gsettings-desktop-schemas
    gtk4
    harfbuzz
    hicolor-icon-theme
    libadwaita
    pango.out
    shared-mime-info
  ];

  westonWithSimpleClients = pkgs.weston.overrideAttrs (old: {
    mesonFlags = map (
      flag: if flag == "-Dsimple-clients=" then "-Dsimple-clients=all" else flag
    ) old.mesonFlags;
  });

  sessionSwitcherPython = pkgs.python3.withPackages (
    ps: with ps; [
      dbus-python
      pycairo
      pygobject3
    ]
  );

  dbusSessionConfigForDev = pkgs.runCommand "brood-dbus-session.conf" { } ''
    substitute ${pkgs.dbus}/share/dbus-1/session.conf "$out" \
      --replace-fail \
      '<include ignore_missing="yes">/etc/dbus-1/session.conf</include>' \
      ""
  '';

  # The container's /etc/dbus-1/session.conf is only a distro redirect stub,
  # while the Nix D-Bus daemon is not patched to follow that distro layout.
  # Keep ordinary `dbus-run-session` test invocations working by replacing its
  # --session argument with a standalone configuration in the Nix closure.
  # Drop its legacy /etc include so complete NixOS-generated host
  # configurations cannot add a second listening address.
  dbusDaemonForDev = pkgs.writeShellScriptBin "dbus-daemon" ''
    args=()
    for arg in "$@"; do
      if [[ "$arg" == --session ]]; then
        args+=(--config-file=${dbusSessionConfigForDev})
      else
        args+=("$arg")
      fi
    done
    exec ${pkgs.dbus}/bin/dbus-daemon "''${args[@]}"
  '';

  appTools = [
    pkgs.coreutils
    pkgs.findutils
    pkgs.foot
    pkgs.git
    pkgs.nodejs
    pkgs.ripgrep
    sessionSwitcherPython
    pkgs.util-linux
    pkgs.wl-clipboard
  ];

  devToolsFor = rust: [
    pkgs.bashInteractive
    pkgs.coreutils
    pkgs.findutils
    pkgs.git
    pkgs.gnumake
    pkgs.neovim
    pkgs.pkg-config
    sessionSwitcherPython
    pkgs.ripgrep
    rust
    pkgs.wl-clipboard
  ];

  fullDevExtras = [
    pkgs.act
    pkgs.actionlint
    pkgs.docker
    pkgs.docker-buildx
    pkgs.firefox
    pkgs.go
    pkgs.jq
  ];

  wasmTools = [ pkgs.wasm-tools ];
  wasi32Cc = pkgs.pkgsCross.wasi32.stdenv.cc;
  wasi32Binutils = wasi32Cc.bintools;

  smokeTools = [
    dbusDaemonForDev
    pkgs.dbus
    pkgs.foot
    pkgs.gobject-introspection
    pkgs.glmark2
    pkgs.grim
    pkgs.imagemagick
    pkgs.jq
    pkgs.mesa-demos
    pkgs.nix
    pkgs.procps
    pkgs.sway
    pkgs.speedcrunch
    pkgs.vulkan-tools
    pkgs.wayland-scanner
    pkgs.wayland-protocols
    pkgs.wayland-utils
    pkgs.wlr-protocols
    pkgs.wtype
    westonWithSimpleClients
  ];
in
{
  inherit
    appTools
    fullDevExtras
    gtkRuntime
    nativeLibs
    rustToolchain
    rustToolchainWithTools
    sessionSwitcherPython
    smokeTools
    wasmTools
    wasi32Binutils
    wasi32Cc
    westonWithSimpleClients
    ;

  devTools = devToolsFor rustToolchain ++ wasmTools;
  devToolsWithTools = devToolsFor rustToolchainWithTools ++ wasmTools;
}
