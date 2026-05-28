from migen import *
from migen.genlib.cdc import MultiReg
from migen.genlib.resetsync import AsyncResetSynchronizer
from misoc.interconnect.csr import *


class ClockSwitchFSM(Module):
    def __init__(self):
        self.i_clk_sw = Signal()

        self.o_clk_sw = Signal()
        self.o_reset = Signal()

        ###

        i_switch = Signal()
        o_switch = Signal()
        reset = Signal()

        # at 125MHz bootstrap cd, will get around 0.5ms
        delay_counter = Signal(16, reset=0xFFFF)

        # register to prevent glitches
        self.sync.bootstrap += [
            self.o_clk_sw.eq(o_switch),
            self.o_reset.eq(reset),
        ]

        self.o_clk_sw.attr.add("no_retiming")
        self.o_reset.attr.add("no_retiming")
        self.i_clk_sw.attr.add("no_retiming")
        i_switch.attr.add("no_retiming")

        self.specials += MultiReg(self.i_clk_sw, i_switch, "bootstrap")

        fsm = ClockDomainsRenamer("bootstrap")(FSM(reset_state="START"))

        self.submodules += fsm

        fsm.act("START",
            If(i_switch & ~o_switch,
                NextState("RESET_START"))
        )
        
        fsm.act("RESET_START",
            reset.eq(1),
            If(delay_counter == 0,
                NextValue(delay_counter, 0xFFFF),
                NextState("CLOCK_SWITCH")
            ).Else(
                NextValue(delay_counter, delay_counter-1),
            )
        )

        fsm.act("CLOCK_SWITCH",
            reset.eq(1),
            NextValue(o_switch, 1),
            NextValue(delay_counter, delay_counter-1),
            If(delay_counter == 0,
                NextState("END"))
        )
        fsm.act("END",
            NextValue(o_switch, 1),
            reset.eq(0))


class SYSCRG(Module, AutoCSR):
    def __init__(self, platform, ps7, main_clk, clk_sw=None, clk_sw_status=None, freq=125e6,
                 ext_async_rst=None, force_clk_sw=None):
        # assumes bootstrap clock is same freq as main and sys output
        self.clock_domains.cd_sys = ClockDomain()
        self.clock_domains.cd_sys4x = ClockDomain(reset_less=True)
        self.clock_domains.cd_sys5x = ClockDomain(reset_less=True)
        self.clock_domains.cd_clk200 = ClockDomain()

        self.current_clock = CSRStatus()

        self.cd_sys.clk.attr.add("keep")

        bootstrap_clk = ClockSignal("bootstrap")

        period = 1e9/freq

        self.submodules.clk_sw_fsm = ClockSwitchFSM()

        if force_clk_sw is None:
            if clk_sw is None:
                self.clock_switch = CSRStorage()
                self.comb += self.clk_sw_fsm.i_clk_sw.eq(self.clock_switch.storage)
            else:
                self.comb += self.clk_sw_fsm.i_clk_sw.eq(clk_sw)
            mmcm_clkin_sel = self.clk_sw_fsm.o_clk_sw
            mmcm_reset = self.clk_sw_fsm.o_reset
        else:
            self.clock_switch = CSRStorage()
            mmcm_clkin_sel = Signal(reset=force_clk_sw)
            mmcm_reset = Signal(reset=0)
            self.comb += [
                self.clk_sw_fsm.i_clk_sw.eq(0),
                mmcm_clkin_sel.eq(force_clk_sw),
                mmcm_reset.eq(0),
            ]

        self.mmcm_locked = Signal()
        mmcm_sys = Signal()
        mmcm_sys4x = Signal()
        mmcm_sys5x = Signal()
        mmcm_clk208 = Signal()
        mmcm_fb_clk = Signal()
        self.specials += [
            Instance("MMCME2_ADV",
                p_STARTUP_WAIT="FALSE", o_LOCKED=self.mmcm_locked,
                p_BANDWIDTH="HIGH",
                p_REF_JITTER1=0.001,
                p_CLKIN1_PERIOD=period, i_CLKIN1=main_clk,
                p_CLKIN2_PERIOD=period, i_CLKIN2=bootstrap_clk,
                i_CLKINSEL=mmcm_clkin_sel,

                # VCO @ 1.25GHz
                p_CLKFBOUT_MULT_F=10, p_DIVCLK_DIVIDE=1,
                i_CLKFBIN=mmcm_fb_clk,
                i_RST=mmcm_reset,

                o_CLKFBOUT=mmcm_fb_clk,

                p_CLKOUT0_DIVIDE_F=2.5, p_CLKOUT0_PHASE=0.0, o_CLKOUT0=mmcm_sys4x,

                # 125MHz
                p_CLKOUT1_DIVIDE=10, p_CLKOUT1_PHASE=0.0, o_CLKOUT1=mmcm_sys,

                # 625MHz
                p_CLKOUT2_DIVIDE=2, p_CLKOUT2_PHASE=0.0, o_CLKOUT2=mmcm_sys5x,

                # 208MHz
                p_CLKOUT3_DIVIDE=6, p_CLKOUT3_PHASE=0.0, o_CLKOUT3=mmcm_clk208,
            ),
            Instance("BUFG", i_I=mmcm_sys5x, o_O=self.cd_sys5x.clk),
            Instance("BUFG", i_I=mmcm_sys, o_O=self.cd_sys.clk),
            Instance("BUFG", i_I=mmcm_sys4x, o_O=self.cd_sys4x.clk),
            Instance("BUFG", i_I=mmcm_clk208, o_O=self.cd_clk200.clk),
        ]

        if ext_async_rst is not None:
            self.specials += [
                AsyncResetSynchronizer(self.cd_sys, ~self.mmcm_locked | ext_async_rst),
                AsyncResetSynchronizer(self.cd_clk200, ~self.mmcm_locked | ext_async_rst),
            ]
        else:
            self.specials += [
                AsyncResetSynchronizer(self.cd_sys, ~self.mmcm_locked),
                AsyncResetSynchronizer(self.cd_clk200, ~self.mmcm_locked),
            ]

        reset_counter = Signal(4, reset=15)
        ic_reset = Signal(reset=1)
        self.sync.clk200 += \
            If(reset_counter != 0,
                reset_counter.eq(reset_counter - 1)
            ).Else(
                ic_reset.eq(0)
            )
        self.specials += Instance("IDELAYCTRL", i_REFCLK=ClockSignal("clk200"), i_RST=ic_reset)

        if force_clk_sw is not None:
            self.comb += self.current_clock.status.eq(force_clk_sw)
        elif clk_sw_status is None:
            self.comb += self.current_clock.status.eq(self.clk_sw_fsm.o_clk_sw)
        else:
            self.comb += self.current_clock.status.eq(clk_sw_status)
