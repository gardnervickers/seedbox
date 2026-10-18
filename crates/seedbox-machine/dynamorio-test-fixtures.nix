{ pkgs }:
let
  runtime = import ./dynamorio.nix { inherit pkgs; };
  # Same public release SDK used by the production progress client. This is
  # only a build dependency: execution uses the current submodule's runtime.
  sdk = (import ./progress.nix { inherit pkgs; }).sdk;
in pkgs.stdenv.mkDerivation {
  pname = "seedbox-dynamorio-test-fixtures";
  version = "0.1.0";
  src = ./tests/dynamorio;
  nativeBuildInputs = with pkgs; [ cmake ninja patchelf ];
  # The release SDK is an archive, not a directory.
  preConfigure = ''
    mkdir sdk
    tar -xf ${sdk} -C sdk --strip-components=1
    cmakeFlags+=("-DDynamoRIO_DIR=$PWD/sdk/cmake")
  '';
  installPhase = ''
    mkdir -p "$out/bin" "$out/lib"
    cp application "$out/bin/"
    cp libpreserve.so "$out/lib/"
    patchelf --set-rpath '${runtime}/lib64/release:${runtime}/ext/lib64/release' "$out/lib/libpreserve.so"
    ln -s ${runtime} "$out/runtime"
  '';
}
