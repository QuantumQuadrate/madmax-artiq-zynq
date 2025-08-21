#![no_std]
#![no_main]
#![recursion_limit = "1024"] // for futures_util::select!
#![feature(alloc_error_handler)]
#![feature(const_btree_len)]
#![allow(internal_features)]
#![feature(lang_items)]
#![allow(unexpected_cfgs)]

#[macro_use]
extern crate alloc;

#[cfg(all(feature = "target_kasli_soc", has_virtual_leds))]
use core::cell::RefCell;

use libasync::task;
#[cfg(has_drtio_eem)]
use libboard_artiq::drtio_eem;
#[cfg(feature = "target_kasli_soc")]
use libboard_artiq::io_expander;
#[cfg(has_cxp_grabber)]
use libboard_artiq::{cxp_grabber, cxp_phys};
use libboard_artiq::{i2c, identifier_read, logger, pl};
use libboard_zynq::{gic, mpcore, timer};
use libconfig;
use libcortex_a9::l2c::enable_l2_cache;
use libsupport_zynq::{exception_vectors, ram};
use log::{info, warn};

mod analyzer;
mod comms;

mod mgmt;
mod moninj;
mod panic;
mod proto_async;
mod rpc_async;
mod rtio_clocking;
mod rtio_dma;
mod rtio_mgt;
#[cfg(has_drtio)]
mod subkernel;

// linker symbols
extern "C" {
    static __exceptions_start: u32;
}

#[cfg(all(feature = "target_kasli_soc", has_virtual_leds))]
async fn io_expanders_service(
    i2c_bus: RefCell<&mut libboard_zynq::i2c::I2c>,
    io_expander0: RefCell<io_expander::IoExpander>,
    io_expander1: RefCell<io_expander::IoExpander>,
) {
    loop {
        task::r#yield().await;
        io_expander0
            .borrow_mut()
            .service(&mut i2c_bus.borrow_mut())
            .expect("I2C I/O expander #0 service failed");
        io_expander1
            .borrow_mut()
            .service(&mut i2c_bus.borrow_mut())
            .expect("I2C I/O expander #1 service failed");
    }
}

#[cfg(has_grabber)]
mod grabber {
    use libboard_artiq::grabber;
    use libboard_zynq::timer;

    pub async fn grabber_thread() {
        loop {
            grabber::tick();
            timer::async_delay_ms(200).await;
        }
    }
}

static mut LOG_BUFFER: [u8; 1 << 17] = [0; 1 << 17];

#[no_mangle]
pub fn main_core0() {
    unsafe {
        exception_vectors::set_vector_table(&__exceptions_start as *const u32 as u32);
    }
    enable_l2_cache(0x8);
    timer::start();

    let buffer_logger = unsafe { logger::BufferLogger::new(&mut LOG_BUFFER[..]) };
    buffer_logger.set_uart_log_level(log::LevelFilter::Info);
    buffer_logger.register();
    log::set_max_level(log::LevelFilter::Info);

    info!("NAR3/Zynq7000 starting...");

    ram::init_alloc_core0();
    gic::InterruptController::gic(mpcore::RegisterBlock::mpcore()).enable_interrupts();

    info!("gateware ident: {}", identifier_read(&mut [0; 64]));

    i2c::init();
    #[cfg(feature = "target_kasli_soc")]
    {
        let i2c_bus = i2c::get_bus();
        let mut io_expander0 = io_expander::IoExpander::new(i2c_bus, 0).unwrap();
        let mut io_expander1 = io_expander::IoExpander::new(i2c_bus, 1).unwrap();
        io_expander0
            .init(i2c_bus)
            .expect("I2C I/O expander #0 initialization failed");
        io_expander1
            .init(i2c_bus)
            .expect("I2C I/O expander #1 initialization failed");

        // Drive CLK_SEL to true
        #[cfg(has_si549)]
        io_expander0.set(1, 7, true);

        // Drive TX_DISABLE to false on SFP0..3
        io_expander0.set(0, 1, false);
        io_expander1.set(0, 1, false);
        io_expander0.set(1, 1, false);
        io_expander1.set(1, 1, false);
        io_expander0.service(i2c_bus).unwrap();
        io_expander1.service(i2c_bus).unwrap();
        #[cfg(has_virtual_leds)]
        task::spawn(io_expanders_service(
            RefCell::new(i2c_bus),
            RefCell::new(io_expander0),
            RefCell::new(io_expander1),
        ));
    }

    if let Err(err) = libconfig::init() {
        warn!("config initialization failed: {}", err);
    }
    rtio_clocking::init();

    #[cfg(has_drtio_eem)]
    drtio_eem::init();

    #[cfg(has_grabber)]
    task::spawn(grabber::grabber_thread());

    #[cfg(has_cxp_grabber)]
    {
        cxp_phys::setup();
        task::spawn(cxp_grabber::thread(i2c::get_bus()));
    }

    comms::main();
}
