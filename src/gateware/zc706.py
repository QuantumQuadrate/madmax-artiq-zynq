#!/usr/bin/env python

import argparse
from operator import itemgetter

from migen import *
from migen.build.generic_platform import *
from migen.genlib.resetsync import AsyncResetSynchronizer
from migen.genlib.cdc import MultiReg
from migen_axi.integration.soc_core import SoCCore
from migen_axi.platforms import zc706
from misoc.interconnect.csr import *
from misoc.integration import cpu_interface
from misoc.cores import gpio

from artiq.gateware import rtio, nist_clock, nist_qc2
from artiq.gateware.rtio.phy import ttl_simple, ttl_serdes_7series, dds, spi2, edge_counter
from artiq.gateware.rtio.xilinx_clocking import RTIOClockMultiplier, fix_serdes_timing_path
from artiq.gateware.drtio.transceiver import gtx_7series
from artiq.gateware.drtio.siphaser import SiPhaser7Series
from artiq.gateware.drtio.rx_synchronizer import XilinxRXSynchronizer
from artiq.gateware.drtio import *

import dma
import analyzer
import acpki
import drtio_aux_controller


class RTIOCRG(Module, AutoCSR):
    def __init__(self, platform, rtio_internal_clk):
        self.clock_sel = CSRStorage()
        self.pll_reset = CSRStorage(reset=1)
        self.pll_locked = CSRStatus()
        self.clock_domains.cd_rtio = ClockDomain()
        self.clock_domains.cd_rtiox4 = ClockDomain(reset_less=True)

        rtio_external_clk = Signal()
        user_sma_clock = platform.request("user_sma_clock")
        platform.add_period_constraint(user_sma_clock.p, 8.0)
        self.specials += Instance("IBUFDS",
                                  i_I=user_sma_clock.p, i_IB=user_sma_clock.n,
                                  o_O=rtio_external_clk)

        pll_locked = Signal()
        rtio_clk = Signal()
        rtiox4_clk = Signal()
        self.specials += [
            Instance("PLLE2_ADV",
                     p_STARTUP_WAIT="FALSE", o_LOCKED=pll_locked,

                     p_REF_JITTER1=0.01,
                     p_CLKIN1_PERIOD=8.0, p_CLKIN2_PERIOD=8.0,
                     i_CLKIN1=rtio_internal_clk, i_CLKIN2=rtio_external_clk,
                     # Warning: CLKINSEL=0 means CLKIN2 is selected
                     i_CLKINSEL=~self.clock_sel.storage,

                     # VCO @ 1GHz when using 125MHz input
                     p_CLKFBOUT_MULT=8, p_DIVCLK_DIVIDE=1,
                     i_CLKFBIN=self.cd_rtio.clk,
                     i_RST=self.pll_reset.storage,

                     o_CLKFBOUT=rtio_clk,

                     p_CLKOUT0_DIVIDE=2, p_CLKOUT0_PHASE=0.0,
                     o_CLKOUT0=rtiox4_clk),
            Instance("BUFG", i_I=rtio_clk, o_O=self.cd_rtio.clk),
            Instance("BUFG", i_I=rtiox4_clk, o_O=self.cd_rtiox4.clk),
            AsyncResetSynchronizer(self.cd_rtio, ~pll_locked),
            MultiReg(pll_locked, self.pll_locked.status)
        ]


class SMAClkinForward(Module):
    def __init__(self, platform):
        sma_clkin = platform.request("user_sma_clock")
        sma_clkin_se = Signal()
        si5324_clkin_se = Signal()
        si5324_clkin = platform.request("si5324_clkin")
        self.specials += [
            Instance("IBUFDS", i_I=sma_clkin.p, i_IB=sma_clkin.n, o_O=sma_clkin_se),
            Instance("ODDR", i_C=sma_clkin_se, i_CE=1, i_D1=1, i_D2=0, o_Q=si5324_clkin_se),
            Instance("OBUFDS", i_I=si5324_clkin_se, o_O=si5324_clkin.p, o_OB=si5324_clkin.n)
        ]


