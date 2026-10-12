# Build the scratch "park" DynamoRIO client against the stock DR SDK, like
# nix/progress.nix builds the progress client. Used only by build/jvm-crash/repro.
let
  lock = builtins.fromJSON (builtins.readFile ./flake.lock);
  source = builtins.fetchTree lock.nodes.nixpkgs.locked;
  pkgs = import source.outPath { system = "aarch64-linux"; };
  sdk = pkgs.fetchurl {
    url = "https://github.com/DynamoRIO/dynamorio/releases/download/cronbuild-11.91.20735/DynamoRIO-AArch64-Linux-11.91.20735.tar.gz";
    hash = "sha256-cMcDq/M5+XjvOcbPkMNtdRCiiYG7gGpGuj7AvqrXmto=";
  };
in pkgs.runCommand "repro-park-client" {
  nativeBuildInputs = with pkgs; [ stdenv.cc cmake ninja patchelf ];
} ''
  mkdir sdk src
  tar -xf ${sdk} -C sdk --strip-components=1
  cp ${./build/jvm-crash/repro/park.c} src/park.c
  cp ${./build/jvm-crash/repro/park-CMakeLists.txt} src/CMakeLists.txt
  cmake -G Ninja -S src -B build -DDynamoRIO_DIR="$PWD/sdk/cmake" -DCMAKE_BUILD_TYPE=Release
  cmake --build build
  mkdir -p $out
  cp build/librepro_park.so $out/
  # Resolved against whichever bundle run.sh mounts at /seedbox/progress.
  patchelf --set-rpath /seedbox/progress/lib64/release $out/librepro_park.so
''
