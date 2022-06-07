#!/usr/bin/env python

import argparse
from operator import itemgetter

from migen import *
from migen.build.generic_platform import *
from migen.genlib.resetsync import AsyncResetSynchronizer
from migen.genlib.cdc import MultiReg
from migen_axi.integration.soc_core import SoCCore
from migen_axi.platforms import kasli_soc
from misoc.interconnect.csr import *
from misoc.integration import cpu_interface

from artiq.coredevice import jsondesc
from artiq.gateware import rtio, eem_7series
from artiq.gateware.rtio.phy import ttl_simple
from artiq.gateware.rtio.xilinx_clocking import RTIOClockMultiplier
from artiq.gateware.drtio.transceiver import gtx_7series
from artiq.gateware.drtio.siphaser import SiPhaser7Series
from artiq.gateware.drtio.rx_synchronizer import XilinxRXSynchronizer
from artiq.gateware.drtio import *

import dma
import analyzer
import acpki
import drtio_aux_controller

class RTIOCRG(Module, AutoCSR):
    def __init__(self, platform):
        self.pll_reset = CSRStorage(reset=1)
        self.pll_locked = CSRStatus()
        self.clock_domains.cd_rtio = ClockDomain()
        self.clock_domains.cd_rtiox4 = ClockDomain(reset_less=True)

        clk_synth = platform.request("cdr_clk_clean_fabric")
        clk_synth_se = Signal()
        platform.add_period_constraint(clk_synth.p, 8.0)
        self.specials += [
            Instance("IBUFGDS",
                p_DIFF_TERM="TRUE", p_IBUF_LOW_PWR="FALSE",
                i_I=clk_synth.p, i_IB=clk_synth.n, o_O=clk_synth_se),
        ]

        pll_locked = Signal()
        rtio_clk = Signal()
        rtiox4_clk = Signal()
        fb_clk = Signal()
        self.specials += [
            Instance("PLLE2_ADV",
                     p_STARTUP_WAIT="FALSE", o_LOCKED=pll_locked,
                     p_BANDWIDTH="HIGH",
                     p_REF_JITTER1=0.001,
                     p_CLKIN1_PERIOD=8.0, p_CLKIN2_PERIOD=8.0,
                     i_CLKIN2=clk_synth_se,
                     # Warning: CLKINSEL=0 means CLKIN2 is selected
                     i_CLKINSEL=0,

                     # VCO @ 1.5GHz when using 125MHz input
                     p_CLKFBOUT_MULT=12, p_DIVCLK_DIVIDE=1,
                     i_CLKFBIN=fb_clk,
                     i_RST=self.pll_reset.storage,

                     o_CLKFBOUT=fb_clk,

                     p_CLKOUT0_DIVIDE=3, p_CLKOUT0_PHASE=0.0,
                     o_CLKOUT0=rtiox4_clk,

                     p_CLKOUT1_DIVIDE=12, p_CLKOUT1_PHASE=0.0,
                     o_CLKOUT1=rtio_clk),
            Instance("BUFG", i_I=rtio_clk, o_O=self.cd_rtio.clk),
            Instance("BUFG", i_I=rtiox4_clk, o_O=self.cd_rtiox4.clk),

            AsyncResetSynchronizer(self.cd_rtio, ~pll_locked),
            MultiReg(pll_locked, self.pll_locked.status)
        ]

        
eem_iostandard_dict = {
     0: "LVDS_25",
     1: "LVDS_25",
     2: "LVDS",
     3: "LVDS",
     4: "LVDS",
     5: "LVDS",
     6: "LVDS",
     7: "LVDS",
     8: "LVDS_25",
     9: "LVDS_25",
    10: "LVDS",
    11: "LVDS",
}


def eem_iostandard(eem):
    return IOStandard(eem_iostandard_dict[eem])


class SMAClkinForward(Module):
    def __init__(self, platform):
        sma_clkin = platform.request("sma_clkin")
        sma_clkin_se = Signal()
        cdr_clk_se = Signal()
        cdr_clk = platform.request("cdr_clk")
        self.specials += [
            Instance("IBUFDS", i_I=sma_clkin.p, i_IB=sma_clkin.n, o_O=sma_clkin_se),
            Instance("ODDR", i_C=sma_clkin_se, i_CE=1, i_D1=1, i_D2=0, o_Q=cdr_clk_se),
            Instance("OBUFDS", i_I=cdr_clk_se, o_O=cdr_clk.p, o_OB=cdr_clk.n)
        ]