# The NIST backplanes require setting VADJ to 3.3V by reprogramming the power supply.
# This also changes the I/O standard for some on-board LEDs.
leds_fmc33 = [
    ("user_led_33", 0, Pins("Y21"), IOStandard("LVCMOS33")),
    ("user_led_33", 1, Pins("G2"), IOStandard("LVCMOS15")),
    ("user_led_33", 2, Pins("W21"), IOStandard("LVCMOS33")),
    ("user_led_33", 3, Pins("A17"), IOStandard("LVCMOS15")),
]

# same deal as with LEDs - changed I/O standard.
si5324_fmc33 = [
    ("si5324_33", 0,
        Subsignal("rst_n", Pins("W23"), IOStandard("LVCMOS33")),
        Subsignal("int", Pins("AJ25"), IOStandard("LVCMOS33"))
    ),
]

pmod1_33 = [
    ("pmod1_33", 0, Pins("AJ21"), IOStandard("LVCMOS33")),
    ("pmod1_33", 1, Pins("AK21"), IOStandard("LVCMOS33")),
    ("pmod1_33", 2, Pins("AB21"), IOStandard("LVCMOS33")),
    ("pmod1_33", 3, Pins("AB16"), IOStandard("LVCMOS33")),
    # rest removed for use with dummy spi
]

_ams101_dac = [
    ("ams101_dac", 0,
        Subsignal("ldac", Pins("XADC:GPIO0")),
        Subsignal("clk", Pins("XADC:GPIO1")),
        Subsignal("mosi", Pins("XADC:GPIO2")),
        Subsignal("cs_n", Pins("XADC:GPIO3")),
        IOStandard("LVCMOS15")
     )
]

_pmod_spi = [ 
    ("pmod_spi", 0,
        # PMOD_1 4-7 pins, same bank as sfp_tx_disable or user_sma_clock
        Subsignal("miso", Pins("Y20"), IOStandard("LVCMOS25")),
        Subsignal("clk", Pins("AA20"), IOStandard("LVCMOS25")),
        Subsignal("mosi", Pins("AC18"), IOStandard("LVCMOS25")),
        Subsignal("cs_n", Pins("AC19"), IOStandard("LVCMOS25")),
        IOStandard("LVCMOS25")
    )
]


def prepare_zc706_platform(platform):
    platform.toolchain.bitstream_commands.extend([
        "set_property BITSTREAM.GENERAL.COMPRESS True [current_design]",
    ])
    platform.add_platform_command("create_clock -name clk_fpga_0 -period 8 [get_pins \"PS7/FCLKCLK[0]\"]")
    platform.add_platform_command("set_input_jitter clk_fpga_0 0.24")


class ZC706(SoCCore):
    def __init__(self, acpki=False):
        self.acpki = acpki
        self.rustc_cfg = dict()

        platform = zc706.Platform()
        prepare_zc706_platform(platform)

        ident = self.__class__.__name__
        if self.acpki:
            ident = "acpki_" + ident
        SoCCore.__init__(self, platform=platform, csr_data_width=32, ident=ident)

        self.submodules.rtio_crg = RTIOCRG(self.platform, self.ps7.cd_sys.clk)
        self.csr_devices.append("rtio_crg")
        self.rustc_cfg["has_rtio_crg_clock_sel"] = None
        self.platform.add_period_constraint(self.rtio_crg.cd_rtio.clk, 8.)
        self.platform.add_false_path_constraints(
            self.ps7.cd_sys.clk,
            self.rtio_crg.cd_rtio.clk)

    def add_rtio(self, rtio_channels):
        self.submodules.rtio_tsc = rtio.TSC("async", glbl_fine_ts_width=3)
        self.submodules.rtio_core = rtio.Core(self.rtio_tsc, rtio_channels)
        self.csr_devices.append("rtio_core")

        if self.acpki:
            self.rustc_cfg["ki_impl"] = "acp"
            self.submodules.rtio = acpki.KernelInitiator(self.rtio_tsc,
                                                         bus=self.ps7.s_axi_acp,
                                                         user=self.ps7.s_axi_acp_user,
                                                         evento=self.ps7.event.o)
            self.csr_devices.append("rtio")
        else:
            self.rustc_cfg["ki_impl"] = "csr"
            self.submodules.rtio = rtio.KernelInitiator(self.rtio_tsc, now64=True)
            self.csr_devices.append("rtio")

        self.submodules.rtio_dma = dma.DMA(self.ps7.s_axi_hp0)
        self.csr_devices.append("rtio_dma")

        self.submodules.cri_con = rtio.CRIInterconnectShared(
            [self.rtio.cri, self.rtio_dma.cri],
            [self.rtio_core.cri])
        self.csr_devices.append("cri_con")

        self.submodules.rtio_moninj = rtio.MonInj(rtio_channels)
        self.csr_devices.append("rtio_moninj")

        self.submodules.rtio_analyzer = analyzer.Analyzer(self.rtio_tsc, self.rtio_core.cri,
                                                          self.ps7.s_axi_hp1)
        self.csr_devices.append("rtio_analyzer")


