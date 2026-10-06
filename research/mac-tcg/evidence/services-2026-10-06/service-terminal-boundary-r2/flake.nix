{
  description = "Seedbox local Mac TCG input-control probe";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  outputs = { nixpkgs, ... }: {
    devShells = nixpkgs.lib.genAttrs [ "aarch64-darwin" "x86_64-linux" ] (system:
      let pkgs = import nixpkgs { inherit system; }; in {
        default = pkgs.mkShell {
          packages = with pkgs; [ qemu python3 zig clang pkg-config glib.dev ];
        };
      });
  };
}