class GenericStandalone(SoCCore):
    def __init__(self, description, acpki=False):
        self.acpki = acpki
        self.rustc_cfg = dict()

        platform = kasli_soc.Platform()
        platform.toolchain.bitstream_commands.extend([
            "set_property BITSTREAM.GENERAL.COMPRESS True [current_design]",
        ])
        ident = description["variant"]
        if self.acpki:
            ident = "acpki_" + ident
        SoCCore.__init__(self, platform=platform, csr_data_width=32, ident=ident)

        platform.add_platform_command("create_clock -name clk_fpga_0 -period 8 [get_pins \"PS7/FCLKCLK[0]\"]")
        platform.add_platform_command("set_input_jitter clk_fpga_0 0.24")

        self.submodules += SMAClkinForward(self.platform)

        self.rustc_cfg["has_si5324"] = None
        self.rustc_cfg["si5324_soft_reset"] = None

        self.crg = self.ps7 # HACK for eem_7series to find the clock
        self.submodules.rtio_crg = RTIOCRG(self.platform)
        self.csr_devices.append("rtio_crg")
        self.platform.add_period_constraint(self.rtio_crg.cd_rtio.clk, 8.)
        self.platform.add_false_path_constraints(
            self.ps7.cd_sys.clk,
            self.rtio_crg.cd_rtio.clk)

        self.rtio_channels = []
        has_grabber = any(peripheral["type"] == "grabber" for peripheral in description["peripherals"])
        if has_grabber:
            self.grabber_csr_group = []
        eem_7series.add_peripherals(self, description["peripherals"], iostandard=eem_iostandard)
        for i in (0, 1):
            print("USER LED at RTIO channel 0x{:06x}".format(len(self.rtio_channels)))
            user_led = self.platform.request("user_led", i)
            phy = ttl_simple.Output(user_led)
            self.submodules += phy
            self.rtio_channels.append(rtio.Channel.from_phy(phy))
        self.config["RTIO_LOG_CHANNEL"] = len(self.rtio_channels)
        self.rtio_channels.append(rtio.LogChannel())

        self.submodules.rtio_tsc = rtio.TSC("async", glbl_fine_ts_width=3)
        self.submodules.rtio_core = rtio.Core(self.rtio_tsc, self.rtio_channels)
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

        self.submodules.rtio_moninj = rtio.MonInj(self.rtio_channels)
        self.csr_devices.append("rtio_moninj")

        self.submodules.rtio_analyzer = analyzer.Analyzer(self.rtio_tsc, self.rtio_core.cri,
                                                          self.ps7.s_axi_hp1)
        self.csr_devices.append("rtio_analyzer")

        if has_grabber:
            self.rustc_cfg["has_grabber"] = None
            self.add_csr_group("grabber", self.grabber_csr_group)
            for grabber in self.grabber_csr_group:
                self.platform.add_false_path_constraints(
                    self.rtio_crg.cd_rtio.clk, getattr(self, grabber).deserializer.cd_cl.clk)


