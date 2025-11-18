from operator import attrgetter

from migen import *
from migen.genlib.cdc import MultiReg
from migen_axi.interconnect import axi
from misoc.interconnect.csr import *

from artiq.gateware import rtio

OUT_BURST_LEN = 10
IN_BURST_LEN = 4


class Engine(Module, AutoCSR):
    def __init__(self, bus, user):
        self.addr_base = CSRStorage(32)
        # statistics (unused)
        self.trig_count = CSRStatus(32)
        self.write_count = CSRStatus(32)

        self.trigger_stb = Signal()

        # Dout : Data received from CPU, output by DMA module
        # Din : Data driven into DMA module, written into CPU
        # When stb assert, index shows word being read/written, dout/din holds
        # data
        #
        # Cycle:
        # trigger_stb pulsed at start
        # Then out_burst_len words are strobed out of dout
        # Then, when din_ready is high, in_burst_len words are strobed in to din
        self.dout_stb = Signal()
        self.din_stb = Signal()
        self.dout_index = Signal(max=16)
        self.din_index = Signal(max=16)
        self.din_ready = Signal()
        self.dout = Signal(64)
        self.din = Signal(64)

        ###

        self.sync += If(self.trigger_stb, self.trig_count.status.eq(self.trig_count.status+1))

        self.comb += [
            user.aruser.eq(0x1f),
            user.awuser.eq(0x1f)
        ]

        ar, aw, w, r, b = attrgetter("ar", "aw", "w", "r", "b")(bus)

        ### Read
        self.comb += [
            ar.addr.eq(self.addr_base.storage),
            self.dout.eq(r.data),
            r.ready.eq(1),
            ar.burst.eq(axi.Burst.incr.value),
            ar.len.eq(OUT_BURST_LEN-1),  # Number of transfers in burst (0->1 transfer, 1->2 transfers...)
            ar.size.eq(3),  # Width of burst: 3 = 8 bytes = 64 bits
            ar.cache.eq(0xf),
        ]

        # read control
        self.submodules.read_fsm = read_fsm = FSM(reset_state="IDLE")
        read_fsm.act("IDLE",
            If(self.trigger_stb,
                ar.valid.eq(1),
                If(ar.ready,
                    NextState("READ")
                ).Else(
                    NextState("READ_START")
                )
            )
        )
        read_fsm.act("READ_START",
            ar.valid.eq(1),
            If(ar.ready,
                NextState("READ"),
            )
        )
        read_fsm.act("READ",
            ar.valid.eq(0),
            If(r.last & r.valid,
                NextState("IDLE")
            )
        )

        self.sync += [
            If(read_fsm.ongoing("IDLE"),
                self.dout_index.eq(0)
            ).Elif(r.valid & read_fsm.ongoing("READ"),
                self.dout_index.eq(self.dout_index+1)
            )
        ]

        self.comb += self.dout_stb.eq(r.valid & r.ready)

        ### Write
        self.comb += [
            w.data.eq(self.din),
            aw.addr.eq(self.addr_base.storage+96),
            w.strb.eq(0xff),
            aw.burst.eq(axi.Burst.incr.value),
            aw.len.eq(IN_BURST_LEN-1),  # Number of transfers in burst minus 1
            aw.size.eq(3),  # Width of burst: 3 = 8 bytes = 64 bits
            aw.cache.eq(0xf),
            b.ready.eq(1),
        ]

        # write control
        self.submodules.write_fsm = write_fsm = FSM(reset_state="IDLE")
        write_fsm.act("IDLE",
            w.valid.eq(0),
            aw.valid.eq(0),
            If(self.trigger_stb,
                aw.valid.eq(1),
                If(aw.ready,  # assumes aw.ready is not deasserted from now on
                    NextState("DATA_WAIT")
                ).Else(
                    NextState("AW_READY_WAIT")
                )
            )
        )
        write_fsm.act("AW_READY_WAIT",
            aw.valid.eq(1),
            If(aw.ready,
                NextState("DATA_WAIT"),
            )
        )
        write_fsm.act("DATA_WAIT",
            aw.valid.eq(0),
            If(self.din_ready,
                w.valid.eq(1),
                NextState("WRITE")
            )
        )
        write_fsm.act("WRITE",
            w.valid.eq(1),
            If(w.ready & w.last,
                NextState("IDLE")
            )
        )

        self.sync += [
            If(w.ready & w.valid,
                self.write_count.status.eq(self.write_count.status+1)),
            If(write_fsm.ongoing("IDLE"),
                self.din_index.eq(0)
            ),
            If(w.ready & w.valid, self.din_index.eq(self.din_index+1))
        ]

        self.comb += [
            w.last.eq(self.din_index==aw.len),
            self.din_stb.eq(w.valid & w.ready)
        ]


