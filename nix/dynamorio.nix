{ pkgs, dynamorioSource ? null }:
let
  arm = pkgs.stdenv.hostPlatform.isAarch64;
in
pkgs.stdenv.mkDerivation {
  pname = "seedbox-dynamorio";
  # ARM builds the DynamoRIO fork (the flake's `dynamorio-arm` input): upstream
  # plus Seedbox's translator fixes and AArch64 IR fixes, as commits on its
  # master. x86 builds the source of its existing release with one patch;
  # nix/progress.nix fetches the matching SDK for each.
  version = if arm then "11.91-seedbox" else "11.3.0";
  src = if arm then
    (if dynamorioSource == null
     then throw "the ARM runtime needs the dynamorio-arm flake input"
     else dynamorioSource)
  else pkgs.fetchurl {
    url = "https://github.com/DynamoRIO/dynamorio/archive/refs/tags/release_11.3.0-1.tar.gz";
    hash = "sha256-aj7BN0bravXAY72wVB37ZbTJvS+hZTxC61RAMiBDLsI=";
  };
  patches = if arm then [ ] else [
    # The same translator syscall mark, in rbx.
    ../guest/progress/dynamorio-x86-translator-syscalls.patch
  ];
  # BUILD_EXT configures every extension, including drsyms whose bundled
  # elfutils submodule is absent from the release source tarball. We build
  # only drreg and its drmgr/drcontainers dependencies.
  postPatch = ''
    substituteInPlace ext/CMakeLists.txt \
      --replace-fail 'add_subdirectory(drsyms)' '# drsyms is not packaged'
    # The AArch64 codec generator and order check run Python scripts by
    # their /usr/bin/env shebangs when CMake's PYTHON3_EXECUTABLE is unset,
    # and the Linux build sandbox has no /usr/bin/env.
    patchShebangs make core/ir/aarch64
  '';
  nativeBuildInputs = with pkgs; [ cmake ninja perl python3 ];
  cmakeFlags = [
    "-DCMAKE_BUILD_TYPE=Release"
    # Upstream compiles C as gnu99 but probes types before setting that flag.
    # GCC 16 defaults to C23, whose built-in bool would misconfigure the core.
    # Select the C standard without adding C flags to upstream C++ probes.
    "-DCMAKE_C_STANDARD=99"
    # The AArch64 codec scripts use this spelling; find_package(Python3) sets
    # Python3_EXECUTABLE, so without it CMake would exec the scripts directly.
    "-DPYTHON3_EXECUTABLE=${pkgs.python3}/bin/python3"
    "-DBUILD_TESTS=OFF"
    "-DBUILD_SAMPLES=OFF"
    "-DBUILD_CLIENTS=OFF"
    "-DBUILD_EXT=ON"
    "-DBUILD_DOCS=OFF"
  ] ++ pkgs.lib.optionals (!arm) [
    # 11.3 probes bool before applying that standard unless try_compile
    # honours it (policy CMP0067).
    "-DCMAKE_POLICY_DEFAULT_CMP0067=NEW"
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
