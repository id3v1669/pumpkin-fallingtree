{
  description = "FallingTree-style tree felling plugin for the Pumpkin Minecraft server (WASM, plugin ABI v0.2)";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      ...
    }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
          manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
        in
        {
          # The plugin component: result/lib/fallingtree.wasm
          default = rustPlatform.buildRustPackage {
            pname = manifest.name;
            inherit (manifest) version;
            src = pkgs.lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;

            # .cargo/config.toml selects wasm32-wasip2; the host-side unit tests run in checks.
            buildPhase = ''
              runHook preBuild
              cargo build --release --offline --target wasm32-wasip2
              runHook postBuild
            '';
            doCheck = false;
            # ELF strip does not understand wasm; cargo already strips (profile.release).
            dontStrip = true;
            installPhase = ''
              runHook preInstall
              install -Dm644 target/wasm32-wasip2/release/pumpkin_fallingtree.wasm $out/lib/fallingtree.wasm
              runHook postInstall
            '';
          };
        }
      );

      checks = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
        in
        {
          tests = rustPlatform.buildRustPackage {
            pname = "pumpkin-fallingtree-tests";
            version = "0";
            src = pkgs.lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;
            buildPhase = "true";
            checkPhase = ''
              cargo test --offline --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget}
            '';
            installPhase = "touch $out";
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        in
        {
          default = pkgs.mkShell {
            packages = [
              toolchain
              pkgs.wasm-tools
              pkgs.just
            ];
          };
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixfmt);
    };
}
