#![no_std]
#![no_main]
#![feature(never_type, panic_info_message, asm, naked_functions)]
#![feature(alloc_error_handler)]

#[macro_use]
extern crate log;

extern crate embedded_hal;

extern crate libboard_zynq;
extern crate libboard_artiq;
extern crate libsupport_zynq;
extern crate libcortex_a9;
extern crate libregister;

extern crate unwind;

extern crate alloc;

use libboard_zynq::{i2c::I2c, timer::GlobalTimer, time::Milliseconds, print, println, mpcore, gic, stdio};
use libsupport_zynq::ram;
#[cfg(has_si5324)]
use libboard_artiq::si5324;
#[cfg(feature = "target_kasli_soc")]
use libboard_artiq::io_expander;
use libboard_artiq::{pl::csr, drtio_routing, drtioaux, logger, identifier_read, init_gateware};
use libcortex_a9::{spin_lock_yield, interrupt_handler, regs::{MPIDR, SP}, notify_spin_lock, asm, l2c::enable_l2_cache};
use libregister::{RegisterW, RegisterR};

use embedded_hal::blocking::delay::DelayUs;
use core::sync::atomic::{AtomicBool, Ordering};

mod repeater;

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
    unsafe {
        csr::drtiosat::rx_up_read() == 1
    }
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


#[cfg(has_drtio_routing)]
macro_rules! forward {
    ($routing_table:expr, $destination:expr, $rank:expr, $repeaters:expr, $packet:expr, $timer:expr) => {{
        let hop = $routing_table.0[$destination as usize][$rank as usize];
        if hop != 0 {
            let repno = (hop - 1) as usize;
            if repno < $repeaters.len() {
                return $repeaters[repno].aux_forward($packet, $timer);
            } else {
                return Err(drtioaux::Error::RoutingError);
            }
        }
    }}
}

#[cfg(not(has_drtio_routing))]
macro_rules! forward {
    ($routing_table:expr, $destination:expr, $rank:expr, $repeaters:expr, $packet:expr, $timer:expr) => {}
}

