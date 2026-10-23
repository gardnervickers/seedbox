# One flake evaluation and one named output for offline asset preparation.
# --impure permits the explicit live-checkout flake reference supplied by xtask;
# package dependencies still come from that flake's lock file.
{ flake, names }:
let
  project = builtins.getFlake flake;
  pkgs = import project.inputs.nixpkgs { system = builtins.currentSystem; };
  packages = project.packages.${builtins.currentSystem};
in pkgs.linkFarm "seedbox-build-inputs" (map (name: {
  inherit name;
  path = packages.${name};
}) names)
