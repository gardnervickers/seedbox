# Build a DynamoRIO variant from Seedbox's recipe (nix/dynamorio.nix) with an
# explicit patch list and build type. Used only by build/jvm-crash/repro.
{ name, patches, buildType ? "Release" }:
let
  lock = builtins.fromJSON (builtins.readFile ./flake.lock);
  source = builtins.fetchTree lock.nodes.nixpkgs.locked;
  pkgs = import source.outPath { system = "aarch64-linux"; };
  base = import ./nix/dynamorio.nix { inherit pkgs; };
in base.overrideAttrs (old: {
  pname = "seedbox-dynamorio-${name}";
  patches = map (p: ./. + "/${p}") patches;
  cmakeFlags = map (f: if f == "-DCMAKE_BUILD_TYPE=Release" then "-DCMAKE_BUILD_TYPE=${buildType}" else f) old.cmakeFlags;
  installPhase = ''
    runHook preInstall
    mkdir -p "$out/ext"
    cp -r lib64 "$out/"
    cp -r ext/lib64 "$out/ext/"
    runHook postInstall
  '';
})
