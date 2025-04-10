use core::ptr::{read_volatile, write_volatile};

use cslice::CSlice;

#[cfg(has_drtio)]
use super::{Message, KERNEL_CHANNEL_1TO0};
use crate::{artiq_raise, pl::csr, resolve_channel_name, rtio_core};

pub const RTIO_O_STATUS_WAIT: u8 = 1;
pub const RTIO_O_STATUS_UNDERFLOW: u8 = 2;
pub const RTIO_O_STATUS_DESTINATION_UNREACHABLE: u8 = 4;
pub const RTIO_I_STATUS_WAIT_EVENT: u8 = 1;
pub const RTIO_I_STATUS_OVERFLOW: u8 = 2;
pub const RTIO_I_STATUS_WAIT_STATUS: u8 = 4;
pub const RTIO_I_STATUS_DESTINATION_UNREACHABLE: u8 = 8;

#[repr(C)]
pub struct TimestampedData {
    timestamp: i64,
    data: i32,
}

pub extern "C" fn init() {
    unsafe {
        rtio_core::reset_write(1);
    }
    #[cfg(has_drtio)]
    unsafe {
        KERNEL_CHANNEL_1TO0.as_mut().unwrap().send(Message::RtioInitRequest);
    }
}

pub extern "C" fn get_counter() -> i64 {
    unsafe {
        csr::rtio::counter_update_write(1);
        csr::rtio::counter_read() as i64
    }
}

pub extern "C" fn now_mu() -> i64 {
    unsafe { csr::rtio::now_read() as i64 }
}

pub extern "C" fn at_mu(t: i64) {
    unsafe {
        csr::rtio::now_write(t as u64);
    }
}

pub extern "C" fn delay_mu(dt: i64) {
    unsafe {
        csr::rtio::now_write(csr::rtio::now_read() + dt as u64);
    }
}

// writing the LSB of o_data (offset=0) triggers the RTIO write
#[inline(always)]
pub unsafe fn rtio_o_data_write(offset: usize, data: u32) {
    write_volatile(
        csr::rtio::O_DATA_ADDR.offset((csr::rtio::O_DATA_SIZE - 1 - offset) as isize),
        data,
    );
}

#[inline(always)]
pub unsafe fn rtio_i_data_read(offset: usize) -> u32 {
    read_volatile(csr::rtio::I_DATA_ADDR.offset((csr::rtio::I_DATA_SIZE - 1 - offset) as isize))
}

#[inline(never)]
unsafe fn process_exceptional_status(channel: i32, status: u8) {
    let timestamp = csr::rtio::now_read() as i64;
    if status & RTIO_O_STATUS_WAIT != 0 {
        while csr::rtio::o_status_read() & RTIO_O_STATUS_WAIT != 0 {}
    }
    if status & RTIO_O_STATUS_UNDERFLOW != 0 {
        artiq_raise!(
            "RTIOUnderflow",
            format!(
                "RTIO underflow at {{1}} mu, channel 0x{:04x}:{}, slack {{2}} mu",
                channel,
                resolve_channel_name(channel as u32)
            ),
            channel as i64,
            timestamp,
            timestamp - get_counter()
        );
    }
    if status & RTIO_O_STATUS_DESTINATION_UNREACHABLE != 0 {
        artiq_raise!(
            "RTIODestinationUnreachable",
            format!(
                "RTIO destination unreachable, output, at {{0}} mu, channel 0x{:04x}:{}",
                channel,
                resolve_channel_name(channel as u32)
            ),
            timestamp,
            channel as i64,
            0
        );
    }
}

pub extern "C" fn output(target: i32, data: i32) {
    unsafe {
        csr::rtio::target_write(target as u32);
        // writing target clears o_data
        rtio_o_data_write(0, data as _);
        let status = csr::rtio::o_status_read();
        if status != 0 {
            process_exceptional_status(target >> 8, status);
        }
    }
}

pub extern "C" fn output_wide(target: i32, data: &CSlice<i32>) {
    unsafe {
        csr::rtio::target_write(target as u32);
        // writing target clears o_data
        for i in (0..data.len()).rev() {
            rtio_o_data_write(i, data[i] as _)
        }
        let status = csr::rtio::o_status_read();
        if status != 0 {
            process_exceptional_status(target >> 8, status);
        }
    }
}

