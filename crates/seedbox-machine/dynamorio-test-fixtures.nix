{ pkgs }:
let
  runtime = import ./dynamorio.nix { inherit pkgs; };
  # Exact WhiteBox sources from PR317's JDK21 runtime fixture, fetched during
  # the Nix build. Runtime tests never retrieve or compile dependencies.
  whitebox = pkgs.fetchurl {
    url = "https://raw.githubusercontent.com/openjdk/jdk21u/jdk-21.0.12-ga/test/lib/jdk/test/whitebox/WhiteBox.java";
    sha256 = "94b4c9a481e764e3b72287a59a1f06987586205a5f2cc5e17c3577996949d287";
  };
  diagnosticCommand = pkgs.fetchurl {
    url = "https://raw.githubusercontent.com/openjdk/jdk21u/jdk-21.0.12-ga/test/lib/jdk/test/whitebox/parser/DiagnosticCommand.java";
    sha256 = "f807936a68041072ca4a8b217e279890e50cbbcca2874488ed11dea70e2952e7";
  };
in pkgs.stdenv.mkDerivation {
  pname = "seedbox-dynamorio-test-fixtures";
  version = "0.1.0";
  src = ./tests/dynamorio;
  nativeBuildInputs = with pkgs; [
    cmake ninja patchelf jdk25 jdk21 python313 go_1_27 nodejs_24
    beam28Packages.erlang luajit dotnet-sdk_10
  ];
  cmakeFlags = [
    "-DCMAKE_BUILD_TYPE=RelWithDebInfo"
    "-DDynamoRIO_DIR=${runtime}/cmake"
    "-DSEEDBOX_UNIX_STATIC_LIBC_DIR=${pkgs.glibc.static}/lib"
    "-DSEEDBOX_JDK21_HOME=${pkgs.jdk21}"
    "-DSEEDBOX_JDK25_HOME=${pkgs.jdk25}"
    "-DSEEDBOX_WHITEBOX_JAVA=${whitebox}"
    "-DSEEDBOX_WHITEBOX_PARSER_JAVA=${diagnosticCommand}"
  ];
  postInstall = ''
    # Clients use only this build's public core/extensions, including after
    # the build tree is removed. Applications retain their normal libc RPATH.
    for client in "$out"/lib/*.so; do
      patchelf --add-rpath '${runtime}/lib64/release:${runtime}/ext/lib64/release' "$client"
    done
    ln -s ${runtime} "$out/runtime"
    mkdir -p "$out/runtimes"
    ln -s ${pkgs.jdk25} "$out/runtimes/java"
    ln -s ${pkgs.jdk21} "$out/runtimes/java21"
    ln -s ${pkgs.python313} "$out/runtimes/python"
    ln -s ${pkgs.go_1_27} "$out/runtimes/go"
    ln -s ${pkgs.nodejs_24} "$out/runtimes/node"
    ln -s ${pkgs.dotnet-sdk_10} "$out/runtimes/dotnet"
    ln -s ${pkgs.beam28Packages.erlang} "$out/runtimes/erl"
    ln -s ${pkgs.luajit} "$out/runtimes/lua"
  '';
}
