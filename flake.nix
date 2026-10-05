{
  description = "Seedbox deterministic execution sandbox in Rust";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      eachSystem = f: nixpkgs.lib.genAttrs systems (system: f (import nixpkgs { inherit system; }));
      sourceFor = pkgs: pkgs.lib.cleanSourceWith {
        src = ./.;
        filter = path: type:
          let name = baseNameOf path; in
          !(builtins.elem name [ "target" "build" "artifacts" "reference" ".direnv" ".pytest_cache" "__pycache__" ".venv" "result" ])
          && pkgs.lib.cleanSourceFilter path type;
      };
    in {
      devShells = eachSystem (pkgs: {
        default = pkgs.mkShell {
          packages = [ pkgs.rustc pkgs.cargo pkgs.rustfmt pkgs.clippy pkgs.pkg-config pkgs.unicorn pkgs.llvmPackages.libclang pkgs.nasm pkgs.lld ];
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
        };
      });
      packages = eachSystem (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "seedbox";
          version = "0.2.0";
          src = sourceFor pkgs;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.pkg-config pkgs.llvmPackages.libclang pkgs.nasm pkgs.lld pkgs.rustfmt pkgs.clippy ];
          buildInputs = [ pkgs.unicorn ];
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          preCheck = ''cargo fmt -- --check'';
          postCheck = ''cargo clippy --offline --locked --release --all-targets -- -D warnings'';
          meta.mainProgram = "seedbox";
        };
      } // pkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
        redis-fixtures = import ./redis-fixtures.nix { inherit pkgs; };
      });
      checks = eachSystem (pkgs: {
        execution = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      } // pkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
        redis = self.packages.x86_64-linux.default.overrideAttrs (previous: {
          postCheck = previous.postCheck + ''
            export SEEDBOX_REDIS_FIXTURES=${self.packages.x86_64-linux.redis-fixtures}/bin
            cargo test --offline --locked --release --test redis -- --ignored
          '';
        });
      });
    };
}
