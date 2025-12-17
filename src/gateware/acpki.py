from operator import attrgetter

from migen import *
from migen.genlib.cdc import MultiReg
from migen_axi.interconnect import axi
from misoc.interconnect.csr import *

from artiq.gateware import rtio

OUT_BURST_LEN = 12
IN_BURST_LEN = 4

RTIO_I_STATUS_WAIT_STATUS = 4
RTIO_O_STATUS_WAIT = 1

BATCH_ENTRY_LEN = 80
REPLY_ADDR_OFFSET = 96

class Engine(Module, AutoCSR):
    def __init__(self, bus, user):
        self.addr_base = Signal(32)
        self.write_addr = Signal(32)

        self.trigger_stb = Signal()

        # Dout : Data received from CPU, output by DMA module
        # Din : Data driven into DMA module, written into CPU
        # When stb is asserted, index shows word being read/written, 
        # dout/din holds data
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

        self.addr_updateable = Signal()

        ###

        self.comb += [
            user.aruser.eq(0x1f),
            user.awuser.eq(0x1f)
        ]

        ar, aw, w, r, b = attrgetter("ar", "aw", "w", "r", "b")(bus)

        ### Read
        self.comb += [
            ar.addr.eq(self.addr_base),
            self.dout.eq(r.data),
            r.ready.eq(1),
            ar.burst.eq(axi.Burst.incr.value),
            ar.len.eq(OUT_BURST_LEN-1),  # Number of transfers in burst (0->1 transfer, 1->2 transfers...)
            ar.size.eq(3),  # Width of burst: 3 = 8 bytes = 64 bits
            ar.cache.eq(0xf),
            self.addr_updateable.eq(~ar.valid),
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
            aw.addr.eq(self.write_addr),
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
            If(write_fsm.ongoing("IDLE"),
                self.din_index.eq(0)
            ),
            If(w.ready & w.valid, self.din_index.eq(self.din_index+1))
        ]

        self.comb += [
            w.last.eq(self.din_index==aw.len),
            self.din_stb.eq(w.valid & w.ready)
        ]

        self.write_idle = Signal()
        self.comb += self.write_idle.eq(write_fsm.ongoing("IDLE"))