fn process_aux_packet(_repeaters: &mut [repeater::Repeater],
        _routing_table: &mut drtio_routing::RoutingTable, _rank: &mut u8,
        packet: drtioaux::Packet, timer: &mut GlobalTimer, i2c: &mut I2c) -> Result<(), drtioaux::Error> {
    // In the code below, *_chan_sel_write takes an u8 if there are fewer than 256 channels,
    // and u16 otherwise; hence the `as _` conversion.
    match packet {
        drtioaux::Packet::EchoRequest =>
            drtioaux::send(0, &drtioaux::Packet::EchoReply),
        drtioaux::Packet::ResetRequest => {
            info!("resetting RTIO");
            drtiosat_reset(true);
            timer.delay_us(100);
            drtiosat_reset(false);
            for rep in _repeaters.iter() {
                if let Err(e) = rep.rtio_reset(timer) {
                    error!("failed to issue RTIO reset ({:?})", e);
                }
            }
            drtioaux::send(0, &drtioaux::Packet::ResetAck)
        },

        drtioaux::Packet::DestinationStatusRequest { destination: _destination } => {
            #[cfg(has_drtio_routing)]
            let hop = _routing_table.0[_destination as usize][*_rank as usize];
            #[cfg(not(has_drtio_routing))]
            let hop = 0;

            if hop == 0 {
                let errors;
                unsafe {
                    errors = csr::drtiosat::rtio_error_read();
                }
                if errors & 1 != 0 {
                    let channel;
                    unsafe {
                        channel = csr::drtiosat::sequence_error_channel_read();
                        csr::drtiosat::rtio_error_write(1);
                    }
                    drtioaux::send(0,
                        &drtioaux::Packet::DestinationSequenceErrorReply { channel })?;
                } else if errors & 2 != 0 {
                    let channel;
                    unsafe {
                        channel = csr::drtiosat::collision_channel_read();
                        csr::drtiosat::rtio_error_write(2);
                    }
                    drtioaux::send(0,
                        &drtioaux::Packet::DestinationCollisionReply { channel })?;
                } else if errors & 4 != 0 {
                    let channel;
                    unsafe {
                        channel = csr::drtiosat::busy_channel_read();
                        csr::drtiosat::rtio_error_write(4);
                    }
                    drtioaux::send(0,
                        &drtioaux::Packet::DestinationBusyReply { channel })?;
                }
                else {
                    drtioaux::send(0, &drtioaux::Packet::DestinationOkReply)?;
                }
            }

            #[cfg(has_drtio_routing)]
            {
                if hop != 0 {
                    let hop = hop as usize;
                    if hop <= csr::DRTIOREP.len() {
                        let repno = hop - 1;
                        match _repeaters[repno].aux_forward(&drtioaux::Packet::DestinationStatusRequest {
                            destination: _destination
                        }, timer) {
                            Ok(()) => (),
                            Err(drtioaux::Error::LinkDown) => drtioaux::send(0, &drtioaux::Packet::DestinationDownReply)?,
                            Err(e) => {
                                drtioaux::send(0, &drtioaux::Packet::DestinationDownReply)?;
                                error!("aux error when handling destination status request: {:?}", e);
                            },
                        }
                    } else {
                        drtioaux::send(0, &drtioaux::Packet::DestinationDownReply)?;
                    }
                }
            }

            Ok(())
        }

        #[cfg(has_drtio_routing)]
        drtioaux::Packet::RoutingSetPath { destination, hops } => {
            _routing_table.0[destination as usize] = hops;
            for rep in _repeaters.iter() {
                if let Err(e) = rep.set_path(destination, &hops, timer) {
                    error!("failed to set path ({:?})", e);
                }
            }
            drtioaux::send(0, &drtioaux::Packet::RoutingAck)
        }
        #[cfg(has_drtio_routing)]
        drtioaux::Packet::RoutingSetRank { rank } => {
            *_rank = rank;
            drtio_routing::interconnect_enable_all(_routing_table, rank);

            let rep_rank = rank + 1;
            for rep in _repeaters.iter() {
                if let Err(e) = rep.set_rank(rep_rank, timer) {
                    error!("failed to set rank ({:?})", e);
                }
            }

            info!("rank: {}", rank);
            info!("routing table: {}", _routing_table);

            drtioaux::send(0, &drtioaux::Packet::RoutingAck)
        }

        #[cfg(not(has_drtio_routing))]
        drtioaux::Packet::RoutingSetPath { destination: _, hops: _ } => {
            drtioaux::send(0, &drtioaux::Packet::RoutingAck)
        }
        #[cfg(not(has_drtio_routing))]
        drtioaux::Packet::RoutingSetRank { rank: _ } => {
            drtioaux::send(0, &drtioaux::Packet::RoutingAck)
        }

        drtioaux::Packet::MonitorRequest { destination: _destination, channel, probe } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet, timer);
            let value;
            #[cfg(has_rtio_moninj)]
            unsafe {
                csr::rtio_moninj::mon_chan_sel_write(channel as _);
                csr::rtio_moninj::mon_probe_sel_write(probe);
                csr::rtio_moninj::mon_value_update_write(1);
                value = csr::rtio_moninj::mon_value_read() as u64;
            }
            #[cfg(not(has_rtio_moninj))]
            {
                value = 0;
            }
            let reply = drtioaux::Packet::MonitorReply { value: value };
            drtioaux::send(0, &reply)
        },
        drtioaux::Packet::InjectionRequest { destination: _destination, channel, 
                                             overrd, value } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet, timer);
            #[cfg(has_rtio_moninj)]
            unsafe {
                csr::rtio_moninj::inj_chan_sel_write(channel as _);
                csr::rtio_moninj::inj_override_sel_write(overrd);
                csr::rtio_moninj::inj_value_write(value);
            }
            Ok(())
        },
        drtioaux::Packet::InjectionStatusRequest { destination: _destination, 
                                                   channel, overrd } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet, timer);
            let value;
            #[cfg(has_rtio_moninj)]
            unsafe {
                csr::rtio_moninj::inj_chan_sel_write(channel as _);
                csr::rtio_moninj::inj_override_sel_write(overrd);
                value = csr::rtio_moninj::inj_value_read();
            }
            #[cfg(not(has_rtio_moninj))]
            {
                value = 0;
            }
            drtioaux::send(0, &drtioaux::Packet::InjectionStatusReply { value: value })
        },

        drtioaux::Packet::I2cStartRequest { destination: _destination, busno: _busno } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet, timer);
            let succeeded = i2c.start().is_ok();
            drtioaux::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded })
        }
        drtioaux::Packet::I2cRestartRequest { destination: _destination, busno: _busno } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet, timer);
            let succeeded = i2c.restart().is_ok();
            drtioaux::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded })
        }
        drtioaux::Packet::I2cStopRequest { destination: _destination, busno: _busno } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet, timer);
            let succeeded = i2c.stop().is_ok();
            drtioaux::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded })
        }
        drtioaux::Packet::I2cWriteRequest { destination: _destination, busno: _busno, data } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet, timer);
            match i2c.write(data) {
                Ok(ack) => drtioaux::send(0,
                    &drtioaux::Packet::I2cWriteReply { succeeded: true, ack: ack }),
                Err(_) => drtioaux::send(0,
                    &drtioaux::Packet::I2cWriteReply { succeeded: false, ack: false })
            }
        }
        drtioaux::Packet::I2cReadRequest { destination: _destination, busno: _busno, ack } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet, timer);
            match i2c.read(ack) {
                Ok(data) => drtioaux::send(0,
                    &drtioaux::Packet::I2cReadReply { succeeded: true, data: data }),
                Err(_) => drtioaux::send(0,
                    &drtioaux::Packet::I2cReadReply { succeeded: false, data: 0xff })
            }
        }
        drtioaux::Packet::I2cSwitchSelectRequest { destination: _destination, busno: _busno, address, mask } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet, timer);
            let ch = match mask { //decode from mainline, PCA9548-centric API
                0x00 => None,
                0x01 => Some(0),
                0x02 => Some(1),
                0x04 => Some(2),
                0x08 => Some(3),
                0x10 => Some(4),
                0x20 => Some(5),
                0x40 => Some(6),
                0x80 => Some(7),
                _ => { return drtioaux::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: false }); }
            };
            let succeeded = i2c.pca954x_select(address, ch).is_ok();
            drtioaux::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded })
        }

        drtioaux::Packet::SpiSetConfigRequest { destination: _destination, busno: _busno, 
                                                flags: _flags, length: _length, div: _div, cs: _cs } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet, timer);
            // todo: reimplement when/if SPI is available
            //let succeeded = spi::set_config(busno, flags, length, div, cs).is_ok();
            drtioaux::send(0,
                &drtioaux::Packet::SpiBasicReply { succeeded: false })
        },
        drtioaux::Packet::SpiWriteRequest { destination: _destination, busno: _busno, data: _data } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet, timer);
            // todo: reimplement when/if SPI is available
            //let succeeded = spi::write(busno, data).is_ok();
            drtioaux::send(0,
                &drtioaux::Packet::SpiBasicReply { succeeded: false })
        }
        drtioaux::Packet::SpiReadRequest { destination: _destination, busno: _busno } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet, timer);
            // todo: reimplement when/if SPI is available
            // match spi::read(busno) {
            //     Ok(data) => drtioaux::send(0,
            //         &drtioaux::Packet::SpiReadReply { succeeded: true, data: data }),
            //     Err(_) => drtioaux::send(0,
            //         &drtioaux::Packet::SpiReadReply { succeeded: false, data: 0 })
            // }
            drtioaux::send(0,
                &drtioaux::Packet::SpiReadReply { succeeded: false, data: 0 })
        }

        _ => {
            warn!("received unexpected aux packet");
            Ok(())
        }
    }
}

