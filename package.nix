{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "ezsocks-rs";
  version = (lib.importTOML ./Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  meta = {
    description = "Small SOCKS5 proxy with Happy Eyeballs";
    homepage = "https://github.com/kalken/ezsocks-rs";
    license = lib.licenses.mit;
    mainProgram = "ezsocks";
    platforms = lib.platforms.unix;
  };
}
