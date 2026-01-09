use core::sync::atomic::{Ordering, fence};

use cslice::CSlice;
use libcortex_a9::asm;
use vcell::VolatileCell;

#[cfg(has_drtio)]
use super::{KERNEL_CHANNEL_0TO1, KERNEL_CHANNEL_1TO0, Message};
use crate::{artiq_raise, kernel::KERNEL_IMAGE, pl::csr, rtio_core};

pub const RTIO_O_STATUS_WAIT: i32 = 1;
pub const RTIO_O_STATUS_UNDERFLOW: i32 = 2;
pub const RTIO_O_STATUS_DESTINATION_UNREACHABLE: i32 = 4;
pub const RTIO_I_STATUS_WAIT_EVENT: i32 = 1;
pub const RTIO_I_STATUS_OVERFLOW: i32 = 2;
#[allow(unused)]
pub const RTIO_I_STATUS_WAIT_STATUS: i32 = 4; // TODO
pub const RTIO_I_STATUS_DESTINATION_UNREACHABLE: i32 = 8;

const RTIO_CMD_OUTPUT: i8 = 0;
const RTIO_CMD_INPUT: i8 = 1;

const BATCH_ENABLED: i8 = 1;
const BATCH_DISABLED: i8 = 0;

#[repr(C)]
pub struct TimestampedData {
    timestamp: i64,
    data: i32,
}

#[repr(C, align(16))]
#[derive(Copy, Clone)]
struct OutTransaction {
    request_cmd: i8,
    data_width: i8,
    padding: [i8; 2],
    request_target: i32,
    request_timestamp: i64,
    request_data: [i32; 16],
}

#[repr(C, align(16))]
struct InTransaction {
    reply_status: VolatileCell<i32>,
    reply_data: VolatileCell<i32>,
    reply_timestamp: VolatileCell<i64>,
    reply_batch_cnt: VolatileCell<i32>,
    padding: i32,
}

static mut IN_BUFFER: InTransaction = InTransaction {
    reply_status: VolatileCell::new(0),
    reply_data: VolatileCell::new(0),
    reply_timestamp: VolatileCell::new(0),
    reply_batch_cnt: VolatileCell::new(0),
    padding: 0,
};

const BUFFER_SIZE: usize = csr::CONFIG_ACPKI_BATCH_SIZE as usize;

#[repr(C, align(16))]
struct OutBuffer {
    /* META */
    ptr: i32, // next writeable position in batch mode, also serves as len
    running: i8,
    padding: [i8; 11], // aligned to 16 bytes (per AXI alignment requirements)
    /* Output transactions */
    transactions: [OutTransaction; BUFFER_SIZE],
}

static mut OUT_BUFFER: OutBuffer = OutBuffer {
    ptr: 0,
    running: 0,
    padding: [0; 11],
    transactions: [OutTransaction {
        request_cmd: 0,
        data_width: 0,
        request_target: 0,
        request_timestamp: 0,
        request_data: [0; 16],
        padding: [0; 2],
    }; BUFFER_SIZE],
};

pub extern "C" fn init() {
    unsafe {
        rtio_core::reset_write(1);
        csr::rtio::in_base_write(&IN_BUFFER as *const InTransaction as u32);
        csr::rtio::out_base_write(&OUT_BUFFER as *const OutBuffer as u32);
        csr::rtio::enable_write(1);
    }
    #[cfg(has_drtio)]
    unsafe {
        KERNEL_CHANNEL_1TO0.as_mut().unwrap().send(Message::RtioInitRequest);
        match KERNEL_CHANNEL_0TO1.as_mut().unwrap().recv() {
            Message::RtioInitReply => (),
            other => panic!("Expected RtioInitReply after RtioInitRequest, but got {:?}", other),
        }
    }
}

pub extern "C" fn get_counter() -> i64 {
    unsafe {
        csr::rtio::counter_update_write(1);
        csr::rtio::counter_read() as i64
    }
}

static mut NOW: i64 = 0;

pub extern "C" fn now_mu() -> i64 {
    unsafe { NOW }
}

pub extern "C" fn at_mu(t: i64) {
    unsafe { NOW = t }
}

pub extern "C" fn delay_mu(dt: i64) {
    unsafe { NOW += dt }
}

