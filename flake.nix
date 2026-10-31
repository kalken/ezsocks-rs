{
  description = "Small SOCKS5 proxy with Happy Eyeballs";

  # Built with this pinned nixpkgs, not the user's, so that every machine
  # gets the same build and can download it from the binary cache.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in {
      packages = forAllSystems (pkgs: rec {
        ezsocks-rs = pkgs.callPackage ./package.nix { };
        default = ezsocks-rs;
      });
    };
}
