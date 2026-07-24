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

  appTools = [
    pkgs.coreutils
    pkgs.findutils
    pkgs.foot
    pkgs.git
    sessionSwitcherPython
    pkgs.util-linux
  ];

  devToolsFor = rust: [
    pkgs.bashInteractive
    pkgs.coreutils
    pkgs.findutils
    pkgs.git
    pkgs.pkg-config
    sessionSwitcherPython
    pkgs.ripgrep
    rust
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

  smokeTools = [
    pkgs.dbus
    pkgs.foot
    pkgs.gobject-introspection
    pkgs.glmark2
    pkgs.grim
    pkgs.mesa-demos
    pkgs.sway
    pkgs.vulkan-tools
    pkgs.wayland-utils
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
    westonWithSimpleClients
    ;

  devTools = devToolsFor rustToolchain ++ wasmTools;
  devToolsWithTools = devToolsFor rustToolchainWithTools ++ wasmTools;
}