class _MasterBase(SoCCore):
    def __init__(self, acpki=False, drtio100mhz=False):
        self.acpki = acpki
        self.rustc_cfg = dict()

        platform = zc706.Platform()
        prepare_zc706_platform(platform)
        ident = self.__class__.__name__
        if self.acpki:
            ident = "acpki_" + ident
        SoCCore.__init__(self, platform=platform, csr_data_width=32, ident=ident)

        platform.add_extension(si5324_fmc33)

        self.sys_clk_freq = 125e6
        rtio_clk_freq = 100e6 if drtio100mhz else self.sys_clk_freq

        platform = self.platform

        self.comb += platform.request("sfp_tx_disable_n").eq(1)
        data_pads = [
            platform.request("sfp"),
            platform.request("user_sma_mgt")
        ]

        self.submodules += SMAClkinForward(self.platform)

        # 1000BASE_BX10 Ethernet compatible, 125MHz RTIO clock
        self.submodules.drtio_transceiver = gtx_7series.GTX(
            clock_pads=platform.request("si5324_clkout"),
            pads=data_pads,
            sys_clk_freq=self.sys_clk_freq,
            rtio_clk_freq=rtio_clk_freq)
        self.csr_devices.append("drtio_transceiver")

        self.submodules.rtio_tsc = rtio.TSC("async", glbl_fine_ts_width=3)

        drtio_csr_group = []
        drtioaux_csr_group = []
        drtioaux_memory_group = []
        self.drtio_cri = []
        for i in range(len(self.drtio_transceiver.channels)):
            core_name = "drtio" + str(i)
            coreaux_name = "drtioaux" + str(i)
            memory_name = "drtioaux" + str(i) + "_mem"
            drtio_csr_group.append(core_name)
            drtioaux_csr_group.append(coreaux_name)
            drtioaux_memory_group.append(memory_name)

            cdr = ClockDomainsRenamer({"rtio_rx": "rtio_rx" + str(i)})

            core = cdr(DRTIOMaster(
                self.rtio_tsc, self.drtio_transceiver.channels[i]))
            setattr(self.submodules, core_name, core)
            self.drtio_cri.append(core.cri)
            self.csr_devices.append(core_name)

            coreaux = cdr(drtio_aux_controller.DRTIOAuxControllerBare(core.link_layer))
            setattr(self.submodules, coreaux_name, coreaux)
            self.csr_devices.append(coreaux_name)

            mem_size = coreaux.get_mem_size()
            memory_address = self.axi2csr.register_port(coreaux.get_tx_port(), mem_size)
            self.axi2csr.register_port(coreaux.get_rx_port(), mem_size)
            self.add_memory_region(memory_name, self.mem_map["csr"] + memory_address, mem_size * 2)
        self.rustc_cfg["has_drtio"] = None
        self.rustc_cfg["has_drtio_routing"] = None
        self.add_csr_group("drtio", drtio_csr_group)
        self.add_csr_group("drtioaux", drtioaux_csr_group)
        self.add_memory_group("drtioaux_mem", drtioaux_memory_group)

        self.rustc_cfg["rtio_frequency"] = str(self.drtio_transceiver.rtio_clk_freq/1e6)

        self.submodules.si5324_rst_n = gpio.GPIOOut(platform.request("si5324_33").rst_n)
        self.csr_devices.append("si5324_rst_n")
        self.rustc_cfg["has_si5324"] = None
        self.rustc_cfg["si5324_as_synthesizer"] = None

        rtio_clk_period = 1e9/self.drtio_transceiver.rtio_clk_freq
        # Constrain TX & RX timing for the first transceiver channel
        # (First channel acts as master for phase alignment for all channels' TX)
        gtx0 = self.drtio_transceiver.gtxs[0]
        platform.add_period_constraint(gtx0.txoutclk, rtio_clk_period)
        platform.add_period_constraint(gtx0.rxoutclk, rtio_clk_period)
        platform.add_false_path_constraints(
            self.ps7.cd_sys.clk,
            gtx0.txoutclk, gtx0.rxoutclk)
        # Constrain RX timing for the each transceiver channel
        # (Each channel performs single-lane phase alignment for RX)
        for gtx in self.drtio_transceiver.gtxs[1:]:
            platform.add_period_constraint(gtx.rxoutclk, rtio_clk_period)
            platform.add_false_path_constraints(
                self.ps7.cd_sys.clk, gtx0.txoutclk, gtx.rxoutclk)

        self.submodules.rtio_crg = RTIOClockMultiplier(self.sys_clk_freq)
        self.csr_devices.append("rtio_crg")
        fix_serdes_timing_path(self.platform)

    def add_rtio(self, rtio_channels):
        self.submodules.rtio_tsc = rtio.TSC("async", glbl_fine_ts_width=3)
        self.submodules.rtio_core = rtio.Core(self.rtio_tsc, rtio_channels)
        self.csr_devices.append("rtio_core")

        if self.acpki:
            self.rustc_cfg["ki_impl"] = "acp"
            self.submodules.rtio = acpki.KernelInitiator(self.rtio_tsc,
                                                         bus=self.ps7.s_axi_acp,
                                                         user=self.ps7.s_axi_acp_user,
                                                         evento=self.ps7.event.o)
            self.csr_devices.append("rtio")
        else:
            self.rustc_cfg["ki_impl"] = "csr"
            self.submodules.rtio = rtio.KernelInitiator(self.rtio_tsc, now64=True)
            self.csr_devices.append("rtio")

        self.submodules.rtio_dma = dma.DMA(self.ps7.s_axi_hp0)
        self.csr_devices.append("rtio_dma")

        self.submodules.cri_con = rtio.CRIInterconnectShared(
            [self.rtio.cri, self.rtio_dma.cri],
            [self.rtio_core.cri] + self.drtio_cri,
            mode="sync", enable_routing=True)
        self.csr_devices.append("cri_con")

        self.submodules.rtio_moninj = rtio.MonInj(rtio_channels)
        self.csr_devices.append("rtio_moninj")

        self.submodules.rtio_analyzer = analyzer.Analyzer(self.rtio_tsc, self.cri_con.switch.slave,
                                                          self.ps7.s_axi_hp1)
        self.csr_devices.append("rtio_analyzer")

        self.submodules.routing_table = rtio.RoutingTableAccess(self.cri_con)
        self.csr_devices.append("routing_table")


