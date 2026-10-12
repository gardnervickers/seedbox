{ pkgs }:
pkgs.stdenv.mkDerivation {
  pname = "seedbox-dynamorio";
  version = "11.91.20735";
  src = pkgs.fetchurl {
    url = "https://github.com/DynamoRIO/dynamorio/archive/refs/tags/cronbuild-11.91.20735.tar.gz";
    hash = "sha256-VNzTsfM80ByTlTeHGTDqE5cKa48oAWiNgdZ6oKCpOZo=";
  };
  patches = [
    ../guest/progress/dynamorio-exclusives.patch
    # Rebuilding a block discards its region list and must recheck the start.
    ../guest/progress/dynamorio-rebuild-vmareas.patch
    # Clean calls without SVE must save each vector register in its own
    # context slot, where dr_get_mcontext() and redirection read it.
    ../guest/progress/dynamorio-cleancall-simd.patch
    # Eager IC invalidation, including multi-page and open-top query consumers.
    ../guest/progress/dynamorio-ic-ivau.patch
    # SME-only Linux can supply an inactive, header-only SVE record with VL=0.
    ../guest/progress/dynamorio-sme-empty-context.patch
    # Opt-in correctness discriminator only; the normal launch recipe does not
    # enable its all-image scan or change the existing retirement protocol.
    ../guest/progress/dynamorio-icache-images.patch
    # Stored-info Debug validation walks the retained cache, not singleton IR.
    ../guest/progress/dynamorio-translation-lookahead.patch
    # Transferable, restartable predicate wait with an owned app context.
    ../guest/progress/dynamorio-predicate-wait.patch
    # Fixed, thread-owned progress debt completed off the code cache.
    ../guest/progress/dynamorio-owned-progress.patch
    # Thread-exit TLS validation needs permissions, not a faulting ELF probe.
    ../guest/progress/dynamorio-tls-permission-query.patch
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
