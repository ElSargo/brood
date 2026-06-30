{ pkgs }:

let
  rustToolchain = pkgs.rust-bin.stable.latest.default.override {
    targets = [ "wasm32-unknown-unknown" ];
  };

  rustToolchainWithTools = pkgs.rust-bin.stable.latest.default.override {
    extensions = [
      "clippy"
      "rust-analyzer"
      "rust-src"
      "rustfmt"
    ];
    targets = [ "wasm32-unknown-unknown" ];
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
    pkgs.docker
    pkgs.docker-buildx
    pkgs.firefox
    pkgs.go
    pkgs.jq
  ];

  smokeTools = [
    pkgs.dbus
    pkgs.foot
    pkgs.gobject-introspection
    pkgs.glmark2
    pkgs.mesa-demos
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
    westonWithSimpleClients
    ;

  devTools = devToolsFor rustToolchain;
  devToolsWithTools = devToolsFor rustToolchainWithTools;
}
