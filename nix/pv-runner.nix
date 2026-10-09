{ config, lib, pkgs, modulesPath, nixpkgs, developmentShells, ... }:
let
  runtime = import ./runtime.nix { inherit pkgs; };
  kernel = import ./kernels.nix { inherit pkgs; };
  # All three workspaces are compiled by the portable prerequisite suite.
  vendor = pkgs.symlinkJoin {
    name = "seedbox-cargo-vendor";
    paths = map (lockFile: pkgs.rustPlatform.importCargoLock { inherit lockFile; }) [
      ../Cargo.lock ../crates/seedbox-vmm/Cargo.lock ../crates/seedbox-arm64/Cargo.lock
    ];
  };
in {
  imports = [ (modulesPath + "/virtualisation/digital-ocean-image.nix") ];
  system.stateVersion = "26.05";
  virtualisation.diskSize = 24576;
  # User data is JSON containing a single-use runner credential, never a NixOS rebuild.
  virtualisation.digitalOcean.rebuildFromUserData = false;
  virtualisation.digitalOcean.setSshKeys = false;
  services.openssh.enable = lib.mkForce false;
  services.do-agent.enable = false;
  networking.firewall.enable = true;
  nix.settings.experimental-features = [ "nix-command" "flakes" ];
  nix.settings.trusted-users = [ "root" ];
  nix.gc.automatic = false;
  virtualisation.podman.enable = true;
  systemd.sockets.podman.wantedBy = [ "sockets.target" ];
  environment.systemPackages = with pkgs; [
    bash coreutils curl git jq python3 nix github-runner gzip gnutar
  ];
  environment.etc."seedbox/nixpkgs".source = nixpkgs.outPath;
  environment.etc."seedbox/kernel".source = "${kernel.guestKernel}/bzImage";
  environment.etc."seedbox/runtime.tar.gz".source = runtime.runtimeArchive;
  environment.etc."seedbox/cargo-config.toml".text = ''
    [source.crates-io]
    replace-with = "seedbox-image"
    [source.seedbox-image]
    directory = "${vendor}"
    [net]
    offline = true
  '';
  # These are image contents, rooted by the system closure. No boot-time install.
  system.extraDependencies = [
    developmentShells.default developmentShells.assets vendor
    runtime.runtimeArchive kernel.guestKernel
  ];
  systemd.tmpfiles.rules = [
    "d /var/lib/seedbox-cargo 0700 root root -"
    "L+ /var/lib/seedbox-cargo/config.toml - - - - /etc/seedbox/cargo-config.toml"
  ];
  systemd.services.seedbox-runner = {
    description = "One GitHub Actions PV batch";
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" "digitalocean-metadata.service" "podman.socket" ];
    requires = [ "digitalocean-metadata.service" "podman.socket" ];
    wants = [ "network-online.target" ];
    path = config.environment.systemPackages ++ [ pkgs.kmod ];
    environment = {
      HOME = "/root";
      RUNNER_ROOT = "/var/lib/seedbox-runner";
      RUNNER_ALLOW_RUNASROOT = "1";
      CARGO_HOME = "/var/lib/seedbox-cargo";
      DOCKER_HOST = "unix:///run/podman/podman.sock";
      SEEDBOX_KERNEL = "/etc/seedbox/kernel";
      SEEDBOX_BUILD_HELPERS = "/etc/seedbox/runtime.tar.gz";
    };
    serviceConfig = {
      Type = "simple";
      StateDirectory = "seedbox-runner";
      WorkingDirectory = "/var/lib/seedbox-runner";
      Restart = "no";
      # Also stop execution locally if GitHub disappears. Destruction is external.
      RuntimeMaxSec = "110min";
    };
    script = ''
      set -euo pipefail
      modprobe kvm_intel 2>/dev/null || modprobe kvm_amd 2>/dev/null || true
      test -c /dev/kvm
      jit=$(jq -er '.user_data | fromjson | .jit' /run/do-metadata/v1.json)
      exec Runner.Listener run --jitconfig "$jit"
    '';
  };
}
