{
    inputs = {
        nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/*.tar.gz";
        rust-overlay = {
            url = "github:oxalica/rust-overlay";
            inputs.nixpkgs.follows = "nixpkgs";
        };
    };
    outputs = attrs: let
        supportedSystems = [
            "aarch64-darwin"
            "aarch64-linux"
            "x86_64-darwin"
            "x86_64-linux"
        ];
        forEachSupportedSystem = f: attrs.nixpkgs.lib.genAttrs supportedSystems (system: f {
            pkgs = import attrs.nixpkgs {
                inherit system;
                overlays = [
                    attrs.rust-overlay.overlays.default
                ];
            };
        });
    in {
        devShells = forEachSupportedSystem({ pkgs, ... }: {
            pre-commit = pkgs.mkShell {
                packages = with pkgs; [
                    cargo-deny
                    rust-bin.nightly.latest.default # nightly cargo, required to run the pre-commit script
                ];
            };
        });
        packages = forEachSupportedSystem ({ pkgs, ... }: let
            manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
        in {
            default = pkgs.rustPlatform.buildRustPackage {
                cargoLock = {
                    allowBuiltinFetchGit = true; # allows omitting cargoLock.outputHashes
                    lockFile = ./Cargo.lock;
                };
                pname = "wurstminebackup";
                src = ./.;
                version = manifest.version;
            };
        });
    };
}