class KernelInitiator(Module, AutoCSR):
    def __init__(self, tsc, bus, user, evento):
        # Core is disabled upon reset to avoid spurious triggering if evento toggles from e.g. boot code.
        self.enable = CSRStorage()

        self.counter = CSRStatus(64)
        self.counter_update = CSR()
        self.o_status = CSRStatus(3)
        self.i_status = CSRStatus(4)

        self.submodules.engine = Engine(bus, user)
        self.cri = rtio.cri.Interface()

        ###

        evento_stb = Signal()
        evento_latched = Signal()
        evento_latched_d = Signal()
        self.specials += MultiReg(evento, evento_latched)
        self.sync += evento_latched_d.eq(evento_latched)
        self.comb += self.engine.trigger_stb.eq(self.enable.storage & (evento_latched != evento_latched_d))

        cri = self.cri

        cmd = Signal(8)
        cmd_write = Signal()
        cmd_read = Signal()
        self.comb += [
            cmd_write.eq(cmd == 0),
            cmd_read.eq(cmd == 1)
        ]

        out_len = Signal(8)
        dout_cases = {}
        dout_cases[0] = [
            cmd.eq(self.engine.dout[:8]),
            out_len.eq(self.engine.dout[8:16]),
            cri.o_address.eq(self.engine.dout[32:40]),
            cri.chan_sel.eq(self.engine.dout[40:]),
        ]
        for i in range(8):
            target = cri.o_data[i*64:(i+1)*64]
            dout_cases[0] += [If(i >= self.engine.dout[8:16], target.eq(0))]

        dout_cases[1] = [
            cri.o_timestamp.eq(self.engine.dout),
            cri.i_timeout.eq(self.engine.dout)
        ]
        for i in range(8):
            target = cri.o_data[i*64:(i+1)*64]
            dout_cases[i+2] = [target.eq(self.engine.dout)]

        self.sync += [
            cri.cmd.eq(rtio.cri.commands["nop"]),
            If(self.engine.dout_stb,
                Case(self.engine.dout_index, dout_cases),
                If(self.engine.dout_index == out_len + 2,
                    If(cmd_write, cri.cmd.eq(rtio.cri.commands["write"])),
                    If(cmd_read, cri.cmd.eq(rtio.cri.commands["read"]))
                )
            )
        ]

        # If input event, wait for response before allow input data to be
        # sampled
        # TODO: If output, wait for wait flag clear
        RTIO_I_STATUS_WAIT_STATUS = 4
        RTIO_O_STATUS_WAIT = 1

        self.submodules.fsm = fsm = FSM(reset_state="IDLE")

        fsm.act("IDLE",
            If(self.engine.trigger_stb, NextState("WAIT_OUT_CYCLE"))
        )
        fsm.act("WAIT_OUT_CYCLE",
            self.engine.din_ready.eq(0),
            If(self.engine.dout_stb & cmd_write & (self.engine.dout_index == out_len + 2),
               NextState("WAIT_READY")
            ),
            # for some reason read requires some delay until the next state
            If(self.engine.dout_stb & cmd_read & (self.engine.dout_index == out_len + 3),
                NextState("WAIT_READY")
            )
        )
        fsm.act("WAIT_READY",
            If(cmd_read & (cri.i_status & RTIO_I_STATUS_WAIT_STATUS == 0) \
                | cmd_write & ~(cri.o_status & RTIO_O_STATUS_WAIT),
                self.engine.din_ready.eq(1),
                NextState("IDLE")
            )
        )

        din_cases_cmdwrite = {
            0: [self.engine.din.eq((1<<16) | cri.o_status)],
            1: [self.engine.din.eq(0)],
        }
        din_cases_cmdread = {
            0: [self.engine.din[:32].eq((1<<16) | cri.i_status), self.engine.din[32:].eq(cri.i_data)],
            1: [self.engine.din.eq(cri.i_timestamp)]
        }

        self.comb += [
            If(cmd_read, Case(self.engine.din_index, din_cases_cmdread)),
            If(cmd_write, Case(self.engine.din_index, din_cases_cmdwrite)),
        ]

        # CRI CSRs
        self.sync += If(self.counter_update.re, self.counter.status.eq(tsc.full_ts_cri))
        self.comb += [
            self.o_status.status.eq(self.cri.o_status),
            self.i_status.status.eq(self.cri.i_status),
        ]
