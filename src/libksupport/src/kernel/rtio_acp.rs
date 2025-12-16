use core::sync::atomic::{Ordering, fence};

use cslice::CSlice;
use libcortex_a9::asm;
use vcell::VolatileCell;

#[cfg(has_drtio)]
use super::{KERNEL_CHANNEL_0TO1, KERNEL_CHANNEL_1TO0, KERNEL_IMAGE, Message};
use crate::{artiq_raise, pl::csr, rtio_core, kernel::KERNEL_IMAGE};

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

#[repr(C)]
pub struct TimestampedData {
    timestamp: i64,
    data: i32,
}

#[repr(C, align(64))]
#[derive(Copy, Clone)]
struct OutTransaction {
    request_cmd: i8,
    data_width: i8,
    padding: [i8; 2],
    request_target: i32,
    request_timestamp: i64,
    request_data: [i32; 16],
}

#[repr(C, align(64))]
struct InTransaction {
    reply_status: VolatileCell<i32>,
    reply_data: VolatileCell<i32>,
    reply_timestamp: VolatileCell<i64>,
    reply_target: VolatileCell<i32>,
    padding: [i32; 3]
}

static mut IN_BUFFER: InTransaction = InTransaction {
    reply_status: VolatileCell::new(0),
    reply_data: VolatileCell::new(0),
    reply_timestamp: VolatileCell::new(0),
    reply_target: VolatileCell::new(0),
    padding: [0; 3]
};

const BUFFER_SIZE: usize = 1024;

struct BatchState {
    ptr: i32,
    running: bool,
    transactions: [OutTransaction; BUFFER_SIZE],
}

static mut BATCH_STATE: BatchState = BatchState {
    ptr: 0,
    running: false,
    transactions: [OutTransaction { 
        request_cmd: 0, 
        data_width: 0,
        request_target: 0, 
        request_timestamp: 0, 
        request_data: [0; 16],
        padding: [0; 2],
    }; BUFFER_SIZE]
};

