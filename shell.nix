{ pkgs ? import <nixpkgs> { }
, pkgs-dev
, ...
}:
let
  linuxPkgs = with pkgs; lib.optional stdenv.isLinux (
    inotifyTools
  );
  macosPkgs = with pkgs; lib.optional stdenv.isDarwin (
    with darwin.apple_sdk.frameworks; [
      # macOS file watcher support
      CoreFoundation
      CoreServices
    ]
  );
  devPkgs = with pkgs; [
    ## rust
    (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)

    pkgs-dev.wasm-pack # v0.11.1
    pkgs-dev.binaryen

    # NodeJS
    nodejs-18_x
  ];
in
with pkgs;
mkShell {
  buildInputs = devPkgs;
}
