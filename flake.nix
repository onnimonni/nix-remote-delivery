{
  description = "Deploy NixOS by syncing source to remote servers and building there";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    let
      overlay = final: prev: {
        nix-remote-delivery = final.rustPlatform.buildRustPackage {
          pname = "nix-remote-delivery";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          doCheck = true;
          meta.mainProgram = "nix-remote-delivery";
        };
      };
    in
    {
      overlays.default = overlay;
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ overlay ];
        };
        package = pkgs.nix-remote-delivery;
      in
      {
        packages.default = package;
        packages.nix-remote-delivery = package;

        apps.default = self.apps.${system}.nix-remote-delivery;
        apps.nix-remote-delivery = {
          type = "app";
          program = "${package}/bin/nix-remote-delivery";
          meta.description = "Deploy NixOS by syncing source to remote servers and building there";
        };

        checks.default = package;
        formatter = pkgs.nixfmt;
      }
    );
}