class _SatelliteBase(SoCCore):
    def __init__(self, acpki=False, drtio100mhz=False):
        self.acpki = acpki
        self.rustc_cfg = dict()

        platform = zc706.Platform()
        prepare_zc706_platform(platform)
        ident = self.__class__.__name__
        if self.acpki:
            ident = "acpki_" + ident
        SoCCore.__init__(self, platform=platform, csr_data_width=32, ident=ident)

        platform.add_extension(si5324_fmc33)

        self.sys_clk_freq = 125e6
        rtio_clk_freq = 100e6 if drtio100mhz else self.sys_clk_freq
        platform = self.platform

        # SFP
        self.comb += platform.request("sfp_tx_disable_n").eq(0)
        data_pads = [
            platform.request("sfp"),
            platform.request("user_sma_mgt")
        ]

        self.submodules.rtio_tsc = rtio.TSC("sync", glbl_fine_ts_width=3)

        # 1000BASE_BX10 Ethernet compatible, 125MHz RTIO clock
        self.submodules.drtio_transceiver = gtx_7series.GTX(
            clock_pads=platform.request("si5324_clkout"),
            pads=data_pads,
            sys_clk_freq=self.sys_clk_freq,
            rtio_clk_freq=rtio_clk_freq)
        self.csr_devices.append("drtio_transceiver")

        drtioaux_csr_group = []
        drtioaux_memory_group = []
        drtiorep_csr_group = []
        self.drtio_cri = []
        for i in range(len(self.drtio_transceiver.channels)):
            coreaux_name = "drtioaux" + str(i)
            memory_name = "drtioaux" + str(i) + "_mem"
            drtioaux_csr_group.append(coreaux_name)
            drtioaux_memory_group.append(memory_name)

            cdr = ClockDomainsRenamer({"rtio_rx": "rtio_rx" + str(i)})

            # Satellite
            if i == 0:
                self.submodules.rx_synchronizer = cdr(XilinxRXSynchronizer())
                core = cdr(DRTIOSatellite(
                    self.rtio_tsc, self.drtio_transceiver.channels[0], self.rx_synchronizer))
                self.submodules.drtiosat = core
                self.csr_devices.append("drtiosat")
            # Repeaters
            else:
                corerep_name = "drtiorep" + str(i-1)
                drtiorep_csr_group.append(corerep_name)
                core = cdr(DRTIORepeater(
                    self.rtio_tsc, self.drtio_transceiver.channels[i]))
                setattr(self.submodules, corerep_name, core)
                self.drtio_cri.append(core.cri)
                self.csr_devices.append(corerep_name)

            coreaux = cdr(drtio_aux_controller.DRTIOAuxControllerBare(core.link_layer))
            setattr(self.submodules, coreaux_name, coreaux)
            self.csr_devices.append(coreaux_name)

            mem_size = coreaux.get_mem_size()
            tx_port = coreaux.get_tx_port()
            rx_port = coreaux.get_rx_port()
            memory_address = self.axi2csr.register_port(tx_port, mem_size)
            # rcv in upper half of the memory, thus added second
            self.axi2csr.register_port(rx_port, mem_size)
            # and registered in PS interface
            # manually, because software refers to rx/tx by halves of entire memory block, not names
            self.add_memory_region(memory_name, self.mem_map["csr"] + memory_address, mem_size * 2)
        self.rustc_cfg["has_drtio"] = None
        self.rustc_cfg["has_drtio_routing"] = None
        self.add_csr_group("drtioaux", drtioaux_csr_group)
        self.add_csr_group("drtiorep", drtiorep_csr_group)
        self.add_memory_group("drtioaux_mem", drtioaux_memory_group)

        self.rustc_cfg["rtio_frequency"] = str(self.drtio_transceiver.rtio_clk_freq/1e6)

        # Si5324 Phaser
        self.submodules.siphaser = SiPhaser7Series(
            si5324_clkin=platform.request("si5324_clkin"),
            rx_synchronizer=self.rx_synchronizer,
            ultrascale=False,
            rtio_clk_freq=self.drtio_transceiver.rtio_clk_freq)
        platform.add_false_path_constraints(
            self.ps7.cd_sys.clk, self.siphaser.mmcm_freerun_output)
        self.csr_devices.append("siphaser")
        self.submodules.si5324_rst_n = gpio.GPIOOut(platform.request("si5324_33").rst_n)
        self.csr_devices.append("si5324_rst_n")
        self.rustc_cfg["has_si5324"] = None
        self.rustc_cfg["has_siphaser"] = None

        rtio_clk_period = 1e9/self.drtio_transceiver.rtio_clk_freq
        # Constrain TX & RX timing for the first transceiver channel
        # (First channel acts as master for phase alignment for all channels' TX)
        gtx0 = self.drtio_transceiver.gtxs[0]
        platform.add_period_constraint(gtx0.txoutclk, rtio_clk_period)
        platform.add_period_constraint(gtx0.rxoutclk, rtio_clk_period)
        platform.add_false_path_constraints(
            self.ps7.cd_sys.clk,
            gtx0.txoutclk, gtx0.rxoutclk)
        # Constrain RX timing for the each transceiver channel
        # (Each channel performs single-lane phase alignment for RX)
        for gtx in self.drtio_transceiver.gtxs[1:]:
            platform.add_period_constraint(gtx.rxoutclk, rtio_clk_period)
            platform.add_false_path_constraints(
                self.ps7.cd_sys.clk, gtx.rxoutclk)

        self.submodules.rtio_crg = RTIOClockMultiplier(self.sys_clk_freq)
        self.csr_devices.append("rtio_crg")
        self.rustc_cfg["has_rtio_crg"] = None
        fix_serdes_timing_path(self.platform)

    def add_rtio(self, rtio_channels):
        self.submodules.rtio_moninj = rtio.MonInj(rtio_channels)
        self.csr_devices.append("rtio_moninj")

        if self.acpki:
            self.rustc_cfg["ki_impl"] = "acp"
            self.submodules.rtio = acpki.KernelInitiator(self.rtio_tsc,
                                                         bus=self.ps7.s_axi_acp,
                                                         user=self.ps7.s_axi_acp_user,
                                                         evento=self.ps7.event.o)
            self.csr_devices.append("rtio")
        else:
            self.rustc_cfg["ki_impl"] = "csr"
            self.submodules.rtio = rtio.KernelInitiator(self.rtio_tsc, now64=True)
            self.csr_devices.append("rtio")

        self.submodules.local_io = SyncRTIO(self.rtio_tsc, rtio_channels)
        self.submodules.cri_con = rtio.CRIInterconnectShared(
            [self.drtiosat.cri],
            [self.local_io.cri] + self.drtio_cri,
            mode="sync", enable_routing=True)
        self.csr_devices.append("cri_con")

        self.submodules.routing_table = rtio.RoutingTableAccess(self.cri_con)
        self.csr_devices.append("routing_table")



class _NIST_CLOCK_RTIO:
    """
    NIST clock hardware, with old backplane and 11 DDS channels
    """
    def __init__(self):
        platform = self.platform
        platform.add_extension(nist_clock.fmc_adapter_io)
        platform.add_extension(leds_fmc33)
        platform.add_extension(pmod1_33)
        platform.add_extension(_ams101_dac)
        platform.add_extension(_pmod_spi)

        rtio_channels = []

        for i in range(16):
            if i % 4 == 3:
                phy = ttl_serdes_7series.InOut_8X(platform.request("ttl", i))
                self.submodules += phy
                rtio_channels.append(rtio.Channel.from_phy(phy, ififo_depth=512))
            else:
                phy = ttl_serdes_7series.Output_8X(platform.request("ttl", i))
                self.submodules += phy
                rtio_channels.append(rtio.Channel.from_phy(phy))

        for i in range(2):
            phy = ttl_serdes_7series.InOut_8X(platform.request("pmt", i))
            self.submodules += phy
            rtio_channels.append(rtio.Channel.from_phy(phy, ififo_depth=512))

        # no SMA GPIO, replaced with PMOD1_0
        phy = ttl_serdes_7series.InOut_8X(platform.request("pmod1_33", 0))
        self.submodules += phy
        rtio_channels.append(rtio.Channel.from_phy(phy, ififo_depth=512))

        phy = ttl_simple.Output(platform.request("user_led_33", 0))
        self.submodules += phy
        rtio_channels.append(rtio.Channel.from_phy(phy))

        ams101_dac = self.platform.request("ams101_dac", 0)
        phy = ttl_simple.Output(ams101_dac.ldac)
        self.submodules += phy
        rtio_channels.append(rtio.Channel.from_phy(phy))

        phy = ttl_simple.ClockGen(platform.request("la32_p"))
        self.submodules += phy
        rtio_channels.append(rtio.Channel.from_phy(phy))

        phy = spi2.SPIMaster(ams101_dac)
        self.submodules += phy
        rtio_channels.append(rtio.Channel.from_phy(
            phy, ififo_depth=4))

        for i in range(3):
            phy = spi2.SPIMaster(self.platform.request("spi", i))
            self.submodules += phy
            rtio_channels.append(rtio.Channel.from_phy(
                phy, ififo_depth=128))

        # no SDIO on PL side, dummy SPI placeholder instead
        phy = spi2.SPIMaster(platform.request("pmod_spi"))
        self.submodules += phy
        rtio_channels.append(rtio.Channel.from_phy(phy, ififo_depth=4))

        phy = dds.AD9914(platform.request("dds"), 11, onehot=True)
        self.submodules += phy
        rtio_channels.append(rtio.Channel.from_phy(phy, ififo_depth=4))

        self.config["RTIO_LOG_CHANNEL"] = len(rtio_channels)
        rtio_channels.append(rtio.LogChannel())

        self.add_rtio(rtio_channels)