class GenericMaster(SoCCore):
    def __init__(self, description, acpki=False):
        sys_clk_freq = 125e6
        rtio_clk_freq = description["rtio_frequency"]

        self.acpki = acpki
        self.rustc_cfg = dict()

        platform = kasli_soc.Platform()
        platform.toolchain.bitstream_commands.extend([
            "set_property BITSTREAM.GENERAL.COMPRESS True [current_design]",
        ])
        ident = description["variant"]
        if self.acpki:
            ident = "acpki_" + ident
        SoCCore.__init__(self, platform=platform, csr_data_width=32, ident=ident)

        platform.add_platform_command("create_clock -name clk_fpga_0 -period 8 [get_pins \"PS7/FCLKCLK[0]\"]")
        platform.add_platform_command("set_input_jitter clk_fpga_0 0.24")

        self.submodules += SMAClkinForward(self.platform)

        data_pads = [platform.request("sfp", i) for i in range(4)]

        self.submodules.drtio_transceiver = gtx_7series.GTX(
            clock_pads=platform.request("clk_gtp"),
            pads=data_pads,
            sys_clk_freq=sys_clk_freq)
        self.csr_devices.append("drtio_transceiver")

        self.crg = self.ps7 # HACK for eem_7series to find the clock
        self.submodules.rtio_crg = RTIOClockMultiplier(rtio_clk_freq)
        self.csr_devices.append("rtio_crg")

        self.rustc_cfg["has_si5324"] = None
        self.rustc_cfg["si5324_soft_reset"] = None

        self.rtio_channels = []
        has_grabber = any(peripheral["type"] == "grabber" for peripheral in description["peripherals"])
        if has_grabber:
            self.grabber_csr_group = []
        eem_7series.add_peripherals(self, description["peripherals"], iostandard=eem_iostandard)
        for i in (0, 1):
            print("USER LED at RTIO channel 0x{:06x}".format(len(self.rtio_channels)))
            user_led = self.platform.request("user_led", i)
            phy = ttl_simple.Output(user_led)
            self.submodules += phy
            self.rtio_channels.append(rtio.Channel.from_phy(phy))
        self.config["RTIO_LOG_CHANNEL"] = len(self.rtio_channels)
        self.rtio_channels.append(rtio.LogChannel())

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

            core = cdr(DRTIOMaster(self.rtio_tsc, self.drtio_transceiver.channels[i]))
            setattr(self.submodules, core_name, core)
            self.drtio_cri.append(core.cri)
            self.csr_devices.append(core_name)

            coreaux = cdr(drtio_aux_controller.DRTIOAuxControllerBare(core.link_layer))
            setattr(self.submodules, coreaux_name, coreaux)
            self.csr_devices.append(coreaux_name)

            size = coreaux.get_mem_size()
            memory_address = self.axi2csr.register_port(coreaux.get_tx_port(), size)
            self.axi2csr.register_port(coreaux.get_rx_port(), size)
            self.add_memory_region(memory_name, self.mem_map["csr"] + memory_address, size * 2)
        self.rustc_cfg["has_drtio"] = None
        self.rustc_cfg["has_drtio_routing"] = None
        self.add_csr_group("drtio", drtio_csr_group)
        self.add_csr_group("drtioaux", drtioaux_csr_group)
        self.add_memory_group("drtioaux_mem", drtioaux_memory_group)

        self.submodules.rtio_core = rtio.Core(self.rtio_tsc, self.rtio_channels)
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
            enable_routing=True)
        self.csr_devices.append("cri_con")

        self.submodules.rtio_moninj = rtio.MonInj(self.rtio_channels)
        self.csr_devices.append("rtio_moninj")

        self.submodules.routing_table = rtio.RoutingTableAccess(self.cri_con)
        self.csr_devices.append("routing_table")

        self.submodules.rtio_analyzer = analyzer.Analyzer(self.rtio_tsc, self.rtio_core.cri,
                                                          self.ps7.s_axi_hp1)
        self.csr_devices.append("rtio_analyzer")

        if has_grabber:
            self.rustc_cfg["has_grabber"] = None
            self.add_csr_group("grabber", self.grabber_csr_group)


