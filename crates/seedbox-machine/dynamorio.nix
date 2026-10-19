{ pkgs, debug ? false }:
let
  source = ../../vendor/dynamorio;
in
assert pkgs.lib.assertMsg (builtins.pathExists (source + "/CMakeLists.txt"))
  "DynamoRIO submodule missing; run git submodule update --init --recursive vendor/dynamorio from the Seedbox checkout.";
pkgs.stdenv.mkDerivation {
  pname = "seedbox-dynamorio${pkgs.lib.optionalString debug "-debug"}";
  version = "11.3.0-1";
  src = pkgs.lib.cleanSource source;
  # Export the public SDK and runtime from one configuration. drsyms needs
  # optional elfutils sources and is not used by the packaged clients.
  postPatch = ''
    substituteInPlace ext/CMakeLists.txt \
      --replace-fail 'add_subdirectory(drsyms)' '# drsyms is not packaged'
    # This baseline clears CMAKE_C_FLAGS before CheckTypeSize, then builds C
    # as gnu99. Preserve the selected C dialect for those real probes too;
    # CMAKE_REQUIRED_FLAGS would also pollute the C++17 probe.
    substituteInPlace CMakeLists.txt \
      --replace-fail 'foreach (var CMAKE_C_FLAGS;CMAKE_CXX_FLAGS)' 'foreach (var CMAKE_CXX_FLAGS)'
    # The AArch64 codec generator and order check run Python scripts by
    # their /usr/bin/env shebangs when CMake's PYTHON3_EXECUTABLE is unset,
    # and the Linux build sandbox has no /usr/bin/env.
    patchShebangs make core/ir/aarch64
  '';
  nativeBuildInputs = with pkgs.buildPackages; [ cmake ninja perl python3 ];
  cmakeFlags = [
    "-DCMAKE_BUILD_TYPE=${if debug then "Debug" else "RelWithDebInfo"}"
    "-DREADELF_EXECUTABLE=${pkgs.stdenv.cc.bintools}/bin/${pkgs.stdenv.cc.targetPrefix}readelf"
    "-DCMAKE_ASM_COMPILER=${pkgs.stdenv.cc.bintools}/bin/${pkgs.stdenv.cc.targetPrefix}as"
    # Upstream compiles C as gnu99 but probes types before setting that flag.
    # GCC 16 defaults to C23, whose built-in bool would misconfigure the core.
    "-DCMAKE_C_FLAGS=-std=gnu99"
    # The AArch64 codec scripts use this spelling; find_package(Python3) sets
    # Python3_EXECUTABLE, so without it CMake would exec the scripts directly.
    "-DPYTHON3_EXECUTABLE=${pkgs.buildPackages.python3}/bin/python3"
    "-DBUILD_TESTS=OFF"
    "-DBUILD_SAMPLES=OFF"
    "-DBUILD_CLIENTS=OFF"
    "-DBUILD_EXT=ON"
    "-DBUILD_DOCS=OFF"
  ];
  # Install the generated SDK export alongside every library it references;
  # consumers must never combine release archive headers with this source ABI.
  installPhase = ''
    runHook preInstall
    cmake --install .
    runHook postInstall
  '';
}
