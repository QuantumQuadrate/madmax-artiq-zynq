#!/usr/bin/env python

import argparse

import analyzer
import dma
from artiq.gateware import rtio
from artiq.gateware.rtio.phy import spi2, ttl_simple
from artiq.gateware.rtio.xilinx_clocking import fix_serdes_timing_path
from config import generate_ident, write_csr_file, write_mem_file, write_rustc_cfg_file
from migen import *
from migen.build.generic_platform import IOStandard, Misc, Pins, Subsignal
from migen.build.platforms import ebaz4205
from migen_axi.integration.soc_core import SoCCore
from misoc.interconnect.csr import *

_ps = [
    (
        "ps",
        0,
        Subsignal("clk", Pins("E7"), IOStandard("LVCMOS33"), Misc("SLEW=FAST")),
        Subsignal("por_b", Pins("C7"), IOStandard("LVCMOS33"), Misc("SLEW=FAST")),
        Subsignal("srst_b", Pins("B10"), IOStandard("LVCMOS18"), Misc("SLEW=FAST")),
    )
]

_ddr = [
    (
        "ddr",
        0,
        Subsignal(
            "a",
            Pins("N2 K2 M3 K3 M4 L1 L4 K4 K1 J4 F5 G4 E4 D4 F4"),
            IOStandard("SSTL15"),
        ),
        Subsignal("ba", Pins("L5 R4 J5"), IOStandard("SSTL15")),
        Subsignal("cas_n", Pins("P5"), IOStandard("SSTL15")),
        Subsignal("cke", Pins("N3"), IOStandard("SSTL15")),
        Subsignal("cs_n", Pins("N1"), IOStandard("SSTL15")),
        Subsignal("ck_n", Pins("M2"), IOStandard("DIFF_SSTL15"), Misc("SLEW=FAST")),
        Subsignal("ck_p", Pins("L2"), IOStandard("DIFF_SSTL15"), Misc("SLEW=FAST")),
        # Pins "T1 Y1" not connected
        Subsignal("dm", Pins("A1 F1"), IOStandard("SSTL15_T_DCI"), Misc("SLEW=FAST")),
        Subsignal(
            "dq",
            Pins("C3 B3 A2 A4 D3 D1 C1 E1 E2 E3 G3 H3 J3 H2 H1 J1"),
            # Pins "P1 P3 R3 R1 T4 U4 U2 U3 V1 Y3 W1 Y4 Y2 W3 V2 V3" not connected
            IOStandard("SSTL15_T_DCI"),
            Misc("SLEW=FAST"),
        ),
        Subsignal(
            "dqs_n",
            Pins("B2 F2"),  # Pins "T2 W4" not connected
            IOStandard("DIFF_SSTL15_T_DCI"),
            Misc("SLEW=FAST"),
        ),
        Subsignal(
            "dqs_p",
            Pins("C2 G2"),  # Pins "R2 W5" not connected
            IOStandard("DIFF_SSTL15_T_DCI"),
            Misc("SLEW=FAST"),
        ),
        Subsignal("vrn", Pins("G5"), IOStandard("SSTL15_T_DCI"), Misc("SLEW=FAST")),
        Subsignal("vrp", Pins("H5"), IOStandard("SSTL15_T_DCI"), Misc("SLEW=FAST")),
        Subsignal("drst_n", Pins("B4"), IOStandard("SSTL15"), Misc("SLEW=FAST")),
        Subsignal("odt", Pins("N5"), IOStandard("SSTL15")),
        Subsignal("ras_n", Pins("P4"), IOStandard("SSTL15")),
        Subsignal("we_n", Pins("M5"), IOStandard("SSTL15")),
    )
]

# Connector J3
_i2c = [
    (
        "i2c",
        0,
        Subsignal("scl", Pins("U12"), IOStandard("LVCMOS33")),
        Subsignal("sda", Pins("V13"), IOStandard("LVCMOS33")),
    )
]

_spi = [
    (
        "spi",
        0,
        Subsignal("clk", Pins("V20")),
        Subsignal("mosi", Pins("U20")),
        Subsignal("cs_n", Pins("P19")),
        IOStandard("LVCMOS33"),
    )
]


