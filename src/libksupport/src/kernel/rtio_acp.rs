use alloc::vec::Vec;
use core::sync::atomic::{Ordering, fence};
use core::mem::MaybeUninit;
use core::ptr::copy_nonoverlapping;

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
struct Transaction {
    /* DOUT */
    request_cmd: i8,
    data_width: i8,
    padding0: [i8; 2],
    request_target: i32,
    request_timestamp: i64,
    request_data: [i32; 16],
    padding1: [i64; 2],
    /* DIN */
    reply_status: VolatileCell<i32>,
    reply_data: VolatileCell<i32>,
    reply_timestamp: VolatileCell<i64>,
    padding2: [i64; 2],
}

static mut TRANSACTION_BUFFER: Transaction = Transaction {
    /* DOUT */
    request_cmd: 0,
    data_width: 0,
    request_target: 0,
    request_timestamp: 0,
    request_data: [0; 16],
    /* DIN */
    reply_status: VolatileCell::new(0),
    reply_data: VolatileCell::new(0),
    reply_timestamp: VolatileCell::new(0),
    padding0: [0; 2],
    padding1: [0; 2],
    padding2: [0; 2],
};

pub extern "C" fn init() {
    unsafe {
        rtio_core::reset_write(1);
        csr::rtio::transaction_base_write(&TRANSACTION_BUFFER as *const Transaction as u32);
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
            let status = TRANSACTION_BUFFER.reply_status.get();
            if status != 0 {
                // Clear status so we can observe response on the next call
                TRANSACTION_BUFFER.reply_status.set(0);
                return status;
            }
        }
    }
}

pub extern "C" fn output(target: i32, data: i32) {
    unsafe {
        TRANSACTION_BUFFER.request_cmd = RTIO_CMD_OUTPUT;
        TRANSACTION_BUFFER.data_width = 1;
        TRANSACTION_BUFFER.request_target = target;
        TRANSACTION_BUFFER.request_timestamp = NOW;
        TRANSACTION_BUFFER.request_data[0] = data;

        let status = await_reply_status() & !(1 << 16);
        if status != 0 {
            process_exceptional_status(target >> 8, status);
        }
    }
}

pub extern "C" fn output_wide(target: i32, data: CSlice<i32>) {
    unsafe {
        TRANSACTION_BUFFER.request_cmd = RTIO_CMD_OUTPUT;
        TRANSACTION_BUFFER.data_width = data.len() as i8;
        TRANSACTION_BUFFER.request_target = target;
        TRANSACTION_BUFFER.request_timestamp = NOW;
        TRANSACTION_BUFFER.request_data[..data.len()].copy_from_slice(data.as_ref());

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
        TRANSACTION_BUFFER.request_cmd = RTIO_CMD_INPUT;
        TRANSACTION_BUFFER.request_timestamp = timeout;
        TRANSACTION_BUFFER.request_target = channel << 8;
        TRANSACTION_BUFFER.data_width = 0;
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

        TRANSACTION_BUFFER.reply_timestamp.get()
    }
}

pub extern "C" fn input_data(channel: i32) -> i32 {
    unsafe {
        TRANSACTION_BUFFER.request_cmd = RTIO_CMD_INPUT;
        TRANSACTION_BUFFER.request_timestamp = -1;
        TRANSACTION_BUFFER.request_target = channel << 8;
        TRANSACTION_BUFFER.data_width = 0;

        let status = await_reply_status();

        process_exceptional_input_status(status, channel);

        TRANSACTION_BUFFER.reply_data.get()
    }
}

pub extern "C" fn input_timestamped_data(timeout: i64, channel: i32) -> TimestampedData {
    unsafe {
        TRANSACTION_BUFFER.request_cmd = RTIO_CMD_INPUT;
        TRANSACTION_BUFFER.request_timestamp = timeout;
        TRANSACTION_BUFFER.request_target = channel << 8;
        TRANSACTION_BUFFER.data_width = 0;

        let status = await_reply_status();
        process_exceptional_input_status(status, channel);

        TimestampedData {
            timestamp: TRANSACTION_BUFFER.reply_timestamp.get(),
            data: TRANSACTION_BUFFER.reply_data.get(),
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

static mut BATCH_RUNNING: bool = false;

#[repr(C, align(64))]
struct BufferedTransaction {
    // same format as normal transaction to reuse the engine
    // using MaybeUninit to avoid unnecessary zeroing and writes
    request_cmd: MaybeUninit<i8>, // always assuming output (forced in gateware), input not supported
    data_width: i8,
    padding0: MaybeUninit<[i8; 2]>,
    request_target: i32,
    request_timestamp: i64,
    request_data: [MaybeUninit<i32>; 16], // potentially todo: use only as much as needed to reduce memory usage
}

static mut BATCH_BUFFER: Vec<BufferedTransaction> = Vec::new();

pub extern "C" fn batch_start() {
    unsafe {
        if BATCH_RUNNING {
            artiq_raise!("RuntimeError", "Batched mode is already running.");
        }
        let library = KERNEL_IMAGE.as_ref().unwrap();
        library.rebind(b"rtio_output", batch_output as *const ()).unwrap();
        library
            .rebind(b"rtio_output_wide", batch_output_wide as *const ())
            .unwrap();
        // pre-allocate the buffer to avoid early reallocations
        BATCH_BUFFER = Vec::with_capacity(1024);
        BATCH_RUNNING = true; 
    }
}

pub extern "C" fn batch_end() {
    // trigger the batch, and check the reply.
    unsafe {
        BATCH_RUNNING = false;
        if BATCH_BUFFER.len() == 0 {
            return;
        }
        csr::rtio::batch_len_write(BATCH_BUFFER.len() as u32);
        csr::rtio::batch_base_write(BATCH_BUFFER.as_ptr() as u32);

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
            let status = TRANSACTION_BUFFER.reply_status.get();
            if status != 0 {
                // Clear status so we can observe response on the next call
                TRANSACTION_BUFFER.reply_status.set(0);
                break status & !(1 << 16);
            }
        };
        // free up memory
        BATCH_BUFFER = Vec::new();
        // len = 0 to indicate we are not in batch mode anymore
        csr::rtio::batch_len_write(0);

        if status != 0 {
            process_exceptional_status(0, status);
        }
    }
}

pub extern "C" fn batch_output(target: i32, data: i32) {
    unsafe {
        BATCH_BUFFER.push(BufferedTransaction {
            request_cmd: MaybeUninit::uninit(),
            data_width: 1,
            request_target: target,
            request_timestamp: NOW,
            request_data: [MaybeUninit::uninit(); 16],
            padding0: MaybeUninit::uninit(),
        });
        BATCH_BUFFER[BATCH_BUFFER.len() - 1].request_data[0] = MaybeUninit::new(data);
    }
}

pub extern "C" fn batch_output_wide(target: i32, data: CSlice<i32>) {
    unsafe {
        BATCH_BUFFER.push(BufferedTransaction {
            request_cmd: MaybeUninit::uninit(),
            data_width: data.len() as i8,
            request_target: target,
            request_timestamp: NOW,
            request_data: [MaybeUninit::uninit(); 16],
            padding0: MaybeUninit::uninit(),
        });
        copy_nonoverlapping(
            data.as_ptr(), 
            BATCH_BUFFER[BATCH_BUFFER.len() - 1].request_data.as_mut_ptr() as *mut i32,
            data.len()
        );
    }
}