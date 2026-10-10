{ pkgs }:
pkgs.stdenv.mkDerivation {
  pname = "seedbox-dynamorio";
  version = "11.3.0-1";
  src = pkgs.fetchurl {
    url = "https://github.com/DynamoRIO/dynamorio/archive/refs/tags/release_11.3.0-1.tar.gz";
    hash = "sha256-aj7BN0bravXAY72wVB37ZbTJvS+hZTxC61RAMiBDLsI=";
  };
  patches = [ ../guest/progress/dynamorio-exclusives.patch ];
  # BUILD_EXT configures every extension, including drsyms whose bundled
  # elfutils submodule is absent from the release source tarball. We build
  # only drreg and its drmgr/drcontainers dependencies.
  postPatch = ''
    substituteInPlace ext/CMakeLists.txt \
      --replace-fail 'add_subdirectory(drsyms)' '# drsyms is not packaged'
    # The AArch64 codec order check executes codecsort.py by its
    # /usr/bin/env shebang, which the Linux build sandbox does not provide.
    patchShebangs core/ir/aarch64/codecsort.py
  '';
  nativeBuildInputs = with pkgs; [ cmake ninja perl python3 ];
  cmakeFlags = [
    "-DCMAKE_BUILD_TYPE=Release"
    # Upstream compiles C as gnu99 but probes types before setting that flag.
    # GCC 16 defaults to C23, whose built-in bool would misconfigure the core.
    "-DCMAKE_REQUIRED_FLAGS=-std=gnu99"
    "-DBUILD_TESTS=OFF"
    "-DBUILD_SAMPLES=OFF"
    "-DBUILD_CLIENTS=OFF"
    "-DBUILD_EXT=ON"
    "-DBUILD_DOCS=OFF"
  ];
  ninjaFlags = [ "dynamorio" "drreg" ];
  installPhase = ''
    runHook preInstall
    mkdir -p "$out/lib64/release"
    cp lib64/release/libdynamorio.so "$out/lib64/release/"
    mkdir -p "$out/ext/lib64/release"
    cp ext/lib64/release/libdrmgr.so ext/lib64/release/libdrreg.so "$out/ext/lib64/release/"
    runHook postInstall
  '';
}
