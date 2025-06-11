#![no_std]
#![no_main]
#![allow(internal_features)]
#![feature(alloc_error_handler, never_type)]
#![feature(lang_items)]
#![allow(unexpected_cfgs)]

#[macro_use]
extern crate log;
extern crate byteorder;
extern crate core_io;
extern crate crc;
extern crate cslice;
extern crate embedded_hal;

extern crate io;
extern crate ksupport;
extern crate libboard_artiq;
extern crate libboard_zynq;
extern crate libconfig;
extern crate libcortex_a9;
extern crate libregister;
extern crate libsupport_zynq;

extern crate unwind;

extern crate alloc;

use analyzer::Analyzer;
use dma::Manager as DmaManager;
use drtiosat_aux::process_aux_packets;
use embedded_hal::blocking::delay::DelayUs;
#[cfg(has_drtio_eem)]
use libboard_artiq::drtio_eem;
#[cfg(has_grabber)]
use libboard_artiq::grabber;
#[cfg(feature = "target_kasli_soc")]
use libboard_artiq::io_expander;
#[cfg(has_si549)]
use libboard_artiq::si549;
#[cfg(has_si5324)]
use libboard_artiq::si5324;
use libboard_artiq::{drtio_routing, drtioaux, identifier_read, logger, pl::csr};
#[cfg(feature = "target_kasli_soc")]
use libboard_zynq::error_led::ErrorLED;
use libboard_zynq::{print, println, time::Milliseconds, timer::GlobalTimer};
use libconfig::Config;
use libcortex_a9::{l2c::enable_l2_cache, regs::MPIDR};
use libregister::RegisterR;
use libsupport_zynq::{exception_vectors, ram};
use mgmt::Manager as CoreManager;
use routing::Router;
use subkernel::Manager as KernelManager;

mod analyzer;
mod dma;
mod drtiosat_aux;
mod mgmt;
mod repeater;
mod routing;
mod subkernel;

// linker symbols
extern "C" {
    static __exceptions_start: u32;
}

fn drtiosat_reset(reset: bool) {
    unsafe {
        csr::drtiosat::reset_write(if reset { 1 } else { 0 });
    }
}

fn drtiosat_reset_phy(reset: bool) {
    unsafe {
        csr::drtiosat::reset_phy_write(if reset { 1 } else { 0 });
    }
}

fn drtiosat_link_rx_up() -> bool {
    unsafe { csr::drtiosat::rx_up_read() == 1 }
}

fn drtiosat_tsc_loaded() -> bool {
    unsafe {
        let tsc_loaded = csr::drtiosat::tsc_loaded_read() == 1;
        if tsc_loaded {
            csr::drtiosat::tsc_loaded_write(1);
        }
        tsc_loaded
    }
}

fn toggle_sed_spread(val: u8) {
    unsafe {
        csr::drtiosat::sed_spread_enable_write(val);
    }
}

fn drtiosat_process_errors() {
    let errors;
    unsafe {
        errors = csr::drtiosat::protocol_error_read();
    }
    if errors & 1 != 0 {
        error!("received packet of an unknown type");
    }
    if errors & 2 != 0 {
        error!("received truncated packet");
    }
    if errors & 4 != 0 {
        let destination;
        unsafe {
            destination = csr::drtiosat::buffer_space_timeout_dest_read();
        }
        error!(
            "timeout attempting to get buffer space from CRI, destination=0x{:02x}",
            destination
        )
    }
    if errors & 8 != 0 {
        let channel;
        let timestamp_event;
        let timestamp_counter;
        unsafe {
            channel = csr::drtiosat::underflow_channel_read();
            timestamp_event = csr::drtiosat::underflow_timestamp_event_read() as i64;
            timestamp_counter = csr::drtiosat::underflow_timestamp_counter_read() as i64;
        }
        error!(
            "write underflow, channel={}, timestamp={}, counter={}, slack={}",
            channel,
            timestamp_event,
            timestamp_counter,
            timestamp_event - timestamp_counter
        );
    }
    if errors & 16 != 0 {
        error!("write overflow");
    }
    unsafe {
        csr::drtiosat::protocol_error_write(errors);
    }
}