class _NIST_QC2_RTIO:
    """
    NIST QC2 hardware, as used in Quantum I and Quantum II, with new backplane
    and 24 DDS channels.  Two backplanes are used.
    """
    def __init__(self):
        platform = self.platform
        platform.add_extension(nist_qc2.fmc_adapter_io)
        platform.add_extension(leds_fmc33)
        platform.add_extension(_ams101_dac)
        platform.add_extension(pmod1_33)

        rtio_channels = []
        edge_counter_phy = []

        # All TTL channels are In+Out capable
        for i in range(40):
            phy = ttl_serdes_7series.InOut_8X(platform.request("ttl", i))
            self.submodules += phy
            rtio_channels.append(rtio.Channel.from_phy(phy, ififo_depth=512))
            # first four TTLs will also have edge counters
            if i < 4:
                edge_counter_phy.append(phy)

        # no SMA GPIO, replaced with PMOD1_0
        phy = ttl_serdes_7series.InOut_8X(platform.request("pmod1_33", 0))
        self.submodules += phy
        rtio_channels.append(rtio.Channel.from_phy(phy, ififo_depth=512))

        phy = ttl_simple.Output(platform.request("user_led_33", 0))
        self.submodules += phy
        rtio_channels.append(rtio.Channel.from_phy(phy))

        ams101_dac = self.platform.request("ams101_dac", 0)
        phy = ttl_simple.Output(ams101_dac.ldac)
        self.submodules += phy
        rtio_channels.append(rtio.Channel.from_phy(phy))

        # CLK0, CLK1 are for clock generators, on backplane SMP connectors
        for i in range(2):
            phy = ttl_simple.ClockGen(
                platform.request("clkout", i))
            self.submodules += phy
            rtio_channels.append(rtio.Channel.from_phy(phy))
        
        phy = spi2.SPIMaster(ams101_dac)
        self.submodules += phy
        rtio_channels.append(rtio.Channel.from_phy(
            phy, ififo_depth=4))

        for i in range(4):
            phy = spi2.SPIMaster(self.platform.request("spi", i))
            self.submodules += phy
            rtio_channels.append(rtio.Channel.from_phy(
                phy, ififo_depth=128))

        for backplane_offset in range(2):
            phy = dds.AD9914(
                platform.request("dds", backplane_offset), 12, onehot=True)
            self.submodules += phy
            rtio_channels.append(rtio.Channel.from_phy(phy, ififo_depth=4))
        
        for phy in edge_counter_phy:
            counter = edge_counter.SimpleEdgeCounter(phy.input_state)
            self.submodules += counter
            rtio_channels.append(rtio.Channel.from_phy(counter))

        self.config["RTIO_LOG_CHANNEL"] = len(rtio_channels)
        rtio_channels.append(rtio.LogChannel())

        self.add_rtio(rtio_channels)