fn process_aux_packets(repeaters: &mut [repeater::Repeater],
        routing_table: &mut drtio_routing::RoutingTable, rank: &mut u8, 
        timer: &mut GlobalTimer, i2c: &mut I2c) {
    let result =
        drtioaux::recv(0).and_then(|packet| {
            if let Some(packet) = packet {
                process_aux_packet(repeaters, routing_table, rank, packet, timer, i2c)
            } else {
                Ok(())
            }
        });
    match result {
        Ok(()) => (),
        Err(e) => warn!("aux packet error ({:?})", e)
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
        error!("timeout attempting to get buffer space from CRI, destination=0x{:02x}", destination)
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
        error!("write underflow, channel={}, timestamp={}, counter={}, slack={}",
               channel, timestamp_event, timestamp_counter, timestamp_event-timestamp_counter);
    }
    if errors & 16 != 0 {
        error!("write overflow");
    }
    unsafe {
        csr::drtiosat::protocol_error_write(errors);
    }
}


#[cfg(has_rtio_crg)]
fn init_rtio_crg(timer: &mut GlobalTimer) {
    unsafe {
        csr::rtio_crg::pll_reset_write(0);
    }
    timer.delay_us(150);
    let locked = unsafe { csr::rtio_crg::pll_locked_read() != 0 };
    if !locked {
        error!("RTIO clock failed");
    }
    else {
        info!("RTIO PLL locked");
    }
}

