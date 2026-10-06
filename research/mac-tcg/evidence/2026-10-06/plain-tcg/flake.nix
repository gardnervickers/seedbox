{
  description = "Seedbox local Mac TCG input-control probe";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  outputs = { nixpkgs, ... }: {
    devShells = nixpkgs.lib.genAttrs [ "aarch64-darwin" "x86_64-linux" ] (system:
      let pkgs = import nixpkgs { inherit system; }; in {
        default = pkgs.mkShell ({
          packages = with pkgs; [ qemu python3 zig ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgsStatic.stdenv.cc ];
        } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          SEEDBOX_STATIC_CC = "${pkgs.pkgsStatic.stdenv.cc}/bin/x86_64-unknown-linux-musl-cc";
        });
      });
  };
}
