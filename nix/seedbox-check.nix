{ pkgs }:
# The real workspace/lock, selecting the isolated coordinator package.
# Parser dependencies never enter the lightweight seedbox-test helper library.
pkgs.rustPlatform.buildRustPackage {
  pname = "seedbox-check";
  version = "0.1.0";
  src = pkgs.lib.cleanSourceWith {
    src = ../.;
    # The Rust checker does not consume the translator sources.
    filter = path: type: path != toString ../vendor && pkgs.lib.cleanSourceFilter path type;
  };
  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [ "-p" "seedbox-check" "--bin" "seedbox-check" ];
  # This tool needs neither the application's Python signing linker nor HVF.
  # Use the ordinary Nix compiler wrapper for this package's Darwin output.
  CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER = "${pkgs.stdenv.cc}/bin/cc";
  doCheck = false; # Focused tests are run from the ordinary workspace separately.
  installPhase = ''
    runHook preInstall
    binary=$(find target -type f -name seedbox-check -perm -0100 -not -path "*/deps/*")
    test -n "$binary"
    install -Dm755 "$binary" "$out/bin/seedbox-check"
    runHook postInstall
  '';
}
