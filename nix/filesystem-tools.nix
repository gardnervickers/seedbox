{ pkgs }:
let
  # mke2fs needs UUID/block probing libraries, not util-linux's login tools.
  # Use e2fsprogs' bundled implementations for this static guest formatter.
  filesystemTools = (pkgs.pkgsStatic.e2fsprogs.override {
    libuuid = null;
    withFuse = false;
  }).overrideAttrs (old: {
    configureFlags = map (flag:
      if flag == "--disable-libuuid" then "--enable-libuuid"
      else if flag == "--disable-libblkid" then "--enable-libblkid"
      else flag
    ) old.configureFlags;
  });
in {
  inherit filesystemTools;
}