pub extern "C" fn input_timestamp(timeout: i64, channel: i32) -> i64 {
    unsafe {
        csr::rtio::target_write((channel as u32) << 8);
        csr::rtio::i_timeout_write(timeout as u64);

        let mut status = RTIO_I_STATUS_WAIT_STATUS;
        while status & RTIO_I_STATUS_WAIT_STATUS != 0 {
            status = csr::rtio::i_status_read();
        }

        if status & RTIO_I_STATUS_OVERFLOW != 0 {
            artiq_raise!(
                "RTIOOverflow",
                format!(
                    "RTIO input overflow on channel 0x{:04x}:{}",
                    channel,
                    resolve_channel_name(channel as u32)
                ),
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
                format!(
                    "RTIO destination unreachable, input, on channel 0x{:04x}:{}",
                    channel,
                    resolve_channel_name(channel as u32)
                ),
                channel as i64,
                0,
                0
            );
        }

        csr::rtio::i_timestamp_read() as i64
    }
}

pub extern "C" fn input_data(channel: i32) -> i32 {
    unsafe {
        csr::rtio::target_write((channel as u32) << 8);
        csr::rtio::i_timeout_write(0xffffffff_ffffffff);

        let mut status = RTIO_I_STATUS_WAIT_STATUS;
        while status & RTIO_I_STATUS_WAIT_STATUS != 0 {
            status = csr::rtio::i_status_read();
        }

        if status & RTIO_I_STATUS_OVERFLOW != 0 {
            artiq_raise!(
                "RTIOOverflow",
                format!(
                    "RTIO input overflow on channel 0x{:04x}:{}",
                    channel,
                    resolve_channel_name(channel as u32)
                ),
                channel as i64,
                0,
                0
            );
        }
        if status & RTIO_I_STATUS_DESTINATION_UNREACHABLE != 0 {
            artiq_raise!(
                "RTIODestinationUnreachable",
                format!(
                    "RTIO destination unreachable, input, on channel 0x{:04x}:{}",
                    channel,
                    resolve_channel_name(channel as u32)
                ),
                channel as i64,
                0,
                0
            );
        }

        rtio_i_data_read(0) as i32
    }
}

pub extern "C" fn input_timestamped_data(timeout: i64, channel: i32) -> TimestampedData {
    unsafe {
        csr::rtio::target_write((channel as u32) << 8);
        csr::rtio::i_timeout_write(timeout as u64);

        let mut status = RTIO_I_STATUS_WAIT_STATUS;
        while status & RTIO_I_STATUS_WAIT_STATUS != 0 {
            status = csr::rtio::i_status_read();
        }

        if status & RTIO_I_STATUS_OVERFLOW != 0 {
            artiq_raise!(
                "RTIOOverflow",
                format!(
                    "RTIO input overflow on channel 0x{:04x}:{}",
                    channel,
                    resolve_channel_name(channel as u32)
                ),
                channel as i64,
                0,
                0
            );
        }
        if status & RTIO_I_STATUS_WAIT_EVENT != 0 {
            return TimestampedData { timestamp: -1, data: 0 };
        }
        if status & RTIO_I_STATUS_DESTINATION_UNREACHABLE != 0 {
            artiq_raise!(
                "RTIODestinationUnreachable",
                format!(
                    "RTIO destination unreachable, input, on channel 0x{:04x}:{}",
                    channel,
                    resolve_channel_name(channel as u32)
                ),
                channel as i64,
                0,
                0
            );
        }

        TimestampedData {
            timestamp: csr::rtio::i_timestamp_read() as i64,
            data: rtio_i_data_read(0) as i32,
        }
    }
}

pub fn write_log(data: &[i8]) {
    unsafe {
        csr::rtio::target_write(csr::CONFIG_RTIO_LOG_CHANNEL << 8);

        let mut word: u32 = 0;
        for i in 0..data.len() {
            word <<= 8;
            word |= data[i] as u32;
            if i % 4 == 3 {
                rtio_o_data_write(0, word);
                word = 0;
            }
        }

        if word != 0 {
            rtio_o_data_write(0, word);
        }
    }
}