#[inline(never)]
pub unsafe fn process_exceptional_status(channel: i32, status: i32, timestamp: i64) {
    // The gateware should handle waiting, but sometimes it will slip through the cracks
    let mut status = status;
    if status & RTIO_O_STATUS_WAIT != 0 {
        while status & RTIO_O_STATUS_WAIT != 0 {
            status = csr::rtio::o_status_read() as i32;
        }
    }
    if status & RTIO_O_STATUS_UNDERFLOW != 0 {
        artiq_raise!(
            "RTIOUnderflow",
            "RTIO underflow at {1} mu, channel {rtio_channel_info:0}, slack {2} mu",
            channel as i64,
            timestamp,
            timestamp - get_counter()
        );
    }
    if status & RTIO_O_STATUS_DESTINATION_UNREACHABLE != 0 {
        artiq_raise!(
            "RTIODestinationUnreachable",
            "RTIO destination unreachable, output, at {1} mu, channel {rtio_channel_info:0}",
            channel as i64,
            timestamp,
            0
        );
    }
}

fn await_reply_status() -> i32 {
    unsafe {
        // dmb and send event (commit the event to gateware)
        fence(Ordering::SeqCst);
        asm::sev();
        // actually await status
        loop {
            let status = IN_BUFFER.reply_status.get();
            if status != 0 {
                // Clear status so we can observe response on the next call
                IN_BUFFER.reply_status.set(0);
                return status;
            }
        }
    }
}

pub extern "C" fn output(target: i32, data: i32) {
    unsafe {
        OUT_BUFFER.transactions[0].request_cmd = RTIO_CMD_OUTPUT;
        OUT_BUFFER.transactions[0].data_width = 1;
        OUT_BUFFER.transactions[0].request_target = target;
        OUT_BUFFER.transactions[0].request_timestamp = NOW;
        OUT_BUFFER.transactions[0].request_data[0] = data;

        let status = await_reply_status() & !(1 << 16);
        if status != 0 {
            process_exceptional_status(target >> 8, status, now_mu());
        }
    }
}

pub extern "C" fn output_wide(target: i32, data: CSlice<i32>) {
    unsafe {
        OUT_BUFFER.transactions[0].request_cmd = RTIO_CMD_OUTPUT;
        OUT_BUFFER.transactions[0].data_width = data.len() as i8;
        OUT_BUFFER.transactions[0].request_target = target;
        OUT_BUFFER.transactions[0].request_timestamp = NOW;
        OUT_BUFFER.transactions[0].request_data[..data.len()].copy_from_slice(data.as_ref());

        let status = await_reply_status() & !(1 << 16);
        if status != 0 {
            process_exceptional_status(target >> 8, status, now_mu());
        }
    }
}

fn process_exceptional_input_status(status: i32, channel: i32) {
    if status & RTIO_I_STATUS_OVERFLOW != 0 {
        artiq_raise!(
            "RTIOOverflow",
            "RTIO input overflow on channel {rtio_channel_info:0}",
            channel as i64,
            0,
            0
        );
    }
    if status & RTIO_I_STATUS_DESTINATION_UNREACHABLE != 0 {
        artiq_raise!(
            "RTIODestinationUnreachable",
            "RTIO destination unreachable, input, on channel {rtio_channel_info:0}",
            channel as i64,
            0,
            0
        );
    }
}

pub extern "C" fn input_timestamp(timeout: i64, channel: i32) -> i64 {
    unsafe {
        OUT_BUFFER.transactions[0].request_cmd = RTIO_CMD_INPUT;
        OUT_BUFFER.transactions[0].request_timestamp = timeout;
        OUT_BUFFER.transactions[0].request_target = channel << 8;
        OUT_BUFFER.transactions[0].data_width = 0;
        let status = await_reply_status();

        if status & RTIO_I_STATUS_OVERFLOW != 0 {
            artiq_raise!(
                "RTIOOverflow",
                "RTIO input overflow on channel {rtio_channel_info:0}",
                channel as i64,
                0,
                0
            );
        }
        if status & RTIO_I_STATUS_WAIT_EVENT != 0 {
            return -1;
        }
        if status & RTIO_I_STATUS_DESTINATION_UNREACHABLE != 0 {
            artiq_raise!(
                "RTIODestinationUnreachable",
                "RTIO destination unreachable, input, on channel {rtio_channel_info:0}",
                channel as i64,
                0,
                0
            );
        }

        IN_BUFFER.reply_timestamp.get()
    }
}

pub extern "C" fn input_data(channel: i32) -> i32 {
    unsafe {
        OUT_BUFFER.transactions[0].request_cmd = RTIO_CMD_INPUT;
        OUT_BUFFER.transactions[0].request_timestamp = -1;
        OUT_BUFFER.transactions[0].request_target = channel << 8;
        OUT_BUFFER.transactions[0].data_width = 0;

        let status = await_reply_status();

        process_exceptional_input_status(status, channel);

        IN_BUFFER.reply_data.get()
    }
}