class NIST_CLOCK(ZC706, _NIST_CLOCK_RTIO):
    def __init__(self, acpki, drtio100mhz):
        ZC706.__init__(self, acpki)
        _NIST_CLOCK_RTIO.__init__(self)

class NIST_CLOCK_Master(_MasterBase, _NIST_CLOCK_RTIO):
    def __init__(self, acpki, drtio100mhz):
        _MasterBase.__init__(self, acpki, drtio100mhz)
        _NIST_CLOCK_RTIO.__init__(self)

class NIST_CLOCK_Satellite(_SatelliteBase, _NIST_CLOCK_RTIO):
    def __init__(self, acpki, drtio100mhz):
        _SatelliteBase.__init__(self, acpki, drtio100mhz)
        _NIST_CLOCK_RTIO.__init__(self)

class NIST_QC2(ZC706, _NIST_QC2_RTIO):
    def __init__(self, acpki, drtio100mhz):
        ZC706.__init__(self, acpki)
        _NIST_QC2_RTIO.__init__(self)

class NIST_QC2_Master(_MasterBase, _NIST_QC2_RTIO):
    def __init__(self, acpki, drtio100mhz):
        _MasterBase.__init__(self, acpki, drtio100mhz)
        _NIST_QC2_RTIO.__init__(self)

