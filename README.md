ARTIQ on Zynq
=============

Entangler Gateware Build
------------------------

This repo can build Kasli-SoC gateware with the Entangler integrated through the flake-based environment.

The flake input for the Entangler is pinned to:

- repo: `QuantumQuadrate/madmax-entangler-core`
- branch: `artiq-integration`

The current standalone example in this repo is [entagnler_test.json](entagnler_test.json). It is configured for:

- one DIO EEM on port `0`
- `NUM_ENTANGLER_INPUT_SIGNALS = 4`
- `NUM_OUTPUT_CHANNELS = 4`
- `NUM_GENERIC_INPUT_SIGNALS = 0`
- `uses_reference = false`
- `running_output = false`

The Entangler gateware settings are taken from [entangler_settings.toml](entangler_settings.toml). Those settings are exported into both the `nix develop` shell and the flake-driven build commands, so changing that file changes the Entangler build configuration for this repo.

For the current single-card example, the intended DIO usage is:

- lower-bank DIO lines are inputs
- upper-bank DIO lines are outputs
- the active Entangler channels use all four inputs and all four outputs on that split

Do not add a separate `dio` peripheral entry for the same EEM port as the
Entangler. The Entangler peripheral owns that DIO card in gateware. When the
Entangler is disabled in software with `entangler.set_config(enable=False)`, the
output lines pass through to their normal `TTLOut` devices. When it is enabled
with `entangler.set_config(enable=True, standalone=True)`, the Entangler core
drives those same output lines.

For the current 4-input/4-output DIO-card layout:

| Physical line | Entangler role | RTIO channel | `device_db.py` device |
|---|---|---:|---|
| `dio0[4]` | output 0 | `0x000000` | `ttl0` |
| `dio0[5]` | output 1 | `0x000001` | `ttl1` |
| `dio0[6]` | output 2 | `0x000002` | `ttl2` |
| `dio0[7]` | output 3 | `0x000003` | `ttl3` |
| `dio0[0]` | input 0 | `0x000004` | `ttl4` |
| `dio0[1]` | input 1 | `0x000005` | `ttl5` |
| `dio0[2]` | input 2 | `0x000006` | `ttl6` |
| `dio0[3]` | input 3 | `0x000007` | `ttl7` |
| Entangler PHY | driver device | `0x000008` | `entangler0` |

End-to-end build from JSON
--------------------------

Use this flow when you want to go from a Kasli-SoC JSON description file to:

- `build/gateware/top.bit`
- `build/runtime.bin` or `build/satman.bin`
- `build/boot.bin`
- `device_db.py`

Operational convention for this repo: when asking for "new gateware" for the
Kasli-SoC hardware, the expected final artifact is a fresh `build/boot.bin`.
The raw `build/gateware/top.bit` is only an intermediate bitstream. It must be
packaged together with the Kasli-SoC second-stage bootloader and matching
firmware before it is useful as the SD-card boot image.

There are two inputs:

- `DESC`: the JSON description file, for example `entagnler_test.json`.
- `ROLE`: the RTIO role, one of `standalone`, `master`, or `satellite`.

The JSON file should also contain a matching `drtio_role`. The `ROLE` variable is used by these commands to choose the correct firmware program:

- `standalone` uses `runtime`.
- `master` uses `runtime`.
- `satellite` uses `satman`.

From the repo root:

```bash
export DESC=entagnler_test.json
export ROLE=standalone

case "$ROLE" in
  standalone|master)
    export FIRMWARE=runtime
    ;;
  satellite)
    export FIRMWARE=satman
    ;;
  *)
    echo "ROLE must be standalone, master, or satellite" >&2
    exit 1
    ;;
esac
```

Build the raw FPGA bitstream:

```bash
cd src
nix develop --command python gateware/kasli_soc.py -g ../build/gateware ../"$DESC"
cd ..
```

This produces `build/gateware/top.bit`. Keep going through the firmware and
`boot.bin` packaging steps below before copying anything to the SD card.

To inspect the Entangler RTIO/DIO mapping without compiling the bitstream, omit
`-g`:

```bash
cd src
nix develop --command python gateware/kasli_soc.py ../"$DESC"
cd ..
```

Use that output to find the right TTL channel after changing
[entangler_settings.toml](entangler_settings.toml), `running_output`, or the EEM
port number. The generated [device_db.py](device_db.py) then gives the matching
ARTIQ device names. For the current `entagnler_test.json`, the active split-bank
mapping is reported as outputs on `dio0[4, 5, 6, 7]` at RTIO channels `0 -> 3`
and inputs on `dio0[0, 1, 2, 3]` at RTIO channels `4 -> 7`.

Build the matching firmware:

```bash
cd src
nix develop --command make TARGET=kasli_soc GWARGS=../"$DESC" "$FIRMWARE"
cd ..
```

This produces:

- `build/firmware/armv7-none-eabihf/release/runtime` and `build/runtime.bin` for `standalone` or `master`.
- `build/firmware/armv7-none-eabihf/release/satman` and `build/satman.bin` for `satellite`.

Build the Kasli-SoC second-stage bootloader:

```bash
mkdir -p build
cd build
nix build git+https://git.m-labs.hk/m-labs/zynq-rs#kasli_soc-szl
cd ..
```

Create `boot.bif` and package the SD-card boot image:

```bash
cd build
printf '%s\n' \
  'the_ROM_image:' \
  '{' \
  '  [bootloader]result/szl.elf' \
  '  gateware/top.bit' \
  "  firmware/armv7-none-eabihf/release/$FIRMWARE" \
  '}' > boot.bif

nix develop .. --command mkbootimage boot.bif boot.bin
cd ..
```

This produces:

- `build/result/szl.elf`
- `build/boot.bif`
- `build/boot.bin`, the final file to copy to the SD card

Generate the ARTIQ device database from the same JSON description:

```bash
nix develop --command bash -lc 'python entangler_device_db_maker.py "$DESC" > device_db.py'
```

Check the generated `core_addr` in `device_db.py` before running experiments. The generator gets the RTIO channel layout from the JSON description and the Entangler settings exported from [entangler_settings.toml](entangler_settings.toml).

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