# Connector DATA1
def _create_ttl():
    _ttl = []

    for idx, elem in enumerate([x for x in range(5, 21) if x not in (10, 12)]):
        _ttl.append(
            ("ttl", idx, Pins("DATA1:DATA1-{}".format(elem)), IOStandard("LVCMOS33")),
        )
    return _ttl


class EBAZ4205(SoCCore):
    def __init__(self, rtio_clk=125e6, acpki=False, acpki_batch_size=1e5):
        self.acpki = acpki

        platform = ebaz4205.Platform()
        platform.toolchain.bitstream_commands.extend(
            [
                "set_property BITSTREAM.GENERAL.COMPRESS True [current_design]",
            ]
        )
        platform.add_extension(_ps)
        platform.add_extension(_ddr)
        platform.add_extension(_i2c)
        platform.add_extension(_spi)
        platform.add_extension(_create_ttl())

        gmii = platform.request("gmii")
        platform.add_period_constraint(gmii.rx_clk, 10)
        platform.add_period_constraint(gmii.tx_clk, 10)
        platform.add_platform_command(
            "set_property CLOCK_DEDICATED_ROUTE FALSE [get_nets gmii_tx_clk_IBUF]"
        )

        ident = generate_ident(self.__class__.__name__)
        if self.acpki:
            ident = "acpki_" + ident
        SoCCore.__init__(self, platform=platform, csr_data_width=32, ident=ident)
        fix_serdes_timing_path(platform)
        self.config["RTIO_FREQUENCY"] = str(rtio_clk / 1e6)
        platform.add_period_constraint(self.ps7.cd_sys.clk, 10)

        self.comb += [
            self.ps7.enet0.enet.gmii.tx_clk.eq(gmii.tx_clk),
            self.ps7.enet0.enet.gmii.rx_clk.eq(gmii.rx_clk),
        ]
        self.clock_domains.cd_eth_rx = ClockDomain(reset_less=False)
        self.clock_domains.cd_eth_tx = ClockDomain(reset_less=False)
        self.comb += [
            ClockSignal("eth_rx").eq(gmii.rx_clk),
            ClockSignal("eth_tx").eq(gmii.tx_clk),
        ]
        self.sync.eth_tx += [
            gmii.txd.eq(self.ps7.enet0.enet.gmii.txd),
            gmii.tx_en.eq(self.ps7.enet0.enet.gmii.tx_en),
        ]
        self.sync.eth_rx += [
            self.ps7.enet0.enet.gmii.rxd.eq(gmii.rxd),
            self.ps7.enet0.enet.gmii.rx_dv.eq(gmii.rx_dv),
        ]

        # MDIO
        mdio = platform.request("mdio")
        self.comb += mdio.mdc.eq(self.ps7.enet0.enet.mdio.mdc)
        self.specials += Instance(
            "IOBUF",
            i_I=self.ps7.enet0.enet.mdio.o,
            io_IO=mdio.mdio,
            o_O=self.ps7.enet0.enet.mdio.i,
            i_T=~self.ps7.enet0.enet.mdio.t_n,
        )

        # I2C
        i2c = self.platform.request("i2c")
        self.specials += [
            # SCL
            Instance(
                "IOBUF",
                i_I=self.ps7.i2c0.scl.o,
                io_IO=i2c.scl,
                o_O=self.ps7.i2c0.scl.i,
                i_T=~self.ps7.i2c0.scl.t_n,
            ),
            # SDA
            Instance(
                "IOBUF",
                i_I=self.ps7.i2c0.sda.o,
                io_IO=i2c.sda,
                o_O=self.ps7.i2c0.sda.i,
                i_T=~self.ps7.i2c0.sda.t_n,
            ),
        ]

        self.rtio_channels = []
        for i in (0, 1):
            print("USER LED at RTIO channel 0x{:06x}".format(len(self.rtio_channels)))
            user_led = self.platform.request("user_led", i)
            phy = ttl_simple.Output(user_led)
            self.submodules += phy
            self.rtio_channels.append(rtio.Channel.from_phy(phy))

        for i in range(14):
            print("TTL at RTIO channel 0x{:06x}".format(len(self.rtio_channels)))
            ttl = self.platform.request("ttl", i)
            phy = ttl_simple.InOut(ttl)
            self.submodules += phy
            self.rtio_channels.append(rtio.Channel.from_phy(phy))

        print("SPI at RTIO channel 0x{:06x}".format(len(self.rtio_channels)))
        spi_phy = spi2.SPIMaster(platform.request("spi"))
        self.submodules += spi_phy
        self.rtio_channels.append(rtio.Channel.from_phy(spi_phy, ififo_depth=4))
        self.config["RTIO_LOG_CHANNEL"] = len(self.rtio_channels)
        self.rtio_channels.append(rtio.LogChannel())

        self.submodules.rtio_tsc = rtio.TSC(glbl_fine_ts_width=3)
        self.submodules.rtio_core = rtio.Core(self.rtio_tsc, self.rtio_channels)
        self.csr_devices.append("rtio_core")
        if self.acpki:
            import acpki

            self.config["KI_IMPL"] = "acp"
            self.config["ACPKI_BATCH_SIZE"] = int(acpki_batch_size)
            self.submodules.rtio = acpki.KernelInitiator(
                self.rtio_tsc,
                bus=self.ps7.s_axi_acp,
                user=self.ps7.s_axi_acp_user,
                evento=self.ps7.event.o,
            )
            self.csr_devices.append("rtio")
        else:
            self.config["KI_IMPL"] = "csr"
            self.submodules.rtio = rtio.KernelInitiator(self.rtio_tsc, now64=True)
            self.csr_devices.append("rtio")

        self.submodules.rtio_dma = dma.DMA(self.ps7.s_axi_hp0)
        self.csr_devices.append("rtio_dma")

        self.submodules.cri_con = rtio.CRIInterconnectShared(
            [self.rtio.cri, self.rtio_dma.cri],
            [self.rtio_core.cri],
            enable_routing=True,
        )
        self.csr_devices.append("cri_con")

        self.submodules.rtio_moninj = rtio.MonInj(self.rtio_channels)
        self.csr_devices.append("rtio_moninj")

        self.submodules.rtio_analyzer = analyzer.Analyzer(
            self.rtio_tsc, self.rtio_core.cri, self.ps7.s_axi_hp1
        )
        self.csr_devices.append("rtio_analyzer")


