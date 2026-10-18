# Offline aggregate output consumed by machine::{pv,tcg}::Factory.
# All inputs are already-built guest files. Host worker/QEMU executables stay separate.
{ pkgs, kernel, init, commandLine, progress ? null, firmware ? null }:
assert (progress != null) != (firmware != null);
let
  pv = progress != null;
  arguments = if pv then "--progress ${progress}" else "--firmware ${firmware}";
in pkgs.runCommand "seedbox-machine-${if pv then "pv" else "tcg"}-guest-assets" {
  nativeBuildInputs = [ pkgs.python3 ];
} ''
  python3 ${./guest-assets.py} ${if pv then "pv" else "tcg"} \
    --kernel ${kernel} --init ${init} \
    --command-line-file ${pkgs.writeText "kernel-command-line" commandLine} \
    ${arguments} --output "$out"
''
