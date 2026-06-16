{
  description = "moon — task runner & monorepo management (launis fork: Ruby tier-3 + jj-aware VCS)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, crane }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Pinned by rust-toolchain.toml (edition 2024, 1.96.0). The wasm target
        # is included so the same toolchain can also build the in-repo plugins.
        rustToolchain = pkgs.rust-bin.stable."1.96.0".default.override {
          targets = [ "wasm32-wasip1" ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Build only the `moon` binary. NOT cleanCargoSource: moon's build
        # embeds non-Rust assets (proto/*.proto, templates/*.tera, res/*.wasm
        # via include_bytes!), so the full tree must be present in the sandbox.
        commonArgs = {
          src = ./.;
          pname = "moon";
          version = "2.3.3";
          strictDeps = true;
          doCheck = false;
          cargoExtraArgs = "-p moon_cli --bin moon";
          nativeBuildInputs = with pkgs; [ pkg-config protobuf ];
          buildInputs = with pkgs; [ openssl ]
            ++ lib.optionals stdenv.isDarwin [ libiconv ];
          OPENSSL_NO_VENDOR = "1";
        };

        moonBin = craneLib.buildPackage (commonArgs // {
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        });
      in
      {
        packages = {
          moon = moonBin;
          default = moonBin;
        };

        apps.default = {
          type = "app";
          program = "${moonBin}/bin/moon";
        };

        devShells.default = pkgs.mkShell {
          packages = [ moonBin rustToolchain ]
            ++ (with pkgs; [ pkg-config protobuf openssl ]);
        };
      });
}
