{
  description = "Dummy Brood nix develop runner project";

  inputs.brood.url = "git+file:../..";

  outputs =
    { self, brood }:
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
        default = brood.devShells.${system}.default.overrideAttrs (old: {
          BROOD_DUMMY_NIX_ENV = "active";
          BROOD_DUMMY_PROJECT_NAME = "session-runner-nix-project";
          shellHook = (old.shellHook or "") + ''
            export BROOD_DUMMY_NIX_ENV=active
            export BROOD_DUMMY_PROJECT_NAME=session-runner-nix-project
          '';
        });
      });
    };
}
