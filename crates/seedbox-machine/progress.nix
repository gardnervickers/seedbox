{ pkgs }:
let
  arm = pkgs.stdenv.hostPlatform.isAarch64;
  # Keep this SDK release and its hashes aligned with dynamorio.nix.
  # The bundle uses the SDK's preload library with the patched core and extensions.
  sdk = pkgs.fetchurl {
    url = "https://github.com/DynamoRIO/dynamorio/releases/download/release_11.3.0-1/DynamoRIO-${if arm then "AArch64-Linux" else "Linux"}-11.3.0.tar.gz";
    hash = if arm then "sha256-n/e3uHdovA0O2hjyJH5VS6sbPoxXbvxXmS87dcEoqGM=" else "sha256-rgIEnfXE2uuC1Ah6qg04v1jN0ms4+2BC3M8ny8Ms7tk=";
  };
  loader = if arm then "ld-linux-aarch64.so.1" else "ld-linux-x86-64.so.2";
  # Both architectures use the patched launcher, core and extensions: the
  # execute-only handoff lives in drrun and the core loader.
  runtime = import ./dynamorio.nix { inherit pkgs; };
in pkgs.runCommand "seedbox-progress.tar" {
  passthru = { inherit sdk; glibcSource = pkgs.glibc.src; };
  nativeBuildInputs = [ pkgs.stdenv.cc ] ++ (with pkgs.buildPackages; [ cmake ninja patchelf python3 ]);
} ''
  mkdir sdk bundle
  tar -xf ${sdk} -C sdk --strip-components=1
  cmake -G Ninja -S ${./guest/progress} -B build \
    -DCMAKE_SYSTEM_NAME=Linux -DCMAKE_SYSTEM_PROCESSOR=${pkgs.stdenv.hostPlatform.parsed.cpu.name} \
    -DCMAKE_C_COMPILER="$CC" -DCMAKE_CXX_COMPILER="$CXX" \
    -DDynamoRIO_DIR="$PWD/sdk/cmake" -DCMAKE_BUILD_TYPE=Release
  cmake --build build
  mkdir -p bundle/bin64 bundle/lib64/release bundle/ext/lib64/release bundle/lib
  cp sdk/bin64/drrun bundle/bin64/
  cp sdk/lib64/release/*.so bundle/lib64/release/
  cp sdk/ext/lib64/release/libdrmgr.so sdk/ext/lib64/release/libdrreg.so bundle/ext/lib64/release/
  cp ${runtime}/bin64/drrun bundle/bin64/drrun
  rm bundle/lib64/release/libdynamorio.so
  cp ${runtime}/lib64/release/libdynamorio.so bundle/lib64/release/
  rm bundle/ext/lib64/release/libdrmgr.so bundle/ext/lib64/release/libdrreg.so
  cp ${runtime}/ext/lib64/release/*.so bundle/ext/lib64/release/
  cp build/libseedbox_progress.so bundle/
  # Only the launcher needs libc. Its private loader/search path must never
  # override the application's libc, including musl and static images.
  cp -L ${pkgs.glibc}/lib/${loader} bundle/lib/
  for name in libc.so.6 libm.so.6 libdl.so.2 libpthread.so.0 librt.so.1; do
    cp -L ${pkgs.glibc}/lib/$name bundle/lib/
  done
  patchelf --set-interpreter /seedbox/progress/lib/${loader} \
    --set-rpath '$ORIGIN/../lib' bundle/bin64/drrun
  patchelf --set-rpath '$ORIGIN/lib64/release:$ORIGIN/ext/lib64/release' bundle/libseedbox_progress.so
  cp sdk/License.txt bundle/DynamoRIO-LICENSE.txt
  cp sdk/ACKNOWLEDGEMENTS bundle/DynamoRIO-ACKNOWLEDGEMENTS.txt
  tar -xf ${pkgs.glibc.src}
  cp glibc-*/COPYING.LIB bundle/glibc-COPYING.LIB
  python3 ${./guest/progress/package.py} bundle "$out"
''
