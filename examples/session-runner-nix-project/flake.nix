{
  description = "Dummy Manifold nix develop runner project";

  inputs.manifold.url = "git+file:../..";

  outputs =
    { self, manifold }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forEachSystem =
        f:
        builtins.listToAttrs (
          map (system: {
            name = system;
            value = f system;
          }) systems
        );
    in
    {
      devShells = forEachSystem (system: {
        default = manifold.devShells.${system}.default.overrideAttrs (old: {
          MANIFOLD_DUMMY_NIX_ENV = "active";
          MANIFOLD_DUMMY_PROJECT_NAME = "session-runner-nix-project";
          shellHook = (old.shellHook or "") + ''
            export MANIFOLD_DUMMY_NIX_ENV=active
            export MANIFOLD_DUMMY_PROJECT_NAME=session-runner-nix-project
          '';
        });
      });
    };
}