class GenericSatellite(SoCCore):
    def __init__(self, description, acpki=False):
        sys_clk_freq = 125e6  
        rtio_clk_freq = description["rtio_frequency"]

        self.acpki = acpki
        self.rustc_cfg = dict()

        platform = kasli_soc.Platform()
        platform.toolchain.bitstream_commands.extend([
            "set_property BITSTREAM.GENERAL.COMPRESS True [current_design]",
        ])
        ident = description["variant"]
        if self.acpki:
            ident = "acpki_" + ident
        SoCCore.__init__(self, platform=platform, csr_data_width=32, ident=ident)

        platform.add_platform_command("create_clock -name clk_fpga_0 -period 8 [get_pins \"PS7/FCLKCLK[0]\"]")
        platform.add_platform_command("set_input_jitter clk_fpga_0 0.24")

        self.crg = self.ps7 # HACK for eem_7series to find the clock
        self.submodules.rtio_crg = RTIOClockMultiplier(rtio_clk_freq)
        self.csr_devices.append("rtio_crg")
        self.rustc_cfg["has_rtio_crg"] = None
        
        data_pads = [platform.request("sfp", i) for i in range(4)]
        
        self.submodules.drtio_transceiver = gtx_7series.GTX(
            clock_pads=platform.request("clk_gtp"),  
            pads=data_pads,
            sys_clk_freq=sys_clk_freq)
        self.csr_devices.append("drtio_transceiver")

        self.rtio_channels = []
        has_grabber = any(peripheral["type"] == "grabber" for peripheral in description["peripherals"])
        if has_grabber:
            self.grabber_csr_group = []
        eem_7series.add_peripherals(self, description["peripherals"], iostandard=eem_iostandard)
        for i in (0, 1):
            print("USER LED at RTIO channel 0x{:06x}".format(len(self.rtio_channels)))
            user_led = self.platform.request("user_led", i)
            phy = ttl_simple.Output(user_led)
            self.submodules += phy
            self.rtio_channels.append(rtio.Channel.from_phy(phy))
        self.config["RTIO_LOG_CHANNEL"] = len(self.rtio_channels)
        self.rtio_channels.append(rtio.LogChannel())

        self.submodules.rtio_tsc = rtio.TSC("sync", glbl_fine_ts_width=3)

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

            if i == 0:
                self.submodules.rx_synchronizer = cdr(XilinxRXSynchronizer())
                core = cdr(DRTIOSatellite(
                    self.rtio_tsc, self.drtio_transceiver.channels[i],
                    self.rx_synchronizer))
                self.submodules.drtiosat = core
                self.csr_devices.append("drtiosat")
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
        self.add_memory_group("drtioaux_mem", drtioaux_memory_group)
        self.add_csr_group("drtiorep", drtiorep_csr_group)

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

        self.submodules.local_io = SyncRTIO(self.rtio_tsc, self.rtio_channels)
        self.comb += self.drtiosat.async_errors.eq(self.local_io.async_errors)

        self.submodules.cri_con = rtio.CRIInterconnectShared(
            [self.drtiosat.cri],
            [self.local_io.cri] + self.drtio_cri,
            mode="sync", enable_routing=True)
        self.csr_devices.append("cri_con")

        self.submodules.routing_table = rtio.RoutingTableAccess(self.cri_con)
        self.csr_devices.append("routing_table")     

        self.submodules.rtio_moninj = rtio.MonInj(self.rtio_channels)
        self.csr_devices.append("rtio_moninj")

        rtio_clk_period = 1e9/rtio_clk_freq
        self.rustc_cfg["rtio_frequency"] = str(rtio_clk_freq/1e6)

        self.submodules.siphaser = SiPhaser7Series(
            si5324_clkin=platform.request("cdr_clk"),
            rx_synchronizer=self.rx_synchronizer,
            ultrascale=False,
            rtio_clk_freq=self.drtio_transceiver.rtio_clk_freq)
        platform.add_false_path_constraints(
            self.crg.cd_sys.clk, self.siphaser.mmcm_freerun_output)
        self.csr_devices.append("siphaser")
        self.rustc_cfg["has_si5324"] = None
        self.rustc_cfg["has_siphaser"] = None
        self.rustc_cfg["si5324_soft_reset"] = None

        gtx0 = self.drtio_transceiver.gtxs[0]
        platform.add_period_constraint(gtx0.txoutclk, rtio_clk_period)
        platform.add_period_constraint(gtx0.rxoutclk, rtio_clk_period)
        platform.add_false_path_constraints(
            self.crg.cd_sys.clk,
            gtx0.txoutclk, gtx0.rxoutclk)
        for gtx in self.drtio_transceiver.gtxs[1:]:
            platform.add_period_constraint(gtx.rxoutclk, rtio_clk_period)
            platform.add_false_path_constraints(
                self.crg.cd_sys.clk, gtx.rxoutclk)

        if has_grabber:
            self.rustc_cfg["has_grabber"] = None
            self.add_csr_group("grabber", self.grabber_csr_group)
            # no RTIO CRG here
     

def write_mem_file(soc, filename):
    with open(filename, "w") as f:
        f.write(cpu_interface.get_mem_rust(
            soc.get_memory_regions(), soc.get_memory_groups(), None))


def write_csr_file(soc, filename):
    with open(filename, "w") as f:
        f.write(cpu_interface.get_csr_rust(
            soc.get_csr_regions(), soc.get_csr_groups(), soc.get_constants()))


def write_rustc_cfg_file(soc, filename):
    with open(filename, "w") as f:
        for k, v in sorted(soc.rustc_cfg.items(), key=itemgetter(0)):
            if v is None:
                f.write("{}\n".format(k))
            else:
                f.write("{}=\"{}\"\n".format(k, v))


def main():
    parser = argparse.ArgumentParser(
        description="ARTIQ device binary builder for generic Kasli-SoC systems")
    parser.add_argument("-r", default=None,
        help="build Rust interface into the specified file")
    parser.add_argument("-c", default=None,
        help="build Rust compiler configuration into the specified file")
    parser.add_argument("-m", default=None,
        help="build Rust memory interface into the specified file")
    parser.add_argument("-g", default=None,
        help="build gateware into the specified directory")
    parser.add_argument("--acpki", default=False, action="store_true",
        help="enable ACPKI")
    parser.add_argument("description", metavar="DESCRIPTION",
                        help="JSON system description file")
    args = parser.parse_args()
    description = jsondesc.load(args.description)

    if description["target"] != "kasli_soc":
        raise ValueError("Description is for a different target")

    if description["base"] == "standalone":
        cls = GenericStandalone
    elif description["base"] == "master":
        cls = GenericMaster
    elif description["base"] == "satellite":
        cls = GenericSatellite
    else:
        raise ValueError("Invalid base")

    soc = cls(description, acpki=args.acpki)
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
