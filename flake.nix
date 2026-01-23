{
  description = "ARTIQ port to the Zynq-7000 platform";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";
  inputs.artiq.url = "git+https://github.com/QuantumQuadrate/madmax-artiq.git";
  inputs.zynq-rs.url = "git+https://git.m-labs.hk/m-labs/zynq-rs";
  inputs.zynq-rs.inputs.nixpkgs.follows = "artiq/nixpkgs";

  # Entangler Core: Nix packaging (dir=nix)
  inputs.entangler-core = {
    url = "git+https://github.com/QuantumQuadrate/madmax-entangler-core.git";
    inputs.artiqpkgs.follows = "artiq";
    inputs.nixpkgs.follows = "artiq/nixpkgs";
  };

  # Entangler Core: raw source tree (needed for PYTHONPATH)
  inputs.entangler-core-src = {
    url = "git+https://github.com/QuantumQuadrate/madmax-entangler-core.git";
    flake = false;
  };

  outputs = {
    self,
    zynq-rs,
    artiq,
    entangler-core,
    entangler-core-src,
    ...
  }@inputs: let
    system = "x86_64-linux";

    pkgs = import artiq.inputs.nixpkgs {
      inherit system;
      overlays = [ (import zynq-rs.inputs.rust-overlay) ];
    };

    zynqpkgs = zynq-rs.packages.${system};
    artiqpkgs = artiq.packages.${system};
    zynqRev = self.sourceInfo.rev or "unknown";

    rust = zynq-rs.rust;
    naerskLib = zynq-rs.naerskLib;

    # Helper: PYTHONPATH entries for entangler source
    entanglerPyPath = ''
      export ENTANGLER_SRC=${entangler-core-src}
      export PYTHONPATH=${entangler-core-src}:${entangler-core-src}/python:${entangler-core-src}/src:$PYTHONPATH
    '';

    fastnumbers = pkgs.python3Packages.buildPythonPackage rec {
      pname = "fastnumbers";
      version = "5.1.0";
      src = pkgs.python3Packages.fetchPypi {
        inherit pname version;
        sha256 = "sha256-4JLTP4uVwxcaL7NOV57+DFSwKQ3X+W/6onYkN2AdkKc=";
      };
      pyproject = true;
      build-system = [ pkgs.python3Packages.setuptools ];
    };

    artiq-netboot = pkgs.python3Packages.buildPythonPackage rec {
      pname = "artiq-netboot";
      version = "unstable-2020-10-15";
      src = pkgs.fetchgit {
        url = "https://git.m-labs.hk/m-labs/artiq-netboot.git";
        rev = "04f69eb07df73abe4b89fde2c24084f7664f2104";
        sha256 = "0ql4fr8m8gpb2yql8aqsdqsssxb8zqd6l65kl1f6s9845zy7shs9";
      };
      pyproject = true;
      build-system = [ pkgs.python3Packages.setuptools ];
    };

    ramda = pkgs.python3Packages.buildPythonPackage {
      pname = "ramda";
      version = "unstable-2020-04-11";
      src = pkgs.fetchFromGitHub {
        owner = "peteut";
        repo = "ramda.py";
        rev = "d315a9717ebd639366bf3fe26bad9e3d08ec3c49";
        sha256 = "sha256-bmSt/IHDnULsZjsC6edELnNH7LoJSVF4L4XhwBAXRkY=";
      };
      pyproject = true;
      build-system = [ pkgs.python3Packages.setuptools ];
      nativeBuildInputs = with pkgs.python3Packages; [ pbr ];
      propagatedBuildInputs = with pkgs.python3Packages; [ fastnumbers ];
      checkInputs = with pkgs.python3Packages; [ pytest ];
      checkPhase = "pytest";
      doCheck = false;
      preBuild = ''
        export PBR_VERSION=0.5.5
      '';
    };

    migen-axi = pkgs.python3Packages.buildPythonPackage {
      pname = "migen-axi";
      version = "unstable-2023-01-06";
      src = pkgs.fetchFromGitHub {
        owner = "peteut";
        repo = "migen-axi";
        rev = "98649a92ed7d4e43f75231e6ef9753e1212fab41";
        sha256 = "sha256-0kEHK+l6gZW750tq89fHRxIh3Gnj5EP2GZX/neWaWzU=";
      };
      pyproject = true;
      build-system = [ pkgs.python3Packages.setuptools ];

      propagatedBuildInputs =
        with pkgs.python3Packages; [
          setuptools click numpy toolz jinja2 ramda
        ] ++ [
          artiqpkgs.migen
          artiqpkgs.misoc
        ];

      checkInputs = with pkgs.python3Packages; [ pytestCheckHook pytest-timeout ];

      # migen/misoc version checks are broken with pyproject for some reason
      postPatch = ''
        sed -i "1,4d" pyproject.toml
        substituteInPlace pyproject.toml \
          --replace '"migen@git+https://github.com/m-labs/migen",' ""
        substituteInPlace pyproject.toml \
          --replace '"misoc@git+https://github.com/m-labs/misoc.git",' ""
        # pytest-flake8 is broken with recent flake8. Re-enable after fix.
        substituteInPlace setup.cfg --replace '--flake8' ""
      '';
    };

    binutils = { platform, target, zlib }:
      pkgs.stdenv.mkDerivation rec {
        basename = "binutils";
        version = "2.30";
        name = "${basename}-${platform}-${version}";
        src = pkgs.fetchurl {
          url = "https://ftp.gnu.org/gnu/binutils/binutils-${version}.tar.bz2";
          sha256 = "028cklfqaab24glva1ks2aqa1zxa6w6xmc8q34zs1sb7h22dxspg";
        };
        configureFlags = [ "--enable-shared" "--enable-deterministic-archives" "--target=${target}" ];
        outputs = [ "out" "info" "man" ];
        depsBuildBuild = [ pkgs.buildPackages.stdenv.cc ];
        buildInputs = [ zlib ];
        enableParallelBuilding = true;
      };

    binutils-arm = pkgs.callPackage binutils {
      platform = "arm";
      target = "armv7-unknown-linux-gnueabihf";
    };

    # FSBL configuration supplied by Vivado 2020.1 for these boards:
    fsblTargets = [ "zc702" "zc706" "zed" ];
    sat_variants = [
      # kasli-soc satellite variants
      "satellite"
      # zc706 satellite variants
      "nist_clock_satellite"
      "nist_qc2_satellite"
      "acpki_nist_clock_satellite"
      "acpki_nist_qc2_satellite"
      "nist_clock_satellite_100mhz"
      "nist_qc2_satellite_100mhz"
      "acpki_nist_clock_satellite_100mhz"
      "acpki_nist_qc2_satellite_100mhz"
    ];

    board-package-set = { target, variant, json ? null }: let
      szl = zynqpkgs."${target}-szl";
      fsbl = zynqpkgs."${target}-fsbl";
      fwtype = if builtins.elem variant sat_variants then "satman" else "runtime";

      firmware = naerskLib.buildPackage rec {
        name = "firmware";
        src = ./src;
        additionalCargoLock = "${rust}/lib/rustlib/src/rust/library/Cargo.lock";
        singleStep = true;

        nativeBuildInputs = [
          pkgs.gnumake
          (pkgs.python3.withPackages (ps: [
            artiqpkgs.migen
            migen-axi
            artiqpkgs.misoc
            artiqpkgs.artiq-build
          ]))
          pkgs.llvmPackages_20.llvm
          pkgs.llvmPackages_20.clang-unwrapped
        ];

        overrideMain = _: {
          buildPhase = ''
            export ZYNQ_REV=${zynqRev}
            export CLANG_EXTRA_INCLUDE_DIR="${pkgs.llvmPackages_20.clang-unwrapped.lib}/lib/clang/20/include"
            export ZYNQ_RS=${zynq-rs}
            ${entanglerPyPath}
            make TARGET=${target} GWARGS="${
              if json == null then "-V ${variant}" else json
            }" ${fwtype}
          '';

          installPhase = ''
            mkdir -p $out $out/nix-support
            cp ../build/${fwtype}.bin $out/${fwtype}.bin
            cp ../build/firmware/armv7-none-eabihf/release/${fwtype} $out/${fwtype}.elf
            echo file binary-dist $out/${fwtype}.bin >> $out/nix-support/hydra-build-products
            echo file binary-dist $out/${fwtype}.elf >> $out/nix-support/hydra-build-products
          '';

          doCheck = false;
          dontFixup = true;
        };
      };

      gateware = pkgs.runCommand "${target}-${variant}-gateware"
        {
          nativeBuildInputs = [
            (pkgs.python3.withPackages (ps: [
              artiqpkgs.migen
              migen-axi
              artiqpkgs.misoc
              artiqpkgs.artiq-build
            ]))
            artiqpkgs.vivado
          ];
        }
        ''
          export ZYNQ_REV=${zynqRev}
          ${entanglerPyPath}
          python ${./src/gateware}/${target}.py -g build ${
            if json == null then "-V ${variant}" else json
          }
          mkdir -p $out $out/nix-support
          cp build/top.bit $out
          echo file binary-dist $out/top.bit >> $out/nix-support/hydra-build-products
        '';

      jtag = pkgs.runCommand "${target}-${variant}-jtag" {} ''
        mkdir $out
        ln -s ${szl}/szl.elf $out
        ln -s ${firmware}/${fwtype}.bin $out
        ln -s ${gateware}/top.bit $out
      '';

      sd = pkgs.runCommand "${target}-${variant}-sd"
        { buildInputs = [ zynqpkgs.mkbootimage ]; }
        ''
          bifdir=`mktemp -d`
          cd $bifdir
          ln -s ${szl}/szl.elf szl.elf
          ln -s ${firmware}/${fwtype}.elf ${fwtype}.elf
          ln -s ${gateware}/top.bit top.bit
          cat > boot.bif << EOF
          the_ROM_image:
          {
            [bootloader]szl.elf
            top.bit
            ${fwtype}.elf
          }
          EOF
          mkdir $out $out/nix-support
          mkbootimage boot.bif $out/boot.bin
          echo file binary-dist $out/boot.bin >> $out/nix-support/hydra-build-products
        '';

      fsbl-sd = pkgs.runCommand "${target}-${variant}-fsbl-sd"
        { buildInputs = [ zynqpkgs.mkbootimage ]; }
        ''
          bifdir=`mktemp -d`
          cd $bifdir
          ln -s ${fsbl}/fsbl.elf fsbl.elf
          ln -s ${gateware}/top.bit top.bit
          ln -s ${firmware}/${fwtype}.elf ${fwtype}.elf
          cat > boot.bif << EOF
          the_ROM_image:
          {
            [bootloader]fsbl.elf
            top.bit
            ${fwtype}.elf
          }
          EOF
          mkdir $out $out/nix-support
          mkbootimage boot.bif $out/boot.bin
          echo file binary-dist $out/boot.bin >> $out/nix-support/hydra-build-products
        '';
    in
      {
        "${target}-${variant}-firmware" = firmware;
        "${target}-${variant}-gateware" = gateware;
        "${target}-${variant}-jtag" = jtag;
        "${target}-${variant}-sd" = sd;
      }
      // (if builtins.elem target fsblTargets then {
        "${target}-${variant}-fsbl-sd" = fsbl-sd;
      } else {});

    gateware-sim = pkgs.stdenv.mkDerivation {
      name = "gateware-sim";
      nativeBuildInputs = [
        (pkgs.python3.withPackages (ps: [ artiqpkgs.migen migen-axi artiqpkgs.artiq-build ]))
      ];
      phases = [ "buildPhase" ];
      buildPhase = ''
        python -m unittest discover ${self}/src/gateware -v
        touch $out
      '';
    };

    fmt-check = pkgs.stdenvNoCC.mkDerivation {
      name = "fmt-check";
      src = ./src;
      nativeBuildInputs = [ rust pkgs.gnumake ];
      phases = [ "unpackPhase" "buildPhase" ];
      buildPhase = ''
        export ZYNQ_RS=${zynq-rs}
        make manifests
        cargo fmt -- --check
        touch $out
      '';
    };

    # for hitl-tests
    zc706-nist_qc2 = board-package-set { target = "zc706"; variant = "nist_qc2"; };
    zc706-acpki_nist_qc2 = board-package-set { target = "zc706"; variant = "acpki_nist_qc2"; };

    make-zc706-hitl-tests = { name, board-package, ddb_folder ? "examples" }:
      pkgs.stdenv.mkDerivation {
        name = "zc706-hitl-tests-${name}";
        __networked = true;

        buildInputs = [
          pkgs.netcat
          pkgs.openssh
          pkgs.rsync
          artiqpkgs.artiq
          artiq-netboot
          zynqpkgs.zc706-szl
        ];
        phases = [ "buildPhase" ];
        buildPhase = ''
          export NIX_SSHOPTS="-F /dev/null -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -i /opt/hydra_id_ed25519"
          LOCKCTL=$(mktemp -d)
          mkfifo $LOCKCTL/lockctl

          cat $LOCKCTL/lockctl | ${pkgs.openssh}/bin/ssh \
          $NIX_SSHOPTS \
          rpi-4 \
          'mkdir -p /tmp/board_lock && flock /tmp/board_lock/zc706-1 -c "echo Ok; cat"' \
          | (
            atexit_unlock() { echo > $LOCKCTL/lockctl; }
            trap atexit_unlock EXIT
            read LOCK_OK

            echo Power cycling board...
            (echo b; sleep 5; echo B; sleep 5) | nc -N -w6 192.168.1.31 3131
            echo Power cycle done.

            export USER=hydra
            export OPENOCD_ZYNQ=${zynq-rs}/openocd
            export SZL=${zynqpkgs.szl}
            bash ${self}/remote_run.sh -h rpi-4 -o "$NIX_SSHOPTS" -d ${board-package}

            echo Waiting for the firmware to boot...
            sleep 15

            echo Running test kernel...
            artiq_run --device-db ${self}/${ddb_folder}/device_db.py ${self}/examples/mandelbrot.py

            echo Running ARTIQ unit tests...
            export ARTIQ_ROOT=${self}/${ddb_folder}
            export ARTIQ_LOW_LATENCY=1
            python -m unittest discover artiq.test.coredevice -v

            touch $out

            echo Completed
            (echo b; sleep 5) | nc -N -w6 192.168.1.31 3131
            echo Board powered off
          )
        '';
      };

    zc706-hitl-tests = make-zc706-hitl-tests {
      name = "nist_qc2";
      board-package = zc706-nist_qc2.zc706-nist_qc2-jtag;
    };

    zc706-acpki-hitl-tests = make-zc706-hitl-tests {
      name = "acpki_nist_qc2";
      board-package = zc706-acpki_nist_qc2.zc706-acpki_nist_qc2-jtag;
      ddb_folder = "examples/acpki";
    };

  in rec {
    packages.${system} =
      {
        inherit fastnumbers artiq-netboot ramda migen-axi binutils-arm;
      }
      // (board-package-set { target = "zc706"; variant = "cxp_4r_fmc"; })
      // (board-package-set { target = "zc706"; variant = "nist_clock"; })
      // (board-package-set { target = "zc706"; variant = "nist_clock_master"; })
      // (board-package-set { target = "zc706"; variant = "nist_clock_master_100mhz"; })
      // (board-package-set { target = "zc706"; variant = "nist_clock_satellite"; })
      // (board-package-set { target = "zc706"; variant = "nist_clock_satellite_100mhz"; })
      // (board-package-set { target = "zc706"; variant = "nist_qc2"; })
      // (board-package-set { target = "zc706"; variant = "nist_qc2_master"; })
      // (board-package-set { target = "zc706"; variant = "nist_qc2_master_100mhz"; })
      // (board-package-set { target = "zc706"; variant = "nist_qc2_satellite"; })
      // (board-package-set { target = "zc706"; variant = "nist_qc2_satellite_100mhz"; })
      // (board-package-set { target = "zc706"; variant = "acpki_nist_clock"; })
      // (board-package-set { target = "zc706"; variant = "acpki_nist_clock_master"; })
      // (board-package-set { target = "zc706"; variant = "acpki_nist_clock_master_100mhz"; })
      // (board-package-set { target = "zc706"; variant = "acpki_nist_clock_satellite"; })
      // (board-package-set { target = "zc706"; variant = "acpki_nist_clock_satellite_100mhz"; })
      // (board-package-set { target = "zc706"; variant = "acpki_nist_qc2"; })
      // (board-package-set { target = "zc706"; variant = "acpki_nist_qc2_master"; })
      // (board-package-set { target = "zc706"; variant = "acpki_nist_qc2_master_100mhz"; })
      // (board-package-set { target = "zc706"; variant = "acpki_nist_qc2_satellite"; })
      // (board-package-set { target = "zc706"; variant = "acpki_nist_qc2_satellite_100mhz"; })
      // (board-package-set { target = "kasli_soc"; variant = "demo"; json = ./demo.json; })
      // (board-package-set { target = "kasli_soc"; variant = "master"; json = ./kasli-soc-master.json; })
      // (board-package-set { target = "kasli_soc"; variant = "satellite"; json = ./kasli-soc-satellite.json; })
      // (board-package-set { target = "ebaz4205"; variant = "base"; });

    hydraJobs =
      packages.${system}
      // {
        inherit zc706-hitl-tests;
        inherit zc706-acpki-hitl-tests;
        inherit gateware-sim;
        inherit fmt-check;
      };

    formatter.${system} = pkgs.alejandra;

    devShell.${system} = pkgs.mkShell {
      name = "artiq-zynq-dev-shell";

      buildInputs = with pkgs; [
        rust
        llvmPackages_20.llvm
        llvmPackages_20.clang-unwrapped
        gnumake
        cacert
        zynqpkgs.mkbootimage
        openocd
        openssh
        rsync

        (python3.withPackages (ps:
          (with artiqpkgs; [
            migen
            migen-axi
            misoc
            artiq
            artiq-netboot
            ps.jsonschema
            ps.pyftdi
          ])
        ))

        artiqpkgs.artiq
        artiqpkgs.vivado
        binutils-arm
        pre-commit
      ];

      ZYNQ_REV = "${zynqRev}";
      CLANG_EXTRA_INCLUDE_DIR = "${pkgs.llvmPackages_20.clang-unwrapped.lib}/lib/clang/20/include";
      ZYNQ_RS = "${zynq-rs}";
      OPENOCD_ZYNQ = "${zynq-rs}/openocd";
      SZL = "${zynqpkgs.szl}";

      shellHook = ''
        ${entanglerPyPath}
        echo "ENTANGLER_SRC=$ENTANGLER_SRC"


      # Vivado env for this dev shell only (no sudo)
      if [ -f /opt/Xilinx/Vivado/2022.2/settings64.sh ]; then
        # shellcheck disable=SC1091
        source /opt/Xilinx/Vivado/2022.2/settings64.sh
      else
        echo "WARNING: Vivado 2022.2 settings64.sh not found at /opt/Xilinx/Vivado/2022.2/settings64.sh"
      fi
    '';
    };

    makeArtiqZynqPackage = board-package-set;
  };
}
