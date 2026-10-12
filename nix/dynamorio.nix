{ pkgs }:
let
  arm = pkgs.stdenv.hostPlatform.isAarch64;
in
pkgs.stdenv.mkDerivation {
  pname = "seedbox-dynamorio";
  # ARM needs a newer core for JITs. x86 builds the source of its existing
  # release; nix/progress.nix fetches the matching SDK for each.
  version = if arm then "11.91.20735" else "11.3.0";
  src = pkgs.fetchurl (if arm then {
    url = "https://github.com/DynamoRIO/dynamorio/archive/refs/tags/cronbuild-11.91.20735.tar.gz";
    hash = "sha256-VNzTsfM80ByTlTeHGTDqE5cKa48oAWiNgdZ6oKCpOZo=";
  } else {
    url = "https://github.com/DynamoRIO/dynamorio/archive/refs/tags/release_11.3.0-1.tar.gz";
    hash = "sha256-aj7BN0bravXAY72wVB37ZbTJvS+hZTxC61RAMiBDLsI=";
  });
  patches = if arm then [
    ../guest/progress/dynamorio-exclusives.patch
    # Rebuilding a block discards its region list and must recheck the start.
    ../guest/progress/dynamorio-rebuild-vmareas.patch
    # Clean calls without SVE must save each vector register in its own
    # context slot, where dr_get_mcontext() and redirection read it.
    ../guest/progress/dynamorio-cleancall-simd.patch
    # The translator marks its own syscalls so the guest charges them less.
    ../guest/progress/dynamorio-translator-syscalls.patch
    # A vfork child's handler copy must not make later signals to
    # initializing threads fatal ("multiple native handlers").
    ../guest/progress/dynamorio-vfork-handlers.patch
    # A thread forking during another thread's stop-all-threads flush must be
    # resumed in place; redirecting it skipped the fork syscall.
    ../guest/progress/dynamorio-fork-synch.patch
    # A vfork parent cannot take the suspend signal until its child execs, so
    # stop-all-threads synchronization counts it as stopped instead of waiting.
    ../guest/progress/dynamorio-vfork-synch.patch
    # Signals that arrive while a thread is suspended must still reach it when
    # it resumes into a blocking syscall.
    ../guest/progress/dynamorio-suspend-pending.patch
    # A signal that interrupts an application thread waiting at a synch-safe
    # spot is delayed; running its handler at once skipped the wait's
    # epilogue and left the thread marked safe while it ran in the cache.
    ../guest/progress/dynamorio-safe-spot-signals.patch
    # Signal emulation gaps found by the native corpus probes: SS_AUTODISARM,
    # signalfd's queued value, signals sent to a fork child before it
    # initialized, and clone3's CLONE_CLEAR_SIGHAND.
    ../guest/progress/dynamorio-sigaltstack-autodisarm.patch
    ../guest/progress/dynamorio-signalfd-value.patch
    ../guest/progress/dynamorio-fork-pending-signals.patch
    ../guest/progress/dynamorio-clone-clear-sighand.patch
    # A signal on the indirect-branch lookup's hit path must still find the
    # fragment to unlink, or a thread looping through indirect branches never
    # receives it.
    ../guest/progress/dynamorio-ibl-hit-signals.patch
  ] else [
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