fn hardware_tick(ts: &mut u64, timer: &mut GlobalTimer) {
    let now = timer.get_time();
    let mut ts_ms = Milliseconds(*ts);
    if now > ts_ms {
        ts_ms = now + Milliseconds(200);
        *ts = ts_ms.0;
        #[cfg(has_grabber)]
        grabber::tick();
    }
}

#[cfg(all(has_si5324, rtio_frequency = "125.0"))]
const SI5324_SETTINGS: si5324::FrequencySettings = si5324::FrequencySettings {
    n1_hs: 5,
    nc1_ls: 8,
    n2_hs: 7,
    n2_ls: 360,
    n31: 63,
    n32: 63,
    bwsel: 4,
    crystal_as_ckin2: true,
};

#[cfg(all(has_si5324, rtio_frequency = "100.0"))]
const SI5324_SETTINGS: si5324::FrequencySettings = si5324::FrequencySettings {
    n1_hs: 5,
    nc1_ls: 10,
    n2_hs: 10,
    n2_ls: 250,
    n31: 50,
    n32: 50,
    bwsel: 4,
    crystal_as_ckin2: true,
};

#[cfg(all(has_si549, rtio_frequency = "125.0"))]
const SI549_SETTINGS: si549::FrequencySetting = si549::FrequencySetting {
    main: si549::DividerConfig {
        hsdiv: 0x058,
        lsdiv: 0,
        fbdiv: 0x04815791F25,
    },
    helper: si549::DividerConfig {
        // 125MHz*32767/32768
        hsdiv: 0x058,
        lsdiv: 0,
        fbdiv: 0x04814E8F442,
    },
};

#[cfg(all(has_si549, rtio_frequency = "100.0"))]
const SI549_SETTINGS: si549::FrequencySetting = si549::FrequencySetting {
    main: si549::DividerConfig {
        hsdiv: 0x06C,
        lsdiv: 0,
        fbdiv: 0x046C5F49797,
    },
    helper: si549::DividerConfig {
        // 100MHz*32767/32768
        hsdiv: 0x06C,
        lsdiv: 0,
        fbdiv: 0x046C5670BBD,
    },
};

static mut LOG_BUFFER: [u8; 1 << 17] = [0; 1 << 17];