class BASE(EBAZ4205):
    def __init__(self, rtio_clk, acpki, acpki_batch_size):
        EBAZ4205.__init__(self, rtio_clk, acpki, acpki_batch_size)


VARIANTS = {cls.__name__.lower(): cls for cls in [BASE]}


def main():
    parser = argparse.ArgumentParser(
        description="ARTIQ port to the EBAZ4205 control card of Ebit E9+ BTC miner"
    )
    parser.add_argument(
        "-r", default=None, help="build Rust interface into the specified file"
    )
    parser.add_argument(
        "-m", default=None, help="build Rust memory interface into the specified file"
    )
    parser.add_argument(
        "-c",
        default=None,
        help="build Rust compiler configuration into the specified file",
    )
    parser.add_argument(
        "-g", default=None, help="build gateware into the specified directory"
    )
    parser.add_argument("--rtio-clk", default=125e6, help="RTIO Clock Frequency (Hz)")
    parser.add_argument("--acpki-batch-size", default=10000, help="ACPKI batch buffer size")
    parser.add_argument(
        "-V",
        "--variant",
        default="base",
        help="variant: " "[acpki_]base" "(default: %(default)s)",
    )
    args = parser.parse_args()

    rtio_clk = int(args.rtio_clk)
    variant = args.variant.lower()
    acpki = variant.startswith("acpki_")
    if acpki:
        variant = variant[6:]

    try:
        cls = VARIANTS[variant]
    except KeyError:
        raise SystemExit("Invalid variant (-V/--variant)")

    soc = cls(rtio_clk=rtio_clk, acpki=acpki, acpki_batch_size=args.acpki_batch_size)
    soc.finalize()

    if args.r is not None:
        write_csr_file(soc, args.r)
    if args.m is not None:
        write_mem_file(soc, args.m)
    if args.c is not None:
        write_rustc_cfg_file(soc, args.c)
    if args.g is not None:
        soc.build(build_dir=args.g)


if __name__ == "__main__":
    main()
