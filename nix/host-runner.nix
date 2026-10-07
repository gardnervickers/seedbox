{ config, lib, pkgs, ... }:
let
  cfg = config.services.seedbox-host-runner;
  user = "seedbox-runner";
  root = "/var/lib/seedbox-host";
  # This copy is installed by Gardner's host rebuild. PRs cannot replace it.
  smokeSource = pkgs.runCommand "seedbox-host-smoke" { } ''
    mkdir -p $out/harness $out/examples $out/python
    cp -r ${../harness/smoke} $out/harness/smoke
    cp -r ${../examples/faults} $out/examples/faults
    cp -r ${../python/seedbox} $out/python/seedbox
  '';
  tools = with pkgs; [
    bash coreutils git util-linux python3 rustc cargo rustfmt clippy pkg-config
    stdenv.cc cpio gzip curl jq gnumake bc bison flex perl elfutils
    openssl skopeo umoci podman iproute2 go
  ];
  installedSmoke = pkgs.runCommand "seedbox-host-smoke-launcher" { } ''
    cp -r ${smokeSource} $out
    chmod -R u+w $out
    cp ${./host-smoke.py} $out/harness/smoke/runner.py
  '';
  finalConfig = pkgs.writeText "seedbox-host-installed-config.json" (builtins.toJSON {
    pairGuards = "${root}/locks";
    jobRoots = map (name: "${root}/jobs/${name}") instances;
    kernel = cfg.kernel;
    pinnedEngine = cfg.pinnedEngine;
    python = "${pkgs.python3}/bin/python3";
    smoke = "${installedSmoke}/harness/smoke/runner.py";
    busybox = "${pkgs.pkgsStatic.busybox}/bin/busybox";
    toolPath = lib.makeBinPath tools;
    systemctl = "${pkgs.systemd}/bin/systemctl";
    systemdRun = "${pkgs.systemd}/bin/systemd-run";
  });
  wrapper = pkgs.writeShellScriptBin "seedbox-host-cpu-lease" ''
    exec ${pkgs.python3}/bin/python3 -I ${./host-cpu-lease.py} ${finalConfig} "$@"
  '';
  instances = lib.genList (i: "seedbox-host-${toString (i + 1)}") cfg.maxParallel;
in {
  options.services.seedbox-host-runner = {
    enable = lib.mkEnableOption "owner-triggered Seedbox AMD PR checks";
    maxParallel = lib.mkOption {
      type = lib.types.ints.between 1 15;
      default = 2;
      description = "Runner services and simultaneous SMT pairs; excludes 0/16.";
    };
    tokenFile = lib.mkOption {
      type = lib.types.str;
      description = "Absolute runtime secret path, never a Nix path literal or store file.";
      example = "/run/secrets/seedbox-runner-token";
    };
    kernel = lib.mkOption {
      type = lib.types.str;
      description = "Absolute root-owned path to the accepted guest bzImage.";
    };
    pinnedEngine = lib.mkOption {
      type = lib.types.str;
      description = "Absolute root-owned stored pinned seedbox-vmm; do not rebuild it.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      { assertion = pkgs.stdenv.hostPlatform.system == "x86_64-linux";
        message = "Seedbox host runner requires the qualified AMD Linux KVM host."; }
      { assertion = lib.all (p: lib.hasPrefix "/" p) [ cfg.tokenFile cfg.kernel cfg.pinnedEngine ];
        message = "Seedbox runner token and stored artifact paths must be absolute strings."; }
      { assertion = !lib.hasPrefix "/nix/store/" cfg.tokenFile;
        message = "Never store the runner token in the Nix store."; }
    ];
    users.groups.${user} = { };
    users.users.${user} = {
      isSystemUser = true;
      group = user;
      home = root;
      createHome = true;
      shell = "${pkgs.shadow}/bin/nologin";
    };
    boot.kernelModules = [ "kvm-amd" "msr" ];
    systemd.tmpfiles.rules = [
      "d ${root}/jobs 0700 ${user} ${user} -"
      "d ${root}/locks 0700 ${user} ${user} -"
      "x /tmp/seedbox-msr-cpu-*.lock - - - -"
    ] ++ map (name: "d ${root}/jobs/${name} 0700 ${user} ${user} -") instances
      ++ lib.genList (cpu: "f /tmp/seedbox-msr-cpu-${toString cpu}.lock 0644 root root -") 32
      ++ lib.genList (i: "f ${root}/locks/active-${toString (i + 1)} 0644 root root -") 15;
    environment.systemPackages = [ wrapper ];
    security.sudo.extraRules = [{
      users = [ user ];
      commands = [
        { command = "${wrapper}/bin/seedbox-host-cpu-lease"; options = [ "NOPASSWD" ]; }
        { command = cfg.pinnedEngine; options = [ "NOPASSWD" ]; }
      ];
    }];
    systemd.services = lib.genAttrs (map (name: "github-runner-${name}") instances) (unit:
      let name = lib.removePrefix "github-runner-" unit;
      in { serviceConfig.ExecStopPost = [
        "+${wrapper}/bin/seedbox-host-cpu-lease reap ${root}/jobs/${name}"
      ]; });
    services.github-runners = lib.genAttrs instances (name: {
      enable = true;
      inherit name;
      url = "https://github.com/gardnervickers/seedbox";
      inherit (cfg) tokenFile;
      user = user;
      group = user;
      extraLabels = [ "seedbox-amd" ];
      nodeRuntimes = [ "node20" "node24" ];
      workDir = "${root}/runners/${name}";
      extraPackages = tools ++ [ pkgs.nix pkgs.sudo wrapper ];
      extraEnvironment = {
        HOST_RUNNER_JOBS = "${root}/jobs/${name}";
        HOST_RUNNER_LOCKS = "${root}/locks";
        HOST_RUNNER_CAP = toString cfg.maxParallel;
        HOST_RUNNER_PINNED_ENGINE = cfg.pinnedEngine;
        HOST_CHECK_CPU_WRAPPER = "${wrapper}/bin/seedbox-host-cpu-lease";
        SEEDBOX_KERNEL = cfg.kernel;
      };
      serviceOverrides = {
        # The stock runner sandbox prevents sudo, shared leases and rootless
        # builders. Keep the runner's writes scoped to its state/work area.
        NoNewPrivileges = false;
        PrivateUsers = false;
        PrivateTmp = false;
        PrivateDevices = false;
        ProtectClock = false;
        PrivateMounts = false;
        ProtectProc = "default";
        RestrictSUIDSGID = false;
        RestrictNamespaces = false;
        CapabilityBoundingSet = lib.mkForce [ "~" ];
        SystemCallFilter = lib.mkForce [ ];
        ReadWritePaths = [ root ];
        TimeoutStopSec = "90s";
        Restart = "always";
        RestartSec = "10s";
      };
    });
  };
}
