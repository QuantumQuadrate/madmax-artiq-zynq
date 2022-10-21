#![no_std]
#![no_main]
#![recursion_limit="1024"]  // for futures_util::select!
#![feature(alloc_error_handler)]
#![feature(panic_info_message)]
#![feature(c_variadic)]
#![feature(const_btree_new)]
#![feature(const_in_array_repeat_expressions)]
#![feature(naked_functions)]
#![feature(asm)]

extern crate alloc;

use log::{info, warn, error};

use libboard_zynq::{timer::GlobalTimer, mpcore, gic};
use libasync::{task, block_async};
use libsupport_zynq::ram;
use nb;
use void::Void;
use libconfig::Config;
use libcortex_a9::l2c::enable_l2_cache;
use libboard_artiq::{logger, identifier_read, init_gateware, pl};

const ASYNC_ERROR_COLLISION: u8 = 1 << 0;
const ASYNC_ERROR_BUSY: u8 = 1 << 1;
const ASYNC_ERROR_SEQUENCE_ERROR: u8 = 1 << 2;

mod proto_async;
mod comms;
mod rpc;
#[cfg(ki_impl = "csr")]
#[path = "rtio_csr.rs"]
mod rtio;
#[cfg(ki_impl = "acp")]
#[path = "rtio_acp.rs"]
mod rtio;
mod rtio_mgt;
mod rtio_clocking;
mod kernel;
mod moninj;
mod eh_artiq;
mod panic;
mod mgmt;
mod analyzer;
mod irq;
mod i2c;

static mut SEEN_ASYNC_ERRORS: u8 = 0;

pub unsafe fn get_async_errors() -> u8 {
    let errors = SEEN_ASYNC_ERRORS;
    SEEN_ASYNC_ERRORS = 0;
    errors
}

fn wait_for_async_rtio_error() -> nb::Result<(), Void> {
    unsafe {
        if pl::csr::rtio_core::async_error_read() != 0 {
            Ok(())
        } else {
            Err(nb::Error::WouldBlock)
        }
    }
}

async fn report_async_rtio_errors() {
    loop {
        let _ = block_async!(wait_for_async_rtio_error()).await;
        unsafe {
            let errors = pl::csr::rtio_core::async_error_read();
            if errors & ASYNC_ERROR_COLLISION != 0 {
                error!("RTIO collision involving channel {}",
                       pl::csr::rtio_core::collision_channel_read());
            }
            if errors & ASYNC_ERROR_BUSY != 0 {
                error!("RTIO busy error involving channel {}",
                       pl::csr::rtio_core::busy_channel_read());
            }
            if errors & ASYNC_ERROR_SEQUENCE_ERROR != 0 {
                error!("RTIO sequence error involving channel {}",
                       pl::csr::rtio_core::sequence_error_channel_read());
            }
            SEEN_ASYNC_ERRORS = errors;
            pl::csr::rtio_core::async_error_write(errors);
        }
    }
}



static mut LOG_BUFFER: [u8; 1<<17] = [0; 1<<17];

#[no_mangle]
pub fn main_core0() {
    enable_l2_cache(0x8);
    let mut timer = GlobalTimer::start();

    let buffer_logger = unsafe {
        logger::BufferLogger::new(&mut LOG_BUFFER[..])
    };
    buffer_logger.set_uart_log_level(log::LevelFilter::Info);
    buffer_logger.register();
    log::set_max_level(log::LevelFilter::Info);

    info!("NAR3/Zynq7000 starting...");

    ram::init_alloc_core0();
    gic::InterruptController::gic(mpcore::RegisterBlock::mpcore()).enable_interrupts();

    init_gateware();
    info!("gateware ident: {}", identifier_read(&mut [0; 64]));

    i2c::init();

    let cfg = match Config::new() {
        Ok(cfg) => cfg,
        Err(err) => {
            warn!("config initialization failed: {}", err);
            Config::new_dummy()
        }
    };

    rtio_clocking::init(&mut timer, &cfg);

    task::spawn(report_async_rtio_errors());

    comms::main(timer, cfg);
}