class NIST_QC2_Satellite(_SatelliteBase, _NIST_QC2_RTIO):
    def __init__(self, acpki, drtio100mhz):
        _SatelliteBase.__init__(self, acpki, drtio100mhz)
        _NIST_QC2_RTIO.__init__(self)

VARIANTS = {cls.__name__.lower(): cls for cls in [NIST_CLOCK, NIST_CLOCK_Master, NIST_CLOCK_Satellite,
                                                  NIST_QC2, NIST_QC2_Master, NIST_QC2_Satellite]}


def write_csr_file(soc, filename):
    with open(filename, "w") as f:
        f.write(cpu_interface.get_csr_rust(
            soc.get_csr_regions(), soc.get_csr_groups(), soc.get_constants()))

def write_mem_file(soc, filename):
    with open(filename, "w") as f:
        f.write(cpu_interface.get_mem_rust(
            soc.get_memory_regions(), soc.get_memory_groups(), None))


def write_rustc_cfg_file(soc, filename):
    with open(filename, "w") as f:
        for k, v in sorted(soc.rustc_cfg.items(), key=itemgetter(0)):
            if v is None:
                f.write("{}\n".format(k))
            else:
                f.write("{}=\"{}\"\n".format(k, v))


def main():
    parser = argparse.ArgumentParser(
        description="ARTIQ port to the ZC706 Zynq development kit")
    parser.add_argument("-r", default=None,
        help="build Rust interface into the specified file")
    parser.add_argument("-m", default=None,
        help="build Rust memory interface into the specified file")
    parser.add_argument("-c", default=None,
        help="build Rust compiler configuration into the specified file")
    parser.add_argument("-g", default=None,
        help="build gateware into the specified directory")
    parser.add_argument("-V", "--variant", default="nist_clock",
                        help="variant: "
                             "[acpki_]nist_clock/nist_qc2[_master/_satellite][_100mhz]"
                             "(default: %(default)s)")
    args = parser.parse_args()

    variant = args.variant.lower()
    acpki = variant.startswith("acpki_")
    if acpki:
        variant = variant[6:]
    drtio100mhz = variant.endswith("_100mhz")
    if drtio100mhz:
        variant = variant[:-7]
    try:
        cls = VARIANTS[variant]
    except KeyError:
        raise SystemExit("Invalid variant (-V/--variant)")

    soc = cls(acpki=acpki, drtio100mhz=drtio100mhz)
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