#[no_mangle]
pub extern "C" fn main_core0() -> i32 {
    unsafe {
        exception_vectors::set_vector_table(&__exceptions_start as *const u32 as u32);
    }
    enable_l2_cache(0x8);

    let mut timer = GlobalTimer::start();

    let buffer_logger = unsafe { logger::BufferLogger::new(&mut LOG_BUFFER[..]) };
    buffer_logger.set_uart_log_level(log::LevelFilter::Info);
    buffer_logger.register();
    log::set_max_level(log::LevelFilter::Info);

    info!("ARTIQ satellite manager starting...");
    info!("gateware ident {}", identifier_read(&mut [0; 64]));

    ram::init_alloc_core0();

    ksupport::kernel::i2c::init();
    let i2c = ksupport::kernel::i2c::get_bus();

    #[cfg(feature = "target_kasli_soc")]
    let (mut io_expander0, mut io_expander1);
    #[cfg(feature = "target_kasli_soc")]
    {
        io_expander0 = io_expander::IoExpander::new(i2c, 0).unwrap();
        io_expander1 = io_expander::IoExpander::new(i2c, 1).unwrap();
        io_expander0
            .init(i2c)
            .expect("I2C I/O expander #0 initialization failed");
        io_expander1
            .init(i2c)
            .expect("I2C I/O expander #1 initialization failed");

        // Drive CLK_SEL to true
        #[cfg(has_si549)]
        io_expander0.set(1, 7, true);

        // Drive TX_DISABLE to false on SFP0..3
        io_expander0.set(0, 1, false);
        io_expander1.set(0, 1, false);
        io_expander0.set(1, 1, false);
        io_expander1.set(1, 1, false);
        io_expander0.service(i2c).unwrap();
        io_expander1.service(i2c).unwrap();
    }

    #[cfg(has_si5324)]
    si5324::setup(i2c, &SI5324_SETTINGS, si5324::Input::Ckin1, &mut timer).expect("cannot initialize Si5324");
    #[cfg(has_si549)]
    si549::main_setup(&mut timer, &SI549_SETTINGS).expect("cannot initialize main Si549");

    timer.delay_us(100_000);
    info!("Switching SYS clocks...");
    unsafe {
        csr::gt_drtio::stable_clkin_write(1);
    }
    timer.delay_us(50_000); // wait for CPLL/QPLL/MMCM lock
    let clk = unsafe { csr::sys_crg::current_clock_read() };
    if clk == 1 {
        info!("SYS CLK switched successfully");
    } else {
        panic!("SYS CLK did not switch");
    }

    unsafe {
        csr::gt_drtio::txenable_write(0xffffffffu32 as _);
    }

    #[cfg(has_drtio_eem)]
    unsafe {
        csr::eem_transceiver::txenable_write(0xffffffffu32 as _);
    }

    #[cfg(has_si549)]
    si549::helper_setup(&mut timer, &SI549_SETTINGS).expect("cannot initialize helper Si549");

    let mut cfg = match Config::new() {
        Ok(cfg) => cfg,
        Err(err) => {
            warn!("config initialization failed: {}", err);
            Config::new_dummy()
        }
    };

    if let Ok(spread_enable) = cfg.read_str("sed_spread_enable") {
        match spread_enable.as_ref() {
            "1" => toggle_sed_spread(1),
            "0" => toggle_sed_spread(0),
            _ => {
                warn!("sed_spread_enable value not supported (only 1, 0 allowed), disabling by default");
                toggle_sed_spread(0)
            }
        };
    } else {
        info!("SED spreading disabled by default");
        toggle_sed_spread(0);
    }

    #[cfg(has_drtio_eem)]
    {
        drtio_eem::init(&mut timer, &cfg);
        unsafe { csr::eem_transceiver::rx_ready_write(1) }
    }

    #[cfg(has_drtio_routing)]
    let mut repeaters = [repeater::Repeater::default(); csr::DRTIOREP.len()];
    #[cfg(not(has_drtio_routing))]
    let mut repeaters = [repeater::Repeater::default(); 0];
    for i in 0..repeaters.len() {
        repeaters[i] = repeater::Repeater::new(i as u8);
    }
    let mut routing_table = drtio_routing::RoutingTable::default_empty();
    let mut rank = 1;
    let mut destination = 1;

    let mut hardware_tick_ts = 0;

    let mut control = ksupport::kernel::Control::start();

    loop {
        let mut router = Router::new();

        while !drtiosat_link_rx_up() {
            drtiosat_process_errors();
            #[allow(unused_mut)]
            for mut rep in repeaters.iter_mut() {
                rep.service(&routing_table, rank, destination, &mut router, &mut timer);
            }
            #[cfg(feature = "target_kasli_soc")]
            {
                io_expander0.service(i2c).expect("I2C I/O expander #0 service failed");
                io_expander1.service(i2c).expect("I2C I/O expander #1 service failed");
            }

            hardware_tick(&mut hardware_tick_ts, &mut timer);
        }

        info!("uplink is up, switching to recovered clock");
        #[cfg(has_siphaser)]
        {
            si5324::siphaser::select_recovered_clock(i2c, true, &mut timer).expect("failed to switch clocks");
            si5324::siphaser::calibrate_skew(&mut timer).expect("failed to calibrate skew");
        }

        #[cfg(has_wrpll)]
        si549::wrpll::select_recovered_clock(true, &mut timer);

        // Various managers created here, so when link is dropped, all DMA traces
        // are cleared out for a clean slate on subsequent connections,
        // without a manual intervention.
        let mut dma_manager = DmaManager::new();
        let mut analyzer = Analyzer::new();
        let mut kernel_manager = KernelManager::new(&mut control);
        let mut core_manager = CoreManager::new(&mut cfg);

        drtioaux::reset(0);
        drtiosat_reset(false);
        drtiosat_reset_phy(false);

        while drtiosat_link_rx_up() {
            drtiosat_process_errors();
            process_aux_packets(
                &mut repeaters,
                &mut routing_table,
                &mut rank,
                &mut destination,
                &mut timer,
                i2c,
                &mut dma_manager,
                &mut analyzer,
                &mut kernel_manager,
                &mut core_manager,
                &mut router,
            );
            #[allow(unused_mut)]
            for mut rep in repeaters.iter_mut() {
                rep.service(&routing_table, rank, destination, &mut router, &mut timer);
            }
            #[cfg(feature = "target_kasli_soc")]
            {
                io_expander0.service(i2c).expect("I2C I/O expander #0 service failed");
                io_expander1.service(i2c).expect("I2C I/O expander #1 service failed");
            }
            hardware_tick(&mut hardware_tick_ts, &mut timer);
            if drtiosat_tsc_loaded() {
                info!("TSC loaded from uplink");
                for rep in repeaters.iter() {
                    if let Err(e) = rep.sync_tsc(&mut timer) {
                        error!("failed to sync TSC ({:?})", e);
                    }
                }
                if let Err(e) = drtioaux::send(0, &drtioaux::Packet::TSCAck) {
                    error!("aux packet error: {:?}", e);
                }
            }
            if let Some(status) = dma_manager.check_state() {
                info!(
                    "playback done, error: {}, channel: {}, timestamp: {}",
                    status.error, status.channel, status.timestamp
                );
                router.route(
                    drtioaux::Packet::DmaPlaybackStatus {
                        source: destination,
                        destination: status.source,
                        id: status.id,
                        error: status.error,
                        channel: status.channel,
                        timestamp: status.timestamp,
                    },
                    &routing_table,
                    rank,
                    destination,
                );
            }

            kernel_manager.process_kern_requests(
                &mut router,
                &routing_table,
                rank,
                destination,
                &mut dma_manager,
                &timer,
            );

            #[cfg(has_drtio_routing)]
            if let Some((repno, packet)) = router.get_downstream_packet() {
                if let Err(e) = repeaters[repno].aux_send(&packet) {
                    warn!("[REP#{}] Error when sending packet to satellite ({:?})", repno, e)
                }
            }

            if let Some(packet) = router.get_upstream_packet() {
                drtioaux::send(0, &packet).unwrap();
            }
        }

        drtiosat_reset_phy(true);
        drtiosat_reset(true);
        drtiosat_tsc_loaded();
        info!("uplink is down, switching to local oscillator clock");
        #[cfg(has_siphaser)]
        si5324::siphaser::select_recovered_clock(i2c, false, &mut timer).expect("failed to switch clocks");
        #[cfg(has_wrpll)]
        si549::wrpll::select_recovered_clock(false, &mut timer);
    }
}

