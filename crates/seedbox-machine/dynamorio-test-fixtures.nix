{ pkgs }:
let
  runtime = import ./dynamorio.nix { inherit pkgs; };
in pkgs.stdenv.mkDerivation {
  pname = "seedbox-dynamorio-test-fixtures";
  version = "0.1.0";
  src = ./tests/dynamorio;
  nativeBuildInputs = with pkgs; [ cmake ninja patchelf jdk25 python3 go nodejs erlang lua dotnet-sdk ];
  cmakeFlags = [
    "-DCMAKE_BUILD_TYPE=RelWithDebInfo"
    "-DDynamoRIO_DIR=${runtime}/cmake"
  ];
  postInstall = ''
    # Clients use only this build's public core/extensions, including after
    # the build tree is removed. Applications retain their normal libc RPATH.
    for client in "$out"/lib/*.so; do
      patchelf --set-rpath '${runtime}/lib64/release:${runtime}/ext/lib64/release' "$client"
    done
    ln -s ${runtime} "$out/runtime"
    mkdir -p "$out/runtimes"
    ln -s ${pkgs.jdk25} "$out/runtimes/java"
    ln -s ${pkgs.python3} "$out/runtimes/python"
    ln -s ${pkgs.go} "$out/runtimes/go"
    ln -s ${pkgs.nodejs} "$out/runtimes/node"
    ln -s ${pkgs.dotnet-sdk} "$out/runtimes/dotnet"
    ln -s ${pkgs.erlang} "$out/runtimes/erl"
    ln -s ${pkgs.lua} "$out/runtimes/lua"
  '';
}
