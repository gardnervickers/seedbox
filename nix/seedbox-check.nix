{ pkgs }:
# Keep the workspace dependency versions/lock, but only copy the coordinator
# and its two local dependencies. Guest/application edits cannot invalidate it.
pkgs.rustPlatform.buildRustPackage {
  pname = "seedbox-check";
  version = "0.1.0";
  src = pkgs.lib.fileset.toSource {
    root = ../.;
    fileset = pkgs.lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates/seedbox-check
      ../crates/seedbox-exec
      ../crates/seedbox-test-time
    ];
  };
  postPatch = ''
    substituteInPlace Cargo.toml \
      --replace-fail 'members = ["crates/*"]' 'members = ["crates/seedbox-check", "crates/seedbox-exec", "crates/seedbox-test-time"]' \
      --replace-fail 'default-members = ["crates/seedbox"]' 'default-members = ["crates/seedbox-check"]'
  '';
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
