let
  zynq-rs = (import ./zynq-rs.nix);
  pkgs = import <nixpkgs> { overlays = [ (import "${zynq-rs}/nix/mozilla-overlay.nix") ]; };
  rustPlatform = (import "${zynq-rs}/nix/rust-platform.nix" { inherit pkgs; });
  cargo-xbuild = (import zynq-rs).cargo-xbuild;
  artiq-fast = <artiq-fast>;
  artiqpkgs = import "${artiq-fast}/default.nix" { inherit pkgs; };
  vivado = import "${artiq-fast}/vivado.nix" { inherit pkgs; };
  zc706-szl = (import zynq-rs).zc706-szl;
in
  pkgs.stdenv.mkDerivation {
    name = "artiq-zynq-env";
    buildInputs = [
      pkgs.gnumake
      rustPlatform.rust.rustc
      rustPlatform.rust.cargo
      pkgs.llvmPackages_9.llvm
      pkgs.llvmPackages_9.clang-unwrapped
      pkgs.cacert
      cargo-xbuild

      pkgs.openocd
      pkgs.openssh pkgs.rsync

      (pkgs.python3.withPackages(ps: (with artiqpkgs; [ migen migen-axi misoc artiq artiq-netboot ])))
      vivado
      artiqpkgs.binutils-arm

      (import "${zynq-rs}/nix/mkbootimage.nix" { inherit pkgs; })
    ];

    XARGO_RUST_SRC = "${rustPlatform.rust.rustc}/lib/rustlib/src/rust/library";
    CLANG_EXTRA_INCLUDE_DIR = "${pkgs.llvmPackages_9.clang-unwrapped.lib}/lib/clang/9.0.1/include";
    OPENOCD_ZYNQ = "${zynq-rs}/openocd";
    SZL = "${zc706-szl}/szl.elf";
  }