#[cfg(not(has_rtio_crg))]
fn init_rtio_crg(_timer: &mut GlobalTimer) { }

fn hardware_tick(ts: &mut u64, timer: &mut GlobalTimer) {
    let now = timer.get_time();
    let mut ts_ms = Milliseconds(*ts);
    if now > ts_ms {
        ts_ms = now + Milliseconds(200);
        *ts = ts_ms.0;
    }
}

#[cfg(all(has_si5324, rtio_frequency = "125.0"))]
const SI5324_SETTINGS: si5324::FrequencySettings
    = si5324::FrequencySettings {
    n1_hs  : 5,
    nc1_ls : 8,
    n2_hs  : 7,
    n2_ls  : 360,
    n31    : 63,
    n32    : 63,
    bwsel  : 4,
    crystal_ref: true
};

#[cfg(all(has_si5324, rtio_frequency = "100.0"))]
const SI5324_SETTINGS: si5324::FrequencySettings
    = si5324::FrequencySettings {
    n1_hs  : 5,
    nc1_ls : 10,
    n2_hs  : 10,
    n2_ls  : 250,
    n31    : 50,
    n32    : 50,
    bwsel  : 4,
    crystal_ref: true
};

static mut LOG_BUFFER: [u8; 1<<17] = [0; 1<<17];