pub extern "C" fn input_timestamped_data(timeout: i64, channel: i32) -> TimestampedData {
    unsafe {
        OUT_BUFFER.transactions[0].request_cmd = RTIO_CMD_INPUT;
        OUT_BUFFER.transactions[0].request_timestamp = timeout;
        OUT_BUFFER.transactions[0].request_target = channel << 8;
        OUT_BUFFER.transactions[0].data_width = 0;

        let status = await_reply_status();
        process_exceptional_input_status(status, channel);

        TimestampedData {
            timestamp: IN_BUFFER.reply_timestamp.get(),
            data: IN_BUFFER.reply_data.get(),
        }
    }
}

pub fn write_log(data: &[i8]) {
    let mut word: u32 = 0;
    for i in 0..data.len() {
        word <<= 8;
        word |= data[i] as u32;
        if i % 4 == 3 {
            output((csr::CONFIG_RTIO_LOG_CHANNEL << 8) as i32, word as i32);
            word = 0;
        }
    }

    if word != 0 {
        output((csr::CONFIG_RTIO_LOG_CHANNEL << 8) as i32, word as i32);
    }
}

pub extern "C" fn batch_start() {
    unsafe {
        if OUT_BUFFER.running == 1 {
            artiq_raise!("RuntimeError", "Batched mode is already running.");
        }
        let library = KERNEL_IMAGE.as_ref().unwrap();
        library.rebind(b"rtio_output", batch_output as *const ()).unwrap();
        library
            .rebind(b"rtio_output_wide", batch_output_wide as *const ())
            .unwrap();
        OUT_BUFFER.running = BATCH_ENABLED;
        OUT_BUFFER.ptr = 0;
    }
}

pub extern "C" fn batch_end() {
    unsafe {
        if OUT_BUFFER.ptr == 0 {
            OUT_BUFFER.running = BATCH_DISABLED;
            return;
        }
        // dmb and send event (commit the event to gateware)
        fence(Ordering::SeqCst);
        asm::sev();

        // start cleaning up before reading status
        let library = KERNEL_IMAGE.as_ref().unwrap();
        library.rebind(b"rtio_output", output as *const ()).unwrap();
        library.rebind(b"rtio_output_wide", output_wide as *const ()).unwrap();

        let status = loop {
            let status = IN_BUFFER.reply_status.get();
            if status != 0 {
                IN_BUFFER.reply_status.set(0);
                break status & !(1 << 16);
            }
        };
        OUT_BUFFER.running = BATCH_DISABLED;
        if status != 0 {
            let ptr = IN_BUFFER.reply_batch_cnt.get();
            let target = OUT_BUFFER.transactions[ptr as usize].request_target >> 8;
            let timestamp = OUT_BUFFER.transactions[ptr as usize].request_timestamp;
            process_exceptional_status(target, status, timestamp);
        }
    }
}

pub extern "C" fn batch_output(target: i32, data: i32) {
    unsafe {
        if OUT_BUFFER.ptr as usize >= BUFFER_SIZE - 1 {
            OUT_BUFFER.ptr = 0;
            artiq_raise!("RuntimeError", "Batch buffer is full");
        }
        OUT_BUFFER.transactions[OUT_BUFFER.ptr as usize].data_width = 1;
        OUT_BUFFER.transactions[OUT_BUFFER.ptr as usize].request_target = target;
        OUT_BUFFER.transactions[OUT_BUFFER.ptr as usize].request_timestamp = NOW;
        OUT_BUFFER.transactions[OUT_BUFFER.ptr as usize].request_data[0] = data;
        OUT_BUFFER.ptr += 1;
    }
}

pub extern "C" fn batch_output_wide(target: i32, data: CSlice<i32>) {
    unsafe {
        if OUT_BUFFER.ptr as usize >= BUFFER_SIZE - 1 {
            OUT_BUFFER.ptr = 0;
            artiq_raise!("RuntimeError", "Batch buffer is full");
        }
        OUT_BUFFER.transactions[OUT_BUFFER.ptr as usize].data_width = data.len() as i8;
        OUT_BUFFER.transactions[OUT_BUFFER.ptr as usize].request_target = target;
        OUT_BUFFER.transactions[OUT_BUFFER.ptr as usize].request_timestamp = NOW;
        OUT_BUFFER.transactions[OUT_BUFFER.ptr as usize].request_data[..data.len()].copy_from_slice(data.as_ref());
        OUT_BUFFER.ptr += 1;
    }
}