class KernelInitiator(Module, AutoCSR):
    def __init__(self, tsc, bus, user, evento):
        # Core is disabled upon reset to avoid spurious triggering if evento toggles from e.g. boot code.
        # Should be also reset between kernels (?)
        self.enable = CSRStorage()
        self.out_base = CSRStorage(32)  # output data (to CRI)
        self.in_base = CSRStorage(32)   # in data (RTIO reply)
        self.batch_len = CSRStorage(32)

        self.counter = CSRStatus(64)
        self.counter_update = CSR()
        self.o_status = CSRStatus(3)
        self.i_status = CSRStatus(4)

        self.submodules.engine = Engine(bus, user)
        self.cri = rtio.cri.Interface()

        ###

        batch_en = Signal()
        self.comb += batch_en.eq(self.enable.storage & ~(self.batch_len.storage == 0))

        batch_offset = Signal.like(self.batch_len.storage)
        batch_counter = Signal.like(self.batch_len.storage)
        batch_stb = Signal()  # triggers the next round
        # save the channel in case of an error in batch mode to help find the problematic one
        target_latched = Signal(32)
        rtio_err = Signal.like(self.cri.o_status)

        evento_stb = Signal()
        evento_latched = Signal()
        evento_latched_d = Signal()
        self.specials += MultiReg(evento, evento_latched)
        self.sync += evento_latched_d.eq(evento_latched)
        self.comb += [
            self.engine.trigger_stb.eq(self.enable.storage & ((evento_latched != evento_latched_d) | batch_stb)),
            self.engine.write_addr.eq(self.in_base.storage),
        ]

        cri = self.cri

        cmd = Signal(8)
        cmd_write = Signal()
        cmd_read = Signal()
        self.comb += [
            cmd_write.eq(cmd == 0 | batch_en),  # rtio output, forced in batch mode
            cmd_read.eq(cmd == 1 & ~batch_en),  # rtio input
        ]

        out_len = Signal(8)
        dout_cases = {}
        # request_cmd: i8
        # data_width: i8
        # padding0: [i8; 2]
        # request_target: i32
        dout_cases[0] = [
            cmd.eq(self.engine.dout[:8]),
            out_len.eq(self.engine.dout[8:16]),
            target_latched.eq(self.engine.dout[32:]),
            cri.o_address.eq(self.engine.dout[32:40]),
            cri.chan_sel.eq(self.engine.dout[40:]),
        ]
        for i in range(8):
            target = cri.o_data[i*64:(i+1)*64]
            dout_cases[0] += [If(i >= self.engine.dout[8:16], target.eq(0))]

        # request_timestamp: i64
        dout_cases[1] = [
            cri.o_timestamp.eq(self.engine.dout),
            cri.i_timeout.eq(self.engine.dout)
        ]
        # request_data: [i32; 16]
        # packed into 64 bit * 8 here
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

        rtio_ready = Signal()
        last_batch = Signal()
        self.comb += [
            rtio_ready.eq(cmd_read & (cri.i_status & RTIO_I_STATUS_WAIT_STATUS == 0) \
                        | (cmd_write & (cri.o_status & RTIO_O_STATUS_WAIT == 0))),
            last_batch.eq(batch_en & (self.batch_len.storage == batch_counter))
        ]

        # If input event, wait for response before 
        # allowing the input data to be sampled

        self.submodules.fsm = fsm = FSM(reset_state="IDLE")

        fsm.act("IDLE",
            If(self.engine.trigger_stb, 
                NextState("WAIT_OUT_CYCLE")),
        )
        fsm.act("WAIT_OUT_CYCLE",
            self.engine.din_ready.eq(0),
            If(self.engine.dout_stb & (self.engine.dout_index == out_len + 3),
                # update batch info for the next round
                If(batch_en, 
                    NextValue(batch_counter, batch_counter + 1),
                    NextValue(batch_offset, batch_offset + BATCH_ENTRY_LEN)
                ),
                NextState("WAIT_READY"),
            )
        )

        fsm.act("WAIT_READY",
            If(rtio_ready,
                # stop the batch in case of an error or when reaching the capacity
                # when batch mode is not enabled, just when it's not busy anymore
                If(~batch_en | last_batch,
                    self.engine.din_ready.eq(1),
                    NextState("IDLE")
                ).Elif(self.engine.dout_index == 0,  # read engine at idle
                # todo: prefetch preamble to determine exact burst length to save cycles
                    batch_stb.eq(1),
                    NextState("WAIT_OUT_CYCLE")
                )
            )
        )

        din_cases_cmdwrite = {
            0: [self.engine.din.eq((1<<16) | cri.o_status)],
            1: [self.engine.din.eq(0)],
            2: [self.engine.din.eq(target_latched)]
        }
        din_cases_cmdread = {
            # reply_status: VolatileCell<i32>, reply_data: VolatileCell<i32>
            0: [self.engine.din[:32].eq((1<<16) | cri.i_status), self.engine.din[32:].eq(cri.i_data)],
            # reply_timestamp: VolatileCell<i64>,
            1: [self.engine.din.eq(cri.i_timestamp)],
            # reply_target: VolatileCell<i32>
            2: [self.engine.din[:32].eq(target_latched)]
        }

        self.comb += [
            If(cmd_read, Case(self.engine.din_index, din_cases_cmdread)),
            If(cmd_write, Case(self.engine.din_index, din_cases_cmdwrite)),
        ]

        # batch-related
        self.sync += [
            If(self.batch_len.re,
                batch_counter.eq(0),
                batch_offset.eq(0),
                rtio_err.eq(0)
            ),
            If(~fsm.ongoing("IDLE") & batch_en,
                # save the RTIO errors besides WAIT
                rtio_err.eq(rtio_err | cri.o_status & ~(RTIO_O_STATUS_WAIT))),
            # feed the engine the new address when in idle or in preparation for the next step
            If(self.engine.addr_updateable,
                # batch_offset = 0 when batch_len = 0
                self.engine.addr_base.eq(self.out_base.storage + batch_offset)
            )
        ]

        # CRI CSRs
        self.sync += If(self.counter_update.re, self.counter.status.eq(tsc.full_ts_cri))
        self.comb += [
            self.o_status.status.eq(self.cri.o_status),
            self.i_status.status.eq(self.cri.i_status),
        ]
