#![no_std]
#![feature(never_type)]
#![feature(naked_functions)]
#![allow(unexpected_cfgs)]

extern crate alloc;
extern crate core_io;
extern crate crc;
extern crate embedded_hal;
extern crate io;
extern crate libasync;
extern crate libboard_zynq;
extern crate libconfig;
extern crate libcortex_a9;
extern crate libregister;
extern crate log;
extern crate log_buffer;

pub mod drtio_routing;
#[cfg(has_drtio)]
pub mod drtioaux;
#[cfg(has_drtio)]
pub mod drtioaux_async;
pub mod drtioaux_proto;
pub mod fiq;
#[cfg(feature = "target_kasli_soc")]
pub mod io_expander;
pub mod logger;
#[cfg(any(has_drtio, has_cxp_grabber))]
#[rustfmt::skip]
#[path = "../../../build/mem.rs"]
pub mod mem;
#[rustfmt::skip]
#[path = "../../../build/pl.rs"]
pub mod pl;
#[cfg(has_drtio_eem)]
pub mod drtio_eem;
#[cfg(has_grabber)]
pub mod grabber;
#[cfg(has_si5324)]
pub mod si5324;
#[cfg(has_si549)]
pub mod si549;
use alloc::{collections::BTreeMap, string::String};
use core::{cmp, str};

use byteorder::NativeEndian;
use io::{Cursor, ProtoRead};
use libcortex_a9::once_lock::OnceLock;
use log::warn;

#[cfg(has_cxp_grabber)]
pub mod cxp_camera_setup;
#[cfg(has_cxp_grabber)]
pub mod cxp_ctrl;
#[cfg(has_cxp_grabber)]
pub mod cxp_grabber;
#[cfg(all(has_cxp_grabber, has_cxp_led))]
pub mod cxp_led;
#[cfg(has_cxp_grabber)]
pub mod cxp_packet;
#[cfg(has_cxp_grabber)]
pub mod cxp_phys;

#[allow(static_mut_refs)]
pub mod i2c {
    use core::mem::MaybeUninit;

    use libboard_zynq::i2c::I2c;

    static mut I2C_BUS: MaybeUninit<I2c> = MaybeUninit::uninit();

    pub fn init() {
        let mut i2c = I2c::i2c0();
        i2c.init().expect("I2C bus initialization failed");
        unsafe { I2C_BUS.write(i2c) };
    }

    pub fn get_bus() -> &'static mut I2c {
        unsafe { I2C_BUS.assume_init_mut() }
    }
}

pub fn identifier_read(buf: &mut [u8]) -> &str {
    unsafe {
        pl::csr::identifier::address_write(0);
        let len = pl::csr::identifier::data_read();
        let len = cmp::min(len, buf.len() as u8);
        for i in 0..len {
            pl::csr::identifier::address_write(1 + i);
            buf[i as usize] = pl::csr::identifier::data_read();
        }
        str::from_utf8_unchecked(&buf[..len as usize])
    }
}

static RTIO_DEVICE_MAP: OnceLock<BTreeMap<u32, String>> = OnceLock::new();

fn read_device_map() -> BTreeMap<u32, String> {
    let mut device_map: BTreeMap<u32, String> = BTreeMap::new();
    let _ = libconfig::read("device_map")
        .and_then(|raw_bytes| {
            let mut bytes_cr = Cursor::new(raw_bytes);
            let size = bytes_cr.read_u32::<NativeEndian>().unwrap();
            for _ in 0..size {
                let channel = bytes_cr.read_u32::<NativeEndian>().unwrap();
                let device_name = bytes_cr.read_string::<NativeEndian>().unwrap();
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

pub fn resolve_channel_name(channel: u32) -> String {
    match RTIO_DEVICE_MAP
        .get()
        .expect("cannot get device map before it is set up")
        .get(&channel)
    {
        Some(val) => val.clone(),
        None => String::from("unknown"),
    }
}

pub fn setup_device_map() {
    RTIO_DEVICE_MAP
        .set(read_device_map())
        .expect("device map can only be initialized once");
}
