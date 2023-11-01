#![no_std]
#![feature(never_type)]

extern crate log;
extern crate crc;
extern crate embedded_hal;
extern crate core_io;
extern crate io;
extern crate libboard_zynq;
extern crate libregister;
extern crate libconfig;
extern crate libcortex_a9;
extern crate libasync;
extern crate log_buffer;

#[path = "../../../build/pl.rs"]
pub mod pl;
pub mod drtioaux_proto;
pub mod drtio_routing;
pub mod logger;
#[cfg(has_si5324)]
pub mod si5324;
#[cfg(has_drtio)]
pub mod drtioaux;
#[cfg(has_drtio)]
pub mod drtioaux_async;
#[cfg(has_drtio)]
#[path = "../../../build/mem.rs"]
pub mod mem;
#[cfg(all(feature = "target_kasli_soc", has_drtio))]
pub mod io_expander;
#[cfg(has_grabber)]
pub mod grabber;

use core::{cmp, str};
use libboard_zynq::slcr;
use libregister::RegisterW;

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

pub fn init_gateware() {
    // Set up PS->PL clocks
    slcr::RegisterBlock::unlocked(|slcr| {
        // As we are touching the mux, the clock may glitch, so reset the PL.
        slcr.fpga_rst_ctrl.write(
            slcr::FpgaRstCtrl::zeroed()
                .fpga0_out_rst(true)
                .fpga1_out_rst(true)
                .fpga2_out_rst(true)
                .fpga3_out_rst(true)
        );
        slcr.fpga0_clk_ctrl.write(
            slcr::Fpga0ClkCtrl::zeroed()
                .src_sel(slcr::PllSource::IoPll)
                .divisor0(8)
                .divisor1(1)
        );
        slcr.fpga_rst_ctrl.write(
            slcr::FpgaRstCtrl::zeroed()
        );
    });
}