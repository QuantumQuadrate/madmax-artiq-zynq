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
from misoc.cores import gpio

from artiq.gateware import rtio, nist_clock, nist_qc2
from artiq.gateware.rtio.phy import ttl_simple, ttl_serdes_7series, dds, spi2, edge_counter
from artiq.gateware.rtio.xilinx_clocking import fix_serdes_timing_path
from artiq.gateware.drtio.transceiver import gtx_7series
from artiq.gateware.drtio.siphaser import SiPhaser7Series
from artiq.gateware.drtio.rx_synchronizer import XilinxRXSynchronizer
from artiq.gateware.drtio import *

import dma
import analyzer
import acpki
import drtio_aux_controller
import zynq_clocking
from config import write_csr_file, write_mem_file, write_rustc_cfg_file

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


class CLK200BootstrapClock(Module):
    def __init__(self, platform, freq=125e6):
        self.clock_domains.cd_bootstrap = ClockDomain(reset_less=True)
        self.cd_bootstrap.clk.attr.add("keep")

        clk200 = platform.request("clk200")
        clk200_se = Signal()

        pll_fb = Signal()
        pll_clkout = Signal()
        assert freq in [125e6, 100e6]
        divide = int(1e9/freq)
        self.specials += [
            Instance("IBUFDS",
                i_I=clk200.p, i_IB=clk200.n, o_O=clk200_se),
            Instance("PLLE2_BASE",
                p_CLKIN1_PERIOD=5.0,
                i_CLKIN1=clk200_se,
                i_CLKFBIN=pll_fb,
                o_CLKFBOUT=pll_fb,

                # VCO @ 1GHz
                p_CLKFBOUT_MULT=5, p_DIVCLK_DIVIDE=1,

                # 125MHz/100MHz for bootstrap
                p_CLKOUT1_DIVIDE=divide, p_CLKOUT1_PHASE=0.0, o_CLKOUT1=pll_clkout,
            ),
            Instance("BUFG", i_I=pll_clkout, o_O=self.cd_bootstrap.clk)
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

class ZC706(SoCCore):
    def __init__(self, acpki=False):
        self.acpki = acpki

        platform = zc706.Platform()
        prepare_zc706_platform(platform)

        ident = self.__class__.__name__
        if self.acpki:
            ident = "acpki_" + ident
        SoCCore.__init__(self, platform=platform, csr_data_width=32, ident=ident, ps_cd_sys=False)

        platform.add_extension(si5324_fmc33)
        self.comb += platform.request("si5324_33").rst_n.eq(1)

        cdr_clk = Signal()
        cdr_clk_buf = Signal()
        si5324_out = platform.request("si5324_clkout")
        platform.add_period_constraint(si5324_out.p, 8.0)
        self.specials += [
            Instance("IBUFDS_GTE2",
                i_CEB=0,
                i_I=si5324_out.p, i_IB=si5324_out.n,
                o_O=cdr_clk,
                p_CLKCM_CFG="TRUE",
                p_CLKRCV_TRST="TRUE",
                p_CLKSWING_CFG=3),
            Instance("BUFG", i_I=cdr_clk, o_O=cdr_clk_buf)
        ]
        self.config["HAS_SI5324"] = None
        self.config["SI5324_AS_SYNTHESIZER"] = None
        self.config["SI5324_SOFT_RESET"] = None
        
        self.submodules.bootstrap = CLK200BootstrapClock(platform)
        self.submodules.sys_crg = zynq_clocking.SYSCRG(self.platform, self.ps7, cdr_clk_buf)
        platform.add_false_path_constraints(
            self.bootstrap.cd_bootstrap.clk, self.sys_crg.cd_sys.clk)
        self.csr_devices.append("sys_crg")

    def add_rtio(self, rtio_channels):
        self.submodules.rtio_tsc = rtio.TSC(glbl_fine_ts_width=3)
        self.submodules.rtio_core = rtio.Core(self.rtio_tsc, rtio_channels)
        self.csr_devices.append("rtio_core")

        if self.acpki:
            self.config["KI_IMPL"] = "acp"
            self.submodules.rtio = acpki.KernelInitiator(self.rtio_tsc,
                                                         bus=self.ps7.s_axi_acp,
                                                         user=self.ps7.s_axi_acp_user,
                                                         evento=self.ps7.event.o)
            self.csr_devices.append("rtio")
        else:
            self.config["KI_IMPL"] = "csr"
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

        clk_freq = 100e6 if drtio100mhz else 125e6

        platform = zc706.Platform()
        prepare_zc706_platform(platform)
        ident = self.__class__.__name__
        if self.acpki:
            ident = "acpki_" + ident
        SoCCore.__init__(self, platform=platform, csr_data_width=32, ident=ident, ps_cd_sys=False)

        platform.add_extension(si5324_fmc33)

        self.comb += platform.request("sfp_tx_disable_n").eq(1)
        data_pads = [
            platform.request("sfp"),
            platform.request("user_sma_mgt")
        ]

        self.submodules += SMAClkinForward(self.platform)

        # 1000BASE_BX10 Ethernet compatible, 125MHz RTIO clock
        self.submodules.gt_drtio = gtx_7series.GTX(
            clock_pads=platform.request("si5324_clkout"),
            pads=data_pads,
            clk_freq=clk_freq)
        self.csr_devices.append("gt_drtio")

        self.submodules.rtio_tsc = rtio.TSC(glbl_fine_ts_width=3)
        ext_async_rst = Signal()
        txout_buf = Signal()
        gtx0 = self.gt_drtio.gtxs[0]
        self.specials += Instance("BUFG", i_I=gtx0.txoutclk, o_O=txout_buf)
        self.submodules.bootstrap = CLK200BootstrapClock(platform, clk_freq)
        self.submodules.sys_crg = zynq_clocking.SYSCRG(
            self.platform, 
            self.ps7, 
            txout_buf,
            clk_sw=self.gt_drtio.stable_clkin.storage,
            clk_sw_status=gtx0.tx_init.done,
            ext_async_rst=ext_async_rst,
            freq=clk_freq)
        platform.add_false_path_constraints(
            self.bootstrap.cd_bootstrap.clk, self.sys_crg.cd_sys.clk)
        self.csr_devices.append("sys_crg")

        self.comb += ext_async_rst.eq(self.sys_crg.clk_sw_fsm.o_clk_sw & ~gtx0.tx_init.done)
        self.specials += MultiReg(self.sys_crg.clk_sw_fsm.o_clk_sw & self.sys_crg.mmcm_locked, self.gt_drtio.clk_path_ready, odomain="bootstrap")

        drtio_csr_group = []
        drtioaux_csr_group = []
        drtioaux_memory_group = []
        self.drtio_cri = []
        for i in range(len(self.gt_drtio.channels)):
            core_name = "drtio" + str(i)
            coreaux_name = "drtioaux" + str(i)
            memory_name = "drtioaux" + str(i) + "_mem"
            drtio_csr_group.append(core_name)
            drtioaux_csr_group.append(coreaux_name)
            drtioaux_memory_group.append(memory_name)

            cdr = ClockDomainsRenamer({"rtio_rx": "rtio_rx" + str(i)})

            core = cdr(DRTIOMaster(
                self.rtio_tsc, self.gt_drtio.channels[i]))
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
        self.config["HAS_DRTIO"] = None
        self.config["HAS_DRTIO_ROUTING"] = None
        self.add_csr_group("drtio", drtio_csr_group)
        self.add_csr_group("drtioaux", drtioaux_csr_group)
        self.add_memory_group("drtioaux_mem", drtioaux_memory_group)

        self.config["RTIO_FREQUENCY"] = str(self.gt_drtio.rtio_clk_freq/1e6)

        self.submodules.si5324_rst_n = gpio.GPIOOut(platform.request("si5324_33").rst_n)
        self.csr_devices.append("si5324_rst_n")
        self.config["HAS_SI5324"] = None
        self.config["SI5324_AS_SYNTHESIZER"] = None

        # Constrain TX & RX timing for the first transceiver channel
        # (First channel acts as master for phase alignment for all channels' TX)
        platform.add_false_path_constraints(
            gtx0.txoutclk, gtx0.rxoutclk)
        # Constrain RX timing for the each transceiver channel
        # (Each channel performs single-lane phase alignment for RX)
        for gtx in self.gt_drtio.gtxs[1:]:
            platform.add_false_path_constraints(
                gtx0.txoutclk, gtx.rxoutclk)

        fix_serdes_timing_path(platform)

    def add_rtio(self, rtio_channels):
        self.submodules.rtio_tsc = rtio.TSC(glbl_fine_ts_width=3)
        self.submodules.rtio_core = rtio.Core(self.rtio_tsc, rtio_channels)
        self.csr_devices.append("rtio_core")

        if self.acpki:
            self.config["KI_IMPL"] = "acp"
            self.submodules.rtio = acpki.KernelInitiator(self.rtio_tsc,
                                                         bus=self.ps7.s_axi_acp,
                                                         user=self.ps7.s_axi_acp_user,
                                                         evento=self.ps7.event.o)
            self.csr_devices.append("rtio")
        else:
            self.config["KI_IMPL"] = "csr"
            self.submodules.rtio = rtio.KernelInitiator(self.rtio_tsc, now64=True)
            self.csr_devices.append("rtio")

        self.submodules.rtio_dma = dma.DMA(self.ps7.s_axi_hp0)
        self.csr_devices.append("rtio_dma")

        self.submodules.cri_con = rtio.CRIInterconnectShared(
            [self.rtio.cri, self.rtio_dma.cri],
            [self.rtio_core.cri] + self.drtio_cri,
            enable_routing=True)
        self.csr_devices.append("cri_con")

        self.submodules.rtio_moninj = rtio.MonInj(rtio_channels)
        self.csr_devices.append("rtio_moninj")

        self.submodules.rtio_analyzer = analyzer.Analyzer(self.rtio_tsc, self.rtio_core.cri,
                                                          self.ps7.s_axi_hp1)
        self.csr_devices.append("rtio_analyzer")

        self.submodules.routing_table = rtio.RoutingTableAccess(self.cri_con)
        self.csr_devices.append("routing_table")


class _SatelliteBase(SoCCore):
    def __init__(self, acpki=False, drtio100mhz=False):
        self.acpki = acpki

        clk_freq = 100e6 if drtio100mhz else 125e6

        platform = zc706.Platform()
        prepare_zc706_platform(platform)
        ident = self.__class__.__name__
        if self.acpki:
            ident = "acpki_" + ident
        SoCCore.__init__(self, platform=platform, csr_data_width=32, ident=ident, ps_cd_sys=False)

        platform.add_extension(si5324_fmc33)

        # SFP
        self.comb += platform.request("sfp_tx_disable_n").eq(0)
        data_pads = [
            platform.request("sfp"),
            platform.request("user_sma_mgt")
        ]

        self.submodules.rtio_tsc = rtio.TSC(glbl_fine_ts_width=3)

        # 1000BASE_BX10 Ethernet compatible, 125MHz RTIO clock
        self.submodules.gt_drtio = gtx_7series.GTX(
            clock_pads=platform.request("si5324_clkout"),
            pads=data_pads,
            clk_freq=clk_freq)
        self.csr_devices.append("gt_drtio")

        ext_async_rst = Signal()
        txout_buf = Signal()
        txout_buf.attr.add("keep")
        gtx0 = self.gt_drtio.gtxs[0]
        self.specials += Instance(
            "BUFG", 
            i_I=gtx0.txoutclk, 
            o_O=txout_buf)
        self.submodules.bootstrap = CLK200BootstrapClock(platform, clk_freq)
        self.submodules.sys_crg = zynq_clocking.SYSCRG(
            self.platform, 
            self.ps7, 
            txout_buf,
            clk_sw=self.gt_drtio.stable_clkin.storage,
            clk_sw_status=gtx0.tx_init.done,
            ext_async_rst=ext_async_rst,
            freq=clk_freq)
        platform.add_false_path_constraints(
            self.bootstrap.cd_bootstrap.clk, self.sys_crg.cd_sys.clk)
        self.csr_devices.append("sys_crg")

        self.comb += ext_async_rst.eq(self.sys_crg.clk_sw_fsm.o_clk_sw & ~gtx0.tx_init.done)
        self.specials += MultiReg(self.sys_crg.clk_sw_fsm.o_clk_sw & self.sys_crg.mmcm_locked, self.gt_drtio.clk_path_ready, odomain="bootstrap")

        drtioaux_csr_group = []
        drtioaux_memory_group = []
        drtiorep_csr_group = []
        self.drtio_cri = []
        for i in range(len(self.gt_drtio.channels)):
            coreaux_name = "drtioaux" + str(i)
            memory_name = "drtioaux" + str(i) + "_mem"
            drtioaux_csr_group.append(coreaux_name)
            drtioaux_memory_group.append(memory_name)

            cdr = ClockDomainsRenamer({"rtio_rx": "rtio_rx" + str(i)})

            # Satellite
            if i == 0:
                self.submodules.rx_synchronizer = cdr(XilinxRXSynchronizer())
                core = cdr(DRTIOSatellite(
                    self.rtio_tsc, self.gt_drtio.channels[0], self.rx_synchronizer))
                self.submodules.drtiosat = core
                self.csr_devices.append("drtiosat")
            # Repeaters
            else:
                corerep_name = "drtiorep" + str(i-1)
                drtiorep_csr_group.append(corerep_name)
                core = cdr(DRTIORepeater(
                    self.rtio_tsc, self.gt_drtio.channels[i]))
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
        self.config["HAS_DRTIO"] = None
        self.config["HAS_DRTIO_ROUTING"] = None
        self.add_csr_group("drtioaux", drtioaux_csr_group)
        self.add_csr_group("drtiorep", drtiorep_csr_group)
        self.add_memory_group("drtioaux_mem", drtioaux_memory_group)

        self.config["RTIO_FREQUENCY"] = str(self.gt_drtio.rtio_clk_freq/1e6)

        # Si5324 Phaser
        self.submodules.siphaser = SiPhaser7Series(
            si5324_clkin=platform.request("si5324_clkin"),
            rx_synchronizer=self.rx_synchronizer,
            ultrascale=False,
            rtio_clk_freq=self.gt_drtio.rtio_clk_freq)
        platform.add_false_path_constraints(
            self.sys_crg.cd_sys.clk, self.siphaser.mmcm_freerun_output)
        self.csr_devices.append("siphaser")
        self.submodules.si5324_rst_n = gpio.GPIOOut(platform.request("si5324_33").rst_n)
        self.csr_devices.append("si5324_rst_n")
        self.config["HAS_SI5324"] = None

        rtio_clk_period = 1e9/self.gt_drtio.rtio_clk_freq
        # Constrain TX & RX timing for the first transceiver channel
        # (First channel acts as master for phase alignment for all channels' TX)
        platform.add_false_path_constraints(
            gtx0.txoutclk, gtx0.rxoutclk)
        # Constrain RX timing for the each transceiver channel
        # (Each channel performs single-lane phase alignment for RX)
        for gtx in self.gt_drtio.gtxs[1:]:
            platform.add_false_path_constraints(
                self.sys_crg.cd_sys.clk, gtx.rxoutclk)

        fix_serdes_timing_path(platform)

    def add_rtio(self, rtio_channels):
        self.submodules.rtio_moninj = rtio.MonInj(rtio_channels)
        self.csr_devices.append("rtio_moninj")

        if self.acpki:
            self.config["KI_IMPL"] = "acp"
            self.submodules.rtio = acpki.KernelInitiator(self.rtio_tsc,
                                                         bus=self.ps7.s_axi_acp,
                                                         user=self.ps7.s_axi_acp_user,
                                                         evento=self.ps7.event.o)
            self.csr_devices.append("rtio")
        else:
            self.config["KI_IMPL"] = "csr"
            self.submodules.rtio = rtio.KernelInitiator(self.rtio_tsc, now64=True)
            self.csr_devices.append("rtio")

        self.submodules.rtio_dma = dma.DMA(self.ps7.s_axi_hp0)
        self.csr_devices.append("rtio_dma")

        self.submodules.local_io = SyncRTIO(self.rtio_tsc, rtio_channels)
        self.comb += [
            self.drtiosat.async_errors.eq(self.local_io.async_errors),
            self.local_io.sed_spread_enable.eq(self.drtiosat.sed_spread_enable.storage)
        ]
        self.submodules.cri_con = rtio.CRIInterconnectShared(
            [self.drtiosat.cri, self.rtio_dma.cri, self.rtio.cri],
            [self.local_io.cri] + self.drtio_cri,
            enable_routing=True)
        self.csr_devices.append("cri_con")

        self.submodules.rtio_analyzer = analyzer.Analyzer(self.rtio_tsc, self.local_io.cri,
                                                          self.ps7.s_axi_hp1)
        self.csr_devices.append("rtio_analyzer")

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
        self.submodules += SMAClkinForward(self.platform)
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
        self.submodules += SMAClkinForward(self.platform)
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
