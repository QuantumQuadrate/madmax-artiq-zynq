ARTIQ on Zynq
=============

How to use
----------

1. Install ARTIQ-6 or newer.
2. Select the latest successful build on Hydra: https://nixbld.m-labs.hk/jobset/artiq/zynq
3. Search for the job named ``<board>-<variant>-sd`` (for example: ``zc706-nist_clock-sd`` or ``zc706-nist_qc2-sd``).
4. Download the ``boot.bin`` "binary distribution" and place it at the root of a FAT-formatted SD card.
5. Optionally, create a ``config.txt`` configuration file at the root of the SD card containing ``key=value`` pairs on each line. Use the ``ip``, ``ip6`` and ``mac`` keys to respectively set the IPv4, IPv6 and MAC address of the board. Configuring an IPv6 address is entirely optional. If these keys are not found, the firmware will use default values that may or may not be compatible with your network.
6. Insert the SD card into the board and set up the board to boot from the SD card. For the ZC706, this is achieved by placing the large DIP switch SW11 in the 00110 position.
7. Power up the board. After the firmware starts successfully, it should respond to ping at its IP addresses, and boot messages can be observed from its UART at 115200bps.
8. Create and use an ARTIQ device database as usual, but set ``"target": "cortexa9"`` in the arguments of the core device.

Configuration
-------------

Configuring the device is done using the ``config.txt`` text file at the root of the SD card, plus the contents of the ``config`` folder. When searching for a configuration key, the firmware first looks for a file named ``/config/[key].bin`` and, if it exists, returns the contents of that file. If not, it looks into ``/config.txt``, which contains a list of ``key=value`` pairs, one per line. The ``config`` folder allows configuration values that consist in binary data, such as the startup kernel.

The following configuration keys are available:

- ``mac``: Ethernet MAC address.
- ``ip``: IPv4 address.
- ``ip6``: IPv6 address.
- ``startup``: startup kernel in ELF format (as produced by ``artiq_compile``).
- ``rtioclk``: source of RTIO clock; valid values are ``external`` and ``internal``.
- ``boot``: SD card "boot.bin" file, for replacing the boot firmware/gateware. Write only.

Configurations can be read/written/removed via ``artiq_coremgmt``. Config erase is
not implemented as it seems not very useful.

Development instructions
------------------------

Configure Nix channels:

```shell
nix-channel --add https://nixbld.m-labs.hk/channel/custom/artiq/fast-beta/artiq-fast
nix-channel --update
```

Note: if you are using Nix channels the first time, you need to be aware of this bug: https://github.com/NixOS/nix/issues/3831

Pure build with Nix and execution on a remote JTAG server:

```shell
nix-build -A zc706-simple-jtag  # or zc706-nist_qc2-jtag or zc706-nist_clock-jtag
./remote_run.sh
```

Impure incremental build and execution on a remote JTAG server:

```shell
nix-shell
cd src
gateware/zc706.py -g ../build/gateware  # build gateware
make                                    # build firmware
cd ..
./remote_run.sh -i
```

Notes:

- This is developed with Nixpkgs 21.05, and the ``nixbld.m-labs.hk`` binary substituter can also be used here (see the ARTIQ manual for the public key and instructions).
- The impure build process is also compatible with non-Nix systems.
- If the board is connected to the local machine, use the ``local_run.sh`` script.
- To update ``zynq-rs``, update the cargo files as per usual for Rust projects, but also keep ``zynq-rs.nix`` in sync.

License
-------

Copyright (C) 2019-2021 M-Labs Limited.

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