pub extern "C" fn init() {
    unsafe {
        rtio_core::reset_write(1);
        csr::rtio::in_base_write(&IN_BUFFER as *const InTransaction as u32);
        csr::rtio::out_base_write(&BATCH_STATE.transactions as *const OutTransaction as u32);
        csr::rtio::batch_len_write(0);
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
pub unsafe fn process_exceptional_status(channel: i32, status: i32) {
    let timestamp = now_mu();
    // The gateware should handle waiting
    assert!(status & RTIO_O_STATUS_WAIT == 0);
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
        BATCH_STATE.transactions[0].request_cmd = RTIO_CMD_OUTPUT;
        BATCH_STATE.transactions[0].data_width = 1;
        BATCH_STATE.transactions[0].request_target = target;
        BATCH_STATE.transactions[0].request_timestamp = NOW;
        BATCH_STATE.transactions[0].request_data[0] = data;

        let status = await_reply_status() & !(1 << 16);
        if status != 0 {
            process_exceptional_status(target >> 8, status);
        }
    }
}

pub extern "C" fn output_wide(target: i32, data: CSlice<i32>) {
    unsafe {
        BATCH_STATE.transactions[0].request_cmd = RTIO_CMD_OUTPUT;
        BATCH_STATE.transactions[0].data_width = data.len() as i8;
        BATCH_STATE.transactions[0].request_target = target;
        BATCH_STATE.transactions[0].request_timestamp = NOW;
        BATCH_STATE.transactions[0].request_data[..data.len()].copy_from_slice(data.as_ref());

        let status = await_reply_status() & !(1 << 16);
        if status != 0 {
            process_exceptional_status(target >> 8, status);
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
        BATCH_STATE.transactions[0].request_cmd = RTIO_CMD_INPUT;
        BATCH_STATE.transactions[0].request_timestamp = timeout;
        BATCH_STATE.transactions[0].request_target = channel << 8;
        BATCH_STATE.transactions[0].data_width = 0;
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
        BATCH_STATE.transactions[0].request_cmd = RTIO_CMD_INPUT;
        BATCH_STATE.transactions[0].request_timestamp = -1;
        BATCH_STATE.transactions[0].request_target = channel << 8;
        BATCH_STATE.transactions[0].data_width = 0;

        let status = await_reply_status();

        process_exceptional_input_status(status, channel);

        IN_BUFFER.reply_data.get()
    }
}

pub extern "C" fn input_timestamped_data(timeout: i64, channel: i32) -> TimestampedData {
    unsafe {
        BATCH_STATE.transactions[0].request_cmd = RTIO_CMD_INPUT;
        BATCH_STATE.transactions[0].request_timestamp = timeout;
        BATCH_STATE.transactions[0].request_target = channel << 8;
        BATCH_STATE.transactions[0].data_width = 0;

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
        if BATCH_STATE.running {
            artiq_raise!("RuntimeError", "Batched mode is already running.");
        }
        let library = KERNEL_IMAGE.as_ref().unwrap();
        library.rebind(b"rtio_output", batch_output as *const ()).unwrap();
        library
            .rebind(b"rtio_output_wide", batch_output_wide as *const ())
            .unwrap();
        BATCH_STATE.running = true;
        BATCH_STATE.ptr = 0;
    }
}

pub extern "C" fn batch_end() {
    unsafe {
        BATCH_STATE.running = false;
        if BATCH_STATE.ptr == 0 {
            return;
        }
        csr::rtio::batch_len_write((BATCH_STATE.ptr) as u32);

        // dmb and send event (commit the event to gateware)
        fence(Ordering::SeqCst);
        asm::sev();

        // start cleaning up before reading status
        let library = KERNEL_IMAGE.as_ref().unwrap();
        library.rebind(b"rtio_output", output as *const ()).unwrap();
        library
            .rebind(b"rtio_output_wide", output_wide as *const ())
            .unwrap();
        
        let status = loop {
            let status = IN_BUFFER.reply_status.get();
            if status != 0 {
                IN_BUFFER.reply_status.set(0);
                break status & !(1 << 16);
            }
        };
        // len = 0 to indicate we are not in batch mode anymore
        csr::rtio::batch_len_write(0);

        if status != 0 {
            let target = IN_BUFFER.reply_target.get();
            process_exceptional_status((target >> 8) as i32, status);
        }
    }
}

pub extern "C" fn batch_output(target: i32, data: i32) {
    unsafe {
        if BATCH_STATE.ptr as usize >= BUFFER_SIZE - 1 {
            artiq_raise!("RuntimeError", "Batch buffer is full");
        }
        BATCH_STATE.transactions[BATCH_STATE.ptr as usize].data_width = 1;
        BATCH_STATE.transactions[BATCH_STATE.ptr as usize].request_target = target;
        BATCH_STATE.transactions[BATCH_STATE.ptr as usize].request_timestamp = NOW;
        BATCH_STATE.transactions[BATCH_STATE.ptr as usize].request_data[0] = data;
        BATCH_STATE.ptr += 1;
    }
}

pub extern "C" fn batch_output_wide(target: i32, data: CSlice<i32>) {
    unsafe {
        if BATCH_STATE.ptr as usize >= BUFFER_SIZE - 1 {
            artiq_raise!("RuntimeError", "Batch buffer is full");
        }
        BATCH_STATE.transactions[BATCH_STATE.ptr as usize].data_width = data.len() as i8;
        BATCH_STATE.transactions[BATCH_STATE.ptr as usize].request_target = target;
        BATCH_STATE.transactions[BATCH_STATE.ptr as usize].request_timestamp = NOW;
        BATCH_STATE.transactions[BATCH_STATE.ptr as usize].request_data[..data.len()].copy_from_slice(data.as_ref());
        BATCH_STATE.ptr += 1;
    }
}