ARTIQ on Zynq
=============

How to build the gateware. 

```bash
git clone https://github.com/QuantumQuadrate/madmax-artiq-zynq.git
cd madmax-artiq-zynq
nix develop
cd src
gateware/kasli_soc.py -g ../build/gateware '/home/jrydberg/Documents/Projects/Artiq_envs/madmax-artiq-zynq/kasli-soc-standalone_node1_with_edgecounters_en.json'
```

for standalone run
``` bash
make TARGET=kasli_soc GWARGS=json runtime 
```
for master satelite run
``` bash
make TARGET=kasli_soc GWARGS=json staman 
```

next run this
```bash
cd ../build
nix build git+https://git.m-labs.hk/m-labs/zynq-rs#kasli_soc-szl
```

Then run 
```bash
```


to get the device_db.py use the entangler_device_db_maker.py as follows

``` bash
python -m entangler_device_db_maker <description_file.json>
```

How to use
----------

1. [Install ARTIQ](https://m-labs.hk/artiq/manual/installing.html). Get the corresponding version to the ``artiq-zynq`` version you are targeting.
2. To obtain firmware binaries, use AFWS or build your own; see [the ARTIQ manual](https://m-labs.hk/artiq/manual/building_developing.html) for detailed instructions or skip to "Development" below. ZC706 variants only can also be downloaded from latest successful build on [Hydra](https://nixbld.m-labs.hk/).
3. Place ``boot.bin`` file at the root ``/`` of a FAT-formatted SD card.
4. Optionally, create a ``config.txt`` configuration file containing ``key=value`` pairs on each line and place it at the root of the SD card. See below for valid keys. The ``ip``, ``ip6`` and ``mac`` keys can be used to set networking information. If these keys are not found, the firmware will use default values which may or may not be compatible with your network.
5. Insert the SD card into the board and set the board to boot from the SD card. For ZC706, this is achieved by placing the large DIP switch SW11 into the 00110 position. On Kasli-SoC, place the BOOT MODE switches to SD.
6. Power up the board. After successful boot the firmware should respond to ping at its IP addresses. Boot output can be observed from UART at 115200bps 8-N-1.
7. Create and use an ARTIQ device database as usual.

Configuration
-------------

Configuring the device is done using the ``config.txt`` text file at the root of the SD card plus optionally a ``config`` folder. When searching for a configuration key, the firmware first looks for a file named ``/config/[key].bin`` and, if it exists, returns the contents of that file. If not, it looks into ``/config.txt``, which should contain a list of ``key=value`` pairs, one per line. ``config.txt`` should be used for most keys but the ``config`` folder allows for setting configuration values which consist of binary data, such as the startup kernel.

The following configuration keys are available among others:

- ``mac``: Ethernet MAC address.
- ``ip``: IPv4 address.
- ``ip6``: IPv6 address.
- ``idle_kernel``: idle kernel in ELF format (as produced by ``artiq_compile``).
- ``startup_kernel``: startup kernel in ELF format (as produced by ``artiq_compile``).
- ``rtio_clock``: source of RTIO clock; valid values are ``ext0_bypass`` and ``int_125``.

See [ARTIQ manual](https://m-labs.hk/artiq/manual-beta/core_device.html#configuration-storage) for full list. Configurations can be read/written/removed with ``artiq_coremgmt``. Config erase is not implemented, as it isn't particularly useful.

For convenience, the ``boot`` key can be used with ``artiq_coremgmt`` and a ``boot.bin`` file to replace firmware/gateware in a running system. This key is read-only. When loading ``boot.bin`` onto the SD card directly, place it at the root and not in the ``config`` folder.

Development instructions
------------------------

ARTIQ on Zynq is packaged using [Nix](https://nixos.org) Flakes. Install Nix 2.8+ and enable flakes by adding ``experimental-features = nix-command flakes`` to ``nix.conf`` (e.g. ``~/.config/nix/nix.conf``).

**Pure build with Nix:**

```shell
nix build .#zc706-nist_clock-jtag  # or zc706-nist_qc2-jtag or zc706-nist_clock-sd or etc
```

Run ``nix flake show`` to see all valid build targets. Targets suffixed with ``-jtag`` produce separate firmware and gateware files, intended for use in booting via JTAG server/Ethernet, e.g. ``./remote_run.sh -i`` with a remote JTAG server. Targets suffixed with ``-sd`` will produce ``boot.bin`` file suitable for SD card boot. ``-firmware`` and ``-gateware`` respectively build firmware and gateware only.

The Kasli-SoC target requires a system description file as input. See ARTIQ manual for exact instructions or use incremental build.

**Impure incremental build:**

For boards with fixed variants, i.e. ZC706, etc. :

```shell
nix develop
cd src
gateware/<board>.py -g ../build/gateware -V <variant> # gateware
make GWARGS="-V <variant>" <runtime/satman>    # firmware
```

For boards with system descriptions, i.e. Kasli-SoC, etc. :

```shell
nix develop
cd src
gateware/<board>.py -g ../build/gateware <description.json> # gateware
make TARGET=<board> GWARGS="path/to/description.json" <runtime/satman> # firmware
```

``szl.elf`` can be obtained with:

```shell
nix build git+https://git.m-labs.hk/m-labs/zynq-rs#<board>-szl
```

To generate ``boot.bin`` use ``mkbootimage``, e.g.:

```shell
echo "the_ROM_image:
    {
        [bootloader]result/szl.elf
        gateware/top.bit
        firmware/armv7-none-eabihf/release/<runtime/satman>
    }
    EOF" >> boot.bif
mkbootimage boot.bif boot.bin
```

Notes:

- The impure build process is also compatible with non-Nix systems.
- Firmware type must be either ``runtime`` for DRTIO-less or DRTIO master variants, or ``satman`` for DRTIO satellite.
- If the board is connected to the local machine by JTAG, use the ``local_run.sh`` script.
- A known Xilinx hardware bug prevents repeatedly loading the bootloader over JTAG without a POR reset. If booting over JTAG, install a jumper on ``PS_POR_B`` and use the POR reset script [here](https://git.m-labs.hk/M-Labs/zynq-rs/src/branch/master/kasli_soc_por.py).

Pre-Commit Hooks
----------------

You are strongly recommended to use the provided pre-commit hooks to automatically reformat files and check for non-optimal Rust/C/C++ practices. Run `pre-commit install` to install the hook and `pre-commit` will automatically run `cargo fmt`, `cargo clippy`, and `clang-format` for you.

Several things to note:

- If `cargo fmt`, `cargo clippy`, or `clang-format` returns an error, the pre-commit hook will fail. You should fix all errors before trying to commit again.
- If `cargo fmt` or `clang-format` reformats some files, the pre-commit hook will also fail. You should review the changes and, if satisfied, try to commit again.

License
-------

Copyright (C) 2019-2024 M-Labs Limited.

ARTIQ is free software: you can redistribute it and/or modify
it under the terms of the GNU Lesser General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

ARTIQ is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU Lesser General Public License for more details.

You should have received a copy of the GNU Lesser General Public License
along with ARTIQ.  If not, see <http://www.gnu.org/licenses/>.
