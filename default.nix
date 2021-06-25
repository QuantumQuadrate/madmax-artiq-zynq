let
  zynq-rs = (import ./zynq-rs.nix);
  pkgs = import <nixpkgs> { overlays = [ (import "${zynq-rs}/nix/mozilla-overlay.nix") ]; };
  rustPlatform = (import "${zynq-rs}/nix/rust-platform.nix" { inherit pkgs; });
  cargo-xbuild = (import zynq-rs).cargo-xbuild;
  mkbootimage = import "${zynq-rs}/nix/mkbootimage.nix" { inherit pkgs; };
  artiqpkgs = import <artiq-fast/default.nix> { inherit pkgs; };
  vivado = import <artiq-fast/vivado.nix> { inherit pkgs; };
  # FSBL configuration supplied by Vivado 2020.1 for these boards:
  fsblTargets = ["zc702" "zc706" "zed"];
  build = { target, variant }: let
    szl = (import zynq-rs)."${target}-szl";
    fsbl = import "${zynq-rs}/nix/fsbl.nix" {
      inherit pkgs;
      board = target;
    };

    firmware = rustPlatform.buildRustPackage rec {
      # note: due to fetchCargoTarball, cargoSha256 depends on package name
      name = "firmware";

      src = ./src;
      cargoSha256 = "1d84yknyizbxgsqj478339fxcyvxq9pzdv0ljrwrgmzgfynqmssj";

      nativeBuildInputs = [
        pkgs.gnumake
        (pkgs.python3.withPackages(ps: (with artiqpkgs; [ migen migen-axi misoc artiq ])))
        cargo-xbuild
        pkgs.llvmPackages_9.llvm
        pkgs.llvmPackages_9.clang-unwrapped
      ];
      buildPhase = ''
        export XARGO_RUST_SRC="${rustPlatform.rust.rustc}/lib/rustlib/src/rust/library"
        export CLANG_EXTRA_INCLUDE_DIR="${pkgs.llvmPackages_9.clang-unwrapped.lib}/lib/clang/9.0.1/include"
        export CARGO_HOME=$(mktemp -d cargo-home.XXX)
        make TARGET=${target} VARIANT=${variant}
      '';

      installPhase = ''
        mkdir -p $out $out/nix-support
        cp ../build/runtime.bin $out/runtime.bin
        cp ../build/firmware/armv7-none-eabihf/release/runtime $out/runtime.elf
        echo file binary-dist $out/runtime.bin >> $out/nix-support/hydra-build-products
        echo file binary-dist $out/runtime.elf >> $out/nix-support/hydra-build-products
      '';

      doCheck = false;
      dontFixup = true;
    };
    gateware = pkgs.runCommand "${target}-${variant}-gateware"
      {
        nativeBuildInputs = [ 
          (pkgs.python3.withPackages(ps: (with artiqpkgs; [ migen migen-axi misoc artiq ])))
          vivado
        ];
      }
      ''
        python ${./src/gateware}/${target}.py -g build -V ${variant}
        mkdir -p $out $out/nix-support
        cp build/top.bit $out
        echo file binary-dist $out/top.bit >> $out/nix-support/hydra-build-products
      '';

    # SZL startup
    jtag = pkgs.runCommand "${target}-${variant}-jtag" {}
      ''
        mkdir $out
        ln -s ${szl}/szl.elf $out
        ln -s ${firmware}/runtime.bin $out
        ln -s ${gateware}/top.bit $out
      '';
    sd = pkgs.runCommand "${target}-${variant}-sd"
      {
        buildInputs = [ mkbootimage ];
      }
      ''
      # Do not use "long" paths in boot.bif, because embedded developers
      # can't write software (mkbootimage will segfault).
      bifdir=`mktemp -d`
      cd $bifdir
      ln -s ${szl}/szl.elf szl.elf
      ln -s ${firmware}/runtime.elf runtime.elf
      ln -s ${gateware}/top.bit top.bit
      cat > boot.bif << EOF
      the_ROM_image:
      {
        [bootloader]szl.elf
        top.bit
        runtime.elf
      }
      EOF
      mkdir $out $out/nix-support
      mkbootimage boot.bif $out/boot.bin
      echo file binary-dist $out/boot.bin >> $out/nix-support/hydra-build-products
      '';

    # FSBL startup
    fsbl-sd = pkgs.runCommand "${target}-${variant}-fsbl-sd"
      {
        buildInputs = [ mkbootimage ];
      }
      ''
      bifdir=`mktemp -d`
      cd $bifdir
      ln -s ${fsbl}/fsbl.elf fsbl.elf
      ln -s ${gateware}/top.bit top.bit
      ln -s ${firmware}/runtime.elf runtime.elf
      cat > boot.bif << EOF
      the_ROM_image:
      {
        [bootloader]fsbl.elf
        top.bit
        runtime.elf
      }
      EOF
      mkdir $out $out/nix-support
      mkbootimage boot.bif $out/boot.bin
      echo file binary-dist $out/boot.bin >> $out/nix-support/hydra-build-products
      '';
  in {
    "${target}-${variant}-firmware" = firmware;
    "${target}-${variant}-gateware" = gateware;
    "${target}-${variant}-jtag" = jtag;
    "${target}-${variant}-sd" = sd;
  } // (
    if builtins.elem target fsblTargets
    then {
      "${target}-${variant}-fsbl-sd" = fsbl-sd;
    }
    else {}
  );
in
  (
    (build { target = "zc706"; variant = "simple"; }) //
    (build { target = "zc706"; variant = "nist_clock"; }) //
    (build { target = "zc706"; variant = "nist_qc2"; }) //
    (build { target = "zc706"; variant = "acpki_simple"; }) //
    (build { target = "zc706"; variant = "acpki_nist_clock"; }) //
    (build { target = "zc706"; variant = "acpki_nist_qc2"; }) //
    (build { target = "coraz7"; variant = "10"; }) //
    (build { target = "coraz7"; variant = "07s"; }) //
    (build { target = "coraz7"; variant = "acpki_10"; }) //
    (build { target = "coraz7"; variant = "acpki_07s"; }) //
    (build { target = "redpitaya"; variant = "simple"; }) //
    (build { target = "redpitaya"; variant = "acpki_simple"; }) //
    { inherit zynq-rs; }
  )
