#![no_std]
#![feature(c_variadic)]
#![feature(const_btree_new)]
#![feature(inline_const)]
#![feature(naked_functions)]

#[macro_use]
extern crate alloc;

use alloc::{collections::BTreeMap, string::String};

use io::{Cursor, ProtoRead};
use libasync::block_async;
use libconfig::Config;
use log::{error, warn};
#[cfg(has_drtiosat)]
pub use pl::csr::drtiosat as rtio_core;
#[cfg(has_rtio_core)]
pub use pl::csr::rtio_core;
use void::Void;

pub mod eh_artiq;
pub mod i2c;
pub mod irq;
pub mod kernel;
pub mod rpc;
#[cfg(ki_impl = "csr")]
#[path = "rtio_csr.rs"]
pub mod rtio;
#[cfg(ki_impl = "acp")]
#[path = "rtio_acp.rs"]
pub mod rtio;
#[rustfmt::skip]
#[path = "../../../build/pl.rs"]
pub mod pl;


#[derive(Debug, Clone)]
pub struct RPCException {
    pub id: u32,
    pub message: u32,
    pub param: [i64; 3],
    pub file: u32,
    pub line: i32,
    pub column: i32,
    pub function: u32,
}

pub static mut SEEN_ASYNC_ERRORS: u8 = 0;

pub const ASYNC_ERROR_COLLISION: u8 = 1 << 0;
pub const ASYNC_ERROR_BUSY: u8 = 1 << 1;
pub const ASYNC_ERROR_SEQUENCE_ERROR: u8 = 1 << 2;

pub unsafe fn get_async_errors() -> u8 {
    let errors = SEEN_ASYNC_ERRORS;
    SEEN_ASYNC_ERRORS = 0;
    errors
}

fn wait_for_async_rtio_error() -> nb::Result<(), Void> {
    unsafe {
        #[cfg(has_rtio_core)]
        let errors = rtio_core::async_error_read();
        #[cfg(has_drtiosat)]
        let errors = rtio_core::protocol_error_read();
        if errors != 0 {
            Ok(())
        } else {
            Err(nb::Error::WouldBlock)
        }
    }
}

pub async fn report_async_rtio_errors() {
    loop {
        let _ = block_async!(wait_for_async_rtio_error()).await;
        unsafe {
            #[cfg(has_rtio_core)]
            let errors = rtio_core::async_error_read();
            #[cfg(has_drtiosat)]
            let errors = rtio_core::protocol_error_read();
            if errors & ASYNC_ERROR_COLLISION != 0 {
                let channel = rtio_core::collision_channel_read();
                error!(
                    "RTIO collision involving channel 0x{:04x}:{}",
                    channel,
                    resolve_channel_name(channel as u32)
                );
            }
            if errors & ASYNC_ERROR_BUSY != 0 {
                let channel = rtio_core::busy_channel_read();
                error!(
                    "RTIO busy error involving channel 0x{:04x}:{}",
                    channel,
                    resolve_channel_name(channel as u32)
                );
            }
            if errors & ASYNC_ERROR_SEQUENCE_ERROR != 0 {
                let channel = rtio_core::sequence_error_channel_read();
                error!(
                    "RTIO sequence error involving channel 0x{:04x}:{}",
                    channel,
                    resolve_channel_name(channel as u32)
                );
            }
            SEEN_ASYNC_ERRORS = errors;
            #[cfg(has_rtio_core)]
            rtio_core::async_error_write(errors);
            #[cfg(has_drtiosat)]
            rtio_core::protocol_error_write(errors);
        }
    }
}

static mut RTIO_DEVICE_MAP: BTreeMap<u32, String> = BTreeMap::new();

fn read_device_map(cfg: &Config) -> BTreeMap<u32, String> {
    let mut device_map: BTreeMap<u32, String> = BTreeMap::new();
    let _ = cfg
        .read("device_map")
        .and_then(|raw_bytes| {
            let mut bytes_cr = Cursor::new(raw_bytes);
            let size = bytes_cr.read_u32().unwrap();
            for _ in 0..size {
                let channel = bytes_cr.read_u32().unwrap();
                let device_name = bytes_cr.read_string().unwrap();
                if let Some(old_entry) = device_map.insert(channel, device_name.clone()) {
                    warn!(
                        "conflicting device map entries for RTIO channel {}: '{}' and '{}'",
                        channel, old_entry, device_name
                    );
                }
            }
            Ok(())
        })
        .or_else(|err| {
            warn!(
                "error reading device map ({}), device names will not be available in RTIO error messages",
                err
            );
            Err(err)
        });
    device_map
}

fn _resolve_channel_name(channel: u32, device_map: &BTreeMap<u32, String>) -> String {
    match device_map.get(&channel) {
        Some(val) => val.clone(),
        None => String::from("unknown"),
    }
}

pub fn resolve_channel_name(channel: u32) -> String {
    _resolve_channel_name(channel, unsafe { &RTIO_DEVICE_MAP })
}

pub fn setup_device_map(cfg: &Config) {
    unsafe {
        RTIO_DEVICE_MAP = read_device_map(cfg);
    }
}
