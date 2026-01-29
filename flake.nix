{
  description = "ARTIQ port to the Zynq-7000 platform";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";
  inputs.artiq.url = "git+https://github.com/QuantumQuadrate/madmax-artiq.git";
  inputs.zynq-rs.url = "git+https://git.m-labs.hk/m-labs/zynq-rs";
  inputs.zynq-rs.inputs.nixpkgs.follows = "artiq/nixpkgs";

  # --- Entangler core flake (use your fork URL if you want) ---
  inputs.entangler-core = {
    # Example: your fork
    url = "git+https://github.com/QuantumQuadrate/madmax-entangler-core.git";
    # Or upstream tag (works if that flake exists in that repo):
    # url = "git+https://gitlab.com/duke-artiq/entangler-core.git?ref=refs/tags/v1.4.1";
    inputs.artiqpkgs.follows = "artiq";
    inputs.nixpkgs.follows = "artiq/nixpkgs";
  };

  outputs =
    { self
    , zynq-rs
    , artiq
    , entangler-core
    , ...
    }@inputs:
    let
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

      regenCargoTomls = ''
        echo "Regenerating Cargo.toml from templates using ZYNQ_RS=$ZYNQ_RS"
        find ./src -name Cargo.toml.tpl -print0 | while IFS= read -r -d "" tpl; do
          toml="''${tpl%.tpl}"
          cp -f "$tpl" "$toml"
          sed -i "s|@@ZYNQ_RS@@|$ZYNQ_RS|g" "$toml"
        done
      '';

      # --- Use the entangler flake's *built package*, not its source tree ---
      entanglerPkg = entangler-core.packages.${system}.default;

      # Convenience: the Python env we want in nix develop
      pythonDev = pkgs.python3.withPackages (ps:
        (with artiqpkgs; [
          migen
          migen-axi
          misoc
          artiq
          artiq-netboot
          ps.jsonschema
          ps.pyftdi
        ])
        ++ [
          entanglerPkg
        ]
      );

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
        postPatch = ''
          sed -i "1,4d" pyproject.toml
          substituteInPlace pyproject.toml \
            --replace '"migen@git+https://github.com/m-labs/migen",' ""
          substituteInPlace pyproject.toml \
            --replace '"misoc@git+https://github.com/m-labs/misoc.git",' ""
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
          configureFlags = [
            "--enable-shared"
            "--enable-deterministic-archives"
            "--target=${target}"
          ];
          outputs = [ "out" "info" "man" ];
          depsBuildBuild = [ pkgs.buildPackages.stdenv.cc ];
          buildInputs = [ zlib ];
          enableParallelBuilding = true;
        };

      binutils-arm = pkgs.callPackage binutils {
        platform = "arm";
        target = "armv7-unknown-linux-gnueabihf";
      };

      fsblTargets = [ "zc702" "zc706" "zed" ];
      sat_variants = [
        "satellite"
        "nist_clock_satellite"
        "nist_qc2_satellite"
        "acpki_nist_clock_satellite"
        "acpki_nist_qc2_satellite"
        "nist_clock_satellite_100mhz"
        "nist_qc2_satellite_100mhz"
        "acpki_nist_clock_satellite_100mhz"
        "acpki_nist_qc2_satellite_100mhz"
      ];

      board-package-set = { target, variant, json ? null }:
        let
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

              # IMPORTANT: use the python env that includes entanglerPkg
              (pkgs.python3.withPackages (ps: [
                artiqpkgs.migen
                migen-axi
                artiqpkgs.misoc
                artiqpkgs.artiq-build
                entanglerPkg
              ]))

              pkgs.llvmPackages_20.llvm
              pkgs.llvmPackages_20.clang-unwrapped
            ];

            overrideMain = _: {
              buildPhase = ''
                export ZYNQ_REV=${zynqRev}
                export CLANG_EXTRA_INCLUDE_DIR="${pkgs.llvmPackages_20.clang-unwrapped.lib}/lib/clang/20/include"
                export ZYNQ_RS=${zynq-rs}

                ${regenCargoTomls}

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
                  entanglerPkg
                ]))
                artiqpkgs.vivado
              ];
            }
            ''
              export ZYNQ_REV=${zynqRev}
              python ${./src/gateware}/${target}.py -g build ${
                if json == null then "-V ${variant}" else json
              }
              mkdir -p $out $out/nix-support
              cp build/top.bit $out
              echo file binary-dist $out/top.bit >> $out/nix-support/hydra-build-products
            '';

          jtag = pkgs.runCommand "${target}-${variant}-jtag" { } ''
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
        } else { });

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

      formatter.${system} = pkgs.alejandra;

      # ---------------------------
      # DEFAULT nix develop shell
      # ---------------------------
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
          pythonDev
          artiqpkgs.vivado
          binutils-arm
          pre-commit
        ];

        ZYNQ_REV = "${zynqRev}";
        CLANG_EXTRA_INCLUDE_DIR = "${pkgs.llvmPackages_20.clang-unwrapped.lib}/lib/clang/20/include";
        ZYNQ_RS = "${zynq-rs}";
        OPENOCD_ZYNQ = "${zynq-rs}/openocd";
        SZL = "${zynqpkgs.szl}";

        # Auto-load Vivado every time you run `nix develop`
        shellHook = ''
          # Auto-fix stale Cargo.toml paths on shell entry
          if find ./src -name Cargo.toml -maxdepth 2 -print0 2>/dev/null | xargs -0 grep -q '/nix/store'; then
            ${regenCargoTomls}
          fi

          if [ -f /opt/Xilinx/Vivado/2022.2/settings64.sh ]; then
            # shellcheck disable=SC1091
            source /opt/Xilinx/Vivado/2022.2/settings64.sh
          else
            echo "NOTE: Vivado 2022.2 not found at /opt/Xilinx/Vivado/2022.2/settings64.sh"
          fi
        '';
      };

      makeArtiqZynqPackage = board-package-set;
    };
}