extern "C" {
    static mut __stack1_start: u32;
}

static mut PANICKED: [bool; 2] = [false; 2];

#[no_mangle]
pub extern "C" fn exception(_vect: u32, _regs: *const u32, pc: u32, ea: u32) {
    fn hexdump(addr: u32) {
        let addr = (addr - addr % 4) as *const u32;
        let mut ptr = addr;
        println!("@ {:08p}", ptr);
        for _ in 0..4 {
            print!("+{:04x}: ", ptr as usize - addr as usize);
            print!("{:08x} ", unsafe { *ptr });
            ptr = ptr.wrapping_offset(1);
            print!("{:08x} ", unsafe { *ptr });
            ptr = ptr.wrapping_offset(1);
            print!("{:08x} ", unsafe { *ptr });
            ptr = ptr.wrapping_offset(1);
            print!("{:08x}\n", unsafe { *ptr });
            ptr = ptr.wrapping_offset(1);
        }
    }

    hexdump(pc);
    hexdump(ea);
    panic!("exception at PC 0x{:x}, EA 0x{:x}", pc, ea)
}

#[panic_handler]
pub fn panic_fmt(info: &core::panic::PanicInfo) -> ! {
    let id = MPIDR.read().cpu_id() as usize;
    print!("Core {} ", id);
    unsafe {
        if PANICKED[id] {
            println!("nested panic!");
            loop {}
        }
        PANICKED[id] = true;
    }
    print!("panic at ");
    if let Some(location) = info.location() {
        print!("{}:{}:{}", location.file(), location.line(), location.column());
    } else {
        print!("unknown location");
    }
    println!(": {}", info.message());

    #[cfg(feature = "target_kasli_soc")]
    {
        let mut err_led = ErrorLED::error_led();
        err_led.toggle(true);
    }

    loop {}
}

#[lang = "eh_personality"]
#[no_mangle]
pub extern "C" fn rust_eh_personality() {}