#[no_mangle]
pub extern fn main_core0() -> i32 {
    enable_l2_cache(0x8);

    let mut timer = GlobalTimer::start();

    let buffer_logger = unsafe {
        logger::BufferLogger::new(&mut LOG_BUFFER[..])
    };
    buffer_logger.set_uart_log_level(log::LevelFilter::Info);
    buffer_logger.register();
    log::set_max_level(log::LevelFilter::Info);
    
    init_gateware();

    info!("ARTIQ satellite manager starting...");
    info!("gateware ident {}", identifier_read(&mut [0; 64]));

    ram::init_alloc_core0();

    let mut i2c = I2c::i2c0();
    i2c.init().expect("I2C initialization failed");

    #[cfg(feature = "target_kasli_soc")]
    let (mut io_expander0, mut io_expander1);
    #[cfg(feature = "target_kasli_soc")]
    {
        io_expander0 = io_expander::IoExpander::new(&mut i2c, 0).unwrap();
        io_expander1 = io_expander::IoExpander::new(&mut i2c, 1).unwrap();
        io_expander0
            .init(&mut i2c)
            .expect("I2C I/O expander #0 initialization failed");
        io_expander1
            .init(&mut i2c)
            .expect("I2C I/O expander #1 initialization failed");
        // Actively drive TX_DISABLE to false on SFP0..3
        io_expander0.set_oe(&mut i2c, 0, 1 << 1).unwrap();
        io_expander1.set_oe(&mut i2c, 0, 1 << 1).unwrap();
        io_expander0.set_oe(&mut i2c, 1, 1 << 1).unwrap();
        io_expander1.set_oe(&mut i2c, 1, 1 << 1).unwrap();
        io_expander0.set(0, 1, false);
        io_expander1.set(0, 1, false);
        io_expander0.set(1, 1, false);
        io_expander1.set(1, 1, false);
        io_expander0.service(&mut i2c).unwrap();
        io_expander1.service(&mut i2c).unwrap();
    }

    #[cfg(has_si5324)]
    si5324::setup(&mut i2c, &SI5324_SETTINGS, si5324::Input::Ckin1, &mut timer).expect("cannot initialize Si5324");

    unsafe {
        csr::drtio_transceiver::stable_clkin_write(1);
    }
    timer.delay_us(1500); // wait for CPLL/QPLL lock

    unsafe {
        csr::drtio_transceiver::txenable_write(0xffffffffu32 as _);
    }
    init_rtio_crg(&mut timer);

    #[cfg(has_drtio_routing)]
    let mut repeaters = [repeater::Repeater::default(); csr::DRTIOREP.len()];
    #[cfg(not(has_drtio_routing))]
    let mut repeaters = [repeater::Repeater::default(); 0];
    for i in 0..repeaters.len() {
        repeaters[i] = repeater::Repeater::new(i as u8);
    } 
    let mut routing_table = drtio_routing::RoutingTable::default_empty();
    let mut rank = 1;

    let mut hardware_tick_ts = 0;

    loop {
        while !drtiosat_link_rx_up() {
            drtiosat_process_errors();
            #[allow(unused_mut)]
            for mut rep in repeaters.iter_mut() {
                rep.service(&routing_table, rank, &mut timer);
            }
            hardware_tick(&mut hardware_tick_ts, &mut timer);
        }

        info!("uplink is up, switching to recovered clock");
        #[cfg(has_siphaser)]
        {
            si5324::siphaser::select_recovered_clock(&mut i2c, true, &mut timer).expect("failed to switch clocks");
            si5324::siphaser::calibrate_skew(&mut timer).expect("failed to calibrate skew");
        }

        drtioaux::reset(0);
        drtiosat_reset(false);
        drtiosat_reset_phy(false);

        while drtiosat_link_rx_up() {
            drtiosat_process_errors();
            process_aux_packets(&mut repeaters, &mut routing_table, &mut rank, &mut timer, &mut i2c);
            #[allow(unused_mut)]
            for mut rep in repeaters.iter_mut() {
                rep.service(&routing_table, rank, &mut timer);
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
        }

        drtiosat_reset_phy(true);
        drtiosat_reset(true);
        drtiosat_tsc_loaded();
        info!("uplink is down, switching to local oscillator clock");
        #[cfg(has_siphaser)]
        si5324::siphaser::select_recovered_clock(&mut i2c, false, &mut timer).expect("failed to switch clocks");
    }
}

extern "C" {
    static mut __stack1_start: u32;
}

interrupt_handler!(IRQ, irq, __irq_stack0_start, __irq_stack1_start, {
    if MPIDR.read().cpu_id() == 1{
        let mpcore = mpcore::RegisterBlock::mpcore();
        let mut gic = gic::InterruptController::gic(mpcore);
        let id = gic.get_interrupt_id();
        if id.0 == 0 {
            gic.end_interrupt(id);
            asm::exit_irq();
            SP.write(&mut __stack1_start as *mut _ as u32);
            asm::enable_irq();
            CORE1_RESTART.store(false, Ordering::Relaxed);
            notify_spin_lock();
            main_core1();
        }
    stdio::drop_uart();
    }
    loop {}
});

static mut PANICKED: [bool; 2] = [false; 2];

static CORE1_RESTART: AtomicBool = AtomicBool::new(false);

pub fn restart_core1() {
    let mut interrupt_controller = gic::InterruptController::gic(mpcore::RegisterBlock::mpcore());
    CORE1_RESTART.store(true, Ordering::Relaxed);
    interrupt_controller.send_sgi(gic::InterruptId(0), gic::CPUCore::Core1.into());
    while CORE1_RESTART.load(Ordering::Relaxed) {
        spin_lock_yield();
    }
}

#[no_mangle]
pub fn main_core1() {
    let mut interrupt_controller = gic::InterruptController::gic(mpcore::RegisterBlock::mpcore());
    interrupt_controller.enable_interrupts();

    loop {}
}

#[no_mangle]
pub extern fn exception(_vect: u32, _regs: *const u32, pc: u32, ea: u32) {

    fn hexdump(addr: u32) {
        let addr = (addr - addr % 4) as *const u32;
        let mut ptr  = addr;
        println!("@ {:08p}", ptr);
        for _ in 0..4 {
            print!("+{:04x}: ", ptr as usize - addr as usize);
            print!("{:08x} ",   unsafe { *ptr }); ptr = ptr.wrapping_offset(1);
            print!("{:08x} ",   unsafe { *ptr }); ptr = ptr.wrapping_offset(1);
            print!("{:08x} ",   unsafe { *ptr }); ptr = ptr.wrapping_offset(1);
            print!("{:08x}\n",  unsafe { *ptr }); ptr = ptr.wrapping_offset(1);
        }
    }

    hexdump(pc);
    hexdump(ea);
    panic!("exception at PC 0x{:x}, EA 0x{:x}", pc, ea)
}

#[no_mangle] // https://github.com/rust-lang/rust/issues/{38281,51647}
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
    if let Some(message) = info.message() {
        println!(": {}", message);
    } else {
        println!("");
    }


    loop {}
}

// linker symbols
extern "C" {
    static __text_start: u32;
    static __text_end: u32;
    static __exidx_start: u32;
    static __exidx_end: u32;
}

#[no_mangle]
extern fn dl_unwind_find_exidx(_pc: *const u32, len_ptr: *mut u32) -> *const u32 {
    let length;
    let start: *const u32;
    unsafe {
        length = (&__exidx_end as *const u32).offset_from(&__exidx_start) as u32;
        start = &__exidx_start;
        *len_ptr = length;
    }
    start
}
