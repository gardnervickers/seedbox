{ pkgs }:
let
  arm = pkgs.stdenv.hostPlatform.isAarch64;
  # ARM uses the matching SDK launcher/preload libraries with the patched core
  # from nix/dynamorio.nix. Keep its version and hash aligned with that source.
  # The x86 runtime remains on its existing release; this upgrade fixes ARM JITs.
  sdkTag = if arm then "cronbuild-11.91.20735" else "release_11.3.0-1";
  sdkVersion = if arm then "11.91.20735" else "11.3.0";
  sdk = pkgs.fetchurl {
    url = "https://github.com/DynamoRIO/dynamorio/releases/download/${sdkTag}/DynamoRIO-${if arm then "AArch64-Linux" else "Linux"}-${sdkVersion}.tar.gz";
    hash = if arm then "sha256-cMcDq/M5+XjvOcbPkMNtdRCiiYG7gGpGuj7AvqrXmto=" else "sha256-rgIEnfXE2uuC1Ah6qg04v1jN0ms4+2BC3M8ny8Ms7tk=";
  };
  loader = if arm then "ld-linux-aarch64.so.1" else "ld-linux-x86-64.so.2";
  armRuntime = import ./dynamorio.nix { inherit pkgs; };
in pkgs.runCommand "seedbox-progress.tar" {
  passthru = { inherit sdk; glibcSource = pkgs.glibc.src; };
  nativeBuildInputs = with pkgs; [ stdenv.cc cmake ninja patchelf python3 ];
} ''
  mkdir sdk bundle
  tar -xf ${sdk} -C sdk --strip-components=1
  ${pkgs.lib.optionalString arm ''
    cp -R ${armRuntime}/include/. sdk/include/
  ''}
  cmake -G Ninja -S ${../guest/progress} -B build \
    -DDynamoRIO_DIR="$PWD/sdk/cmake" -DCMAKE_BUILD_TYPE=Release
  cmake --build build
  mkdir -p bundle/bin64 bundle/lib64/release bundle/ext/lib64/release bundle/lib
  cp sdk/bin64/drrun bundle/bin64/
  cp sdk/lib64/release/*.so bundle/lib64/release/
  cp sdk/ext/lib64/release/libdrmgr.so sdk/ext/lib64/release/libdrreg.so bundle/ext/lib64/release/
  ${pkgs.lib.optionalString arm ''
    rm bundle/lib64/release/libdynamorio.so
    cp ${armRuntime}/lib64/release/libdynamorio.so bundle/lib64/release/
    rm bundle/ext/lib64/release/libdrmgr.so bundle/ext/lib64/release/libdrreg.so
    cp ${armRuntime}/ext/lib64/release/*.so bundle/ext/lib64/release/
  ''}
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
  python3 ${../guest/progress/package.py} bundle "$out"
''
