#![no_std]
#![no_main]
#![feature(alloc_error_handler, try_trait, never_type, panic_info_message)]

#[macro_use]
extern crate log;
extern crate core_io;
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
use embedded_hal::blocking::delay::DelayUs;
#[cfg(has_grabber)]
use libboard_artiq::grabber;
#[cfg(feature = "target_kasli_soc")]
use libboard_artiq::io_expander;
#[cfg(has_si5324)]
use libboard_artiq::si5324;
#[cfg(has_si549)]
use libboard_artiq::si549;
use libboard_artiq::{drtio_routing, drtioaux,
                     drtioaux_proto::{MASTER_PAYLOAD_MAX_SIZE, SAT_PAYLOAD_MAX_SIZE},
                     identifier_read, logger,
                     pl::csr};
#[cfg(feature = "target_kasli_soc")]
use libboard_zynq::error_led::ErrorLED;
use libboard_zynq::{i2c::I2c, print, println, time::Milliseconds, timer::GlobalTimer};
use libconfig::Config;
use libcortex_a9::{l2c::enable_l2_cache, regs::MPIDR};
use libregister::RegisterR;
use libsupport_zynq::{exception_vectors, ram};
use routing::Router;
use subkernel::Manager as KernelManager;

mod analyzer;
mod dma;
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

#[cfg(has_drtio_routing)]
macro_rules! forward {
    (
        $router:expr,
        $routing_table:expr,
        $destination:expr,
        $rank:expr,
        $self_destination:expr,
        $repeaters:expr,
        $packet:expr,
        $timer:expr
    ) => {{
        let hop = $routing_table.0[$destination as usize][$rank as usize];
        if hop != 0 {
            let repno = (hop - 1) as usize;
            if repno < $repeaters.len() {
                if $packet.expects_response() {
                    return $repeaters[repno].aux_forward(
                        $packet,
                        $router,
                        $routing_table,
                        $rank,
                        $self_destination,
                        $timer,
                    );
                } else {
                    return $repeaters[repno].aux_send($packet);
                }
            } else {
                return Err(drtioaux::Error::RoutingError);
            }
        }
    }};
}

#[cfg(not(has_drtio_routing))]
macro_rules! forward {
    (
        $router:expr,
        $routing_table:expr,
        $destination:expr,
        $rank:expr,
        $self_destination:expr,
        $repeaters:expr,
        $packet:expr,
        $timer:expr
    ) => {};
}

fn process_aux_packet(
    _repeaters: &mut [repeater::Repeater],
    _routing_table: &mut drtio_routing::RoutingTable,
    rank: &mut u8,
    self_destination: &mut u8,
    packet: drtioaux::Packet,
    timer: &mut GlobalTimer,
    i2c: &mut I2c,
    dma_manager: &mut DmaManager,
    analyzer: &mut Analyzer,
    kernel_manager: &mut KernelManager,
    router: &mut Router,
) -> Result<(), drtioaux::Error> {
    // In the code below, *_chan_sel_write takes an u8 if there are fewer than 256 channels,
    // and u16 otherwise; hence the `as _` conversion.
    match packet {
        drtioaux::Packet::EchoRequest => drtioaux::send(0, &drtioaux::Packet::EchoReply),
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
        }

        drtioaux::Packet::DestinationStatusRequest { destination } => {
            #[cfg(has_drtio_routing)]
            let hop = _routing_table.0[destination as usize][*rank as usize];
            #[cfg(not(has_drtio_routing))]
            let hop = 0;

            if hop == 0 {
                *self_destination = destination;
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
                    drtioaux::send(0, &drtioaux::Packet::DestinationSequenceErrorReply { channel })?;
                } else if errors & 2 != 0 {
                    let channel;
                    unsafe {
                        channel = csr::drtiosat::collision_channel_read();
                        csr::drtiosat::rtio_error_write(2);
                    }
                    drtioaux::send(0, &drtioaux::Packet::DestinationCollisionReply { channel })?;
                } else if errors & 4 != 0 {
                    let channel;
                    unsafe {
                        channel = csr::drtiosat::busy_channel_read();
                        csr::drtiosat::rtio_error_write(4);
                    }
                    drtioaux::send(0, &drtioaux::Packet::DestinationBusyReply { channel })?;
                } else {
                    drtioaux::send(0, &drtioaux::Packet::DestinationOkReply)?;
                }
            }

            #[cfg(has_drtio_routing)]
            {
                if hop != 0 {
                    let hop = hop as usize;
                    if hop <= csr::DRTIOREP.len() {
                        let repno = hop - 1;
                        match _repeaters[repno].aux_forward(
                            &drtioaux::Packet::DestinationStatusRequest {
                                destination: destination,
                            },
                            router,
                            _routing_table,
                            *rank,
                            *self_destination,
                            timer,
                        ) {
                            Ok(()) => (),
                            Err(drtioaux::Error::LinkDown) => {
                                drtioaux::send(0, &drtioaux::Packet::DestinationDownReply)?
                            }
                            Err(e) => {
                                drtioaux::send(0, &drtioaux::Packet::DestinationDownReply)?;
                                error!("aux error when handling destination status request: {:?}", e);
                            }
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
        drtioaux::Packet::RoutingSetRank { rank: new_rank } => {
            *rank = new_rank;
            drtio_routing::interconnect_enable_all(_routing_table, new_rank);

            let rep_rank = new_rank + 1;
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
        drtioaux::Packet::RoutingSetPath {
            destination: _,
            hops: _,
        } => drtioaux::send(0, &drtioaux::Packet::RoutingAck),
        #[cfg(not(has_drtio_routing))]
        drtioaux::Packet::RoutingSetRank { rank: _ } => drtioaux::send(0, &drtioaux::Packet::RoutingAck),

        drtioaux::Packet::MonitorRequest {
            destination: _destination,
            channel,
            probe,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
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
        }
        drtioaux::Packet::InjectionRequest {
            destination: _destination,
            channel,
            overrd,
            value,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            #[cfg(has_rtio_moninj)]
            unsafe {
                csr::rtio_moninj::inj_chan_sel_write(channel as _);
                csr::rtio_moninj::inj_override_sel_write(overrd);
                csr::rtio_moninj::inj_value_write(value);
            }
            Ok(())
        }
        drtioaux::Packet::InjectionStatusRequest {
            destination: _destination,
            channel,
            overrd,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
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
        }

        drtioaux::Packet::I2cStartRequest {
            destination: _destination,
            busno: _busno,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            let succeeded = i2c.start().is_ok();
            drtioaux::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded })
        }
        drtioaux::Packet::I2cRestartRequest {
            destination: _destination,
            busno: _busno,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            let succeeded = i2c.restart().is_ok();
            drtioaux::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded })
        }
        drtioaux::Packet::I2cStopRequest {
            destination: _destination,
            busno: _busno,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            let succeeded = i2c.stop().is_ok();
            drtioaux::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded })
        }
        drtioaux::Packet::I2cWriteRequest {
            destination: _destination,
            busno: _busno,
            data,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            match i2c.write(data) {
                Ok(ack) => drtioaux::send(
                    0,
                    &drtioaux::Packet::I2cWriteReply {
                        succeeded: true,
                        ack: ack,
                    },
                ),
                Err(_) => drtioaux::send(
                    0,
                    &drtioaux::Packet::I2cWriteReply {
                        succeeded: false,
                        ack: false,
                    },
                ),
            }
        }
        drtioaux::Packet::I2cReadRequest {
            destination: _destination,
            busno: _busno,
            ack,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            match i2c.read(ack) {
                Ok(data) => drtioaux::send(
                    0,
                    &drtioaux::Packet::I2cReadReply {
                        succeeded: true,
                        data: data,
                    },
                ),
                Err(_) => drtioaux::send(
                    0,
                    &drtioaux::Packet::I2cReadReply {
                        succeeded: false,
                        data: 0xff,
                    },
                ),
            }
        }
        drtioaux::Packet::I2cSwitchSelectRequest {
            destination: _destination,
            busno: _busno,
            address,
            mask,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            let ch = match mask {
                //decode from mainline, PCA9548-centric API
                0x00 => None,
                0x01 => Some(0),
                0x02 => Some(1),
                0x04 => Some(2),
                0x08 => Some(3),
                0x10 => Some(4),
                0x20 => Some(5),
                0x40 => Some(6),
                0x80 => Some(7),
                _ => return drtioaux::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: false }),
            };
            let succeeded = i2c.pca954x_select(address, ch).is_ok();
            drtioaux::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded })
        }

        drtioaux::Packet::SpiSetConfigRequest {
            destination: _destination,
            busno: _busno,
            flags: _flags,
            length: _length,
            div: _div,
            cs: _cs,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            // todo: reimplement when/if SPI is available
            //let succeeded = spi::set_config(busno, flags, length, div, cs).is_ok();
            drtioaux::send(0, &drtioaux::Packet::SpiBasicReply { succeeded: false })
        }
        drtioaux::Packet::SpiWriteRequest {
            destination: _destination,
            busno: _busno,
            data: _data,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            // todo: reimplement when/if SPI is available
            //let succeeded = spi::write(busno, data).is_ok();
            drtioaux::send(0, &drtioaux::Packet::SpiBasicReply { succeeded: false })
        }
        drtioaux::Packet::SpiReadRequest {
            destination: _destination,
            busno: _busno,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            // todo: reimplement when/if SPI is available
            // match spi::read(busno) {
            //     Ok(data) => drtioaux::send(0,
            //         &drtioaux::Packet::SpiReadReply { succeeded: true, data: data }),
            //     Err(_) => drtioaux::send(0,
            //         &drtioaux::Packet::SpiReadReply { succeeded: false, data: 0 })
            // }
            drtioaux::send(
                0,
                &drtioaux::Packet::SpiReadReply {
                    succeeded: false,
                    data: 0,
                },
            )
        }

        drtioaux::Packet::AnalyzerHeaderRequest {
            destination: _destination,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            let header = analyzer.get_header();
            drtioaux::send(
                0,
                &drtioaux::Packet::AnalyzerHeader {
                    total_byte_count: header.total_byte_count,
                    sent_bytes: header.sent_bytes,
                    overflow_occurred: header.error,
                },
            )
        }
        drtioaux::Packet::AnalyzerDataRequest {
            destination: _destination,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            let mut data_slice: [u8; SAT_PAYLOAD_MAX_SIZE] = [0; SAT_PAYLOAD_MAX_SIZE];
            let meta = analyzer.get_data(&mut data_slice);
            drtioaux::send(
                0,
                &drtioaux::Packet::AnalyzerData {
                    last: meta.last,
                    length: meta.len,
                    data: data_slice,
                },
            )
        }

        drtioaux::Packet::DmaAddTraceRequest {
            source,
            destination,
            id,
            status,
            length,
            trace,
        } => {
            forward!(
                router,
                _routing_table,
                destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            *self_destination = destination;
            let succeeded = dma_manager.add(source, id, status, &trace, length as usize).is_ok();
            router.send(
                drtioaux::Packet::DmaAddTraceReply {
                    source: *self_destination,
                    destination: source,
                    id: id,
                    succeeded: succeeded,
                },
                _routing_table,
                *rank,
                *self_destination,
            )
        }
        drtioaux::Packet::DmaAddTraceReply {
            source,
            destination: _destination,
            id,
            succeeded,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            dma_manager.ack_upload(
                kernel_manager,
                source,
                id,
                succeeded,
                router,
                *rank,
                *self_destination,
                _routing_table,
            );
            Ok(())
        }
        drtioaux::Packet::DmaRemoveTraceRequest {
            source,
            destination: _destination,
            id,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            let succeeded = dma_manager.erase(source, id).is_ok();
            router.send(
                drtioaux::Packet::DmaRemoveTraceReply {
                    destination: source,
                    succeeded: succeeded,
                },
                _routing_table,
                *rank,
                *self_destination,
            )
        }
        drtioaux::Packet::DmaRemoveTraceReply {
            destination: _destination,
            succeeded: _,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            Ok(())
        }
        drtioaux::Packet::DmaPlaybackRequest {
            source,
            destination: _destination,
            id,
            timestamp,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            let succeeded = if !kernel_manager.running() {
                dma_manager.playback(source, id, timestamp).is_ok()
            } else {
                false
            };
            router.send(
                drtioaux::Packet::DmaPlaybackReply {
                    destination: source,
                    succeeded: succeeded,
                },
                _routing_table,
                *rank,
                *self_destination,
            )
        }
        drtioaux::Packet::DmaPlaybackReply {
            destination: _destination,
            succeeded,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            if !succeeded {
                kernel_manager.ddma_nack();
            }
            Ok(())
        }
        drtioaux::Packet::DmaPlaybackStatus {
            source: _,
            destination: _destination,
            id,
            error,
            channel,
            timestamp,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            dma_manager.remote_finished(kernel_manager, id, error, channel, timestamp);
            Ok(())
        }

        drtioaux::Packet::SubkernelAddDataRequest {
            destination,
            id,
            status,
            length,
            data,
        } => {
            forward!(
                router,
                _routing_table,
                destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            *self_destination = destination;
            let succeeded = kernel_manager.add(id, status, &data, length as usize).is_ok();
            drtioaux::send(0, &drtioaux::Packet::SubkernelAddDataReply { succeeded: succeeded })
        }
        drtioaux::Packet::SubkernelLoadRunRequest {
            source,
            destination: _destination,
            id,
            run,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            let mut succeeded = kernel_manager.load(id).is_ok();
            // allow preloading a kernel with delayed run
            if run {
                if dma_manager.running() {
                    // cannot run kernel while DDMA is running
                    succeeded = false;
                } else {
                    succeeded |= kernel_manager.run(source, id).is_ok();
                }
            }
            router.send(
                drtioaux::Packet::SubkernelLoadRunReply {
                    destination: source,
                    succeeded: succeeded,
                },
                _routing_table,
                *rank,
                *self_destination,
            )
        }
        drtioaux::Packet::SubkernelLoadRunReply {
            destination: _destination,
            succeeded,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            // received if local subkernel started another, remote subkernel
            kernel_manager.subkernel_load_run_reply(succeeded);
            Ok(())
        }
        drtioaux::Packet::SubkernelFinished {
            destination: _destination,
            id,
            with_exception,
            exception_src,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            kernel_manager.remote_subkernel_finished(id, with_exception, exception_src);
            Ok(())
        }
        drtioaux::Packet::SubkernelExceptionRequest {
            destination: _destination,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            let mut data_slice: [u8; SAT_PAYLOAD_MAX_SIZE] = [0; SAT_PAYLOAD_MAX_SIZE];
            let meta = kernel_manager.exception_get_slice(&mut data_slice);
            drtioaux::send(
                0,
                &drtioaux::Packet::SubkernelException {
                    last: meta.status.is_last(),
                    length: meta.len,
                    data: data_slice,
                },
            )
        }
        drtioaux::Packet::SubkernelMessage {
            source,
            destination: _destination,
            id,
            status,
            length,
            data,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            kernel_manager.message_handle_incoming(status, id, length as usize, &data);
            router.send(
                drtioaux::Packet::SubkernelMessageAck { destination: source },
                _routing_table,
                *rank,
                *self_destination,
            )
        }
        drtioaux::Packet::SubkernelMessageAck {
            destination: _destination,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
                timer
            );
            if kernel_manager.message_ack_slice() {
                let mut data_slice: [u8; MASTER_PAYLOAD_MAX_SIZE] = [0; MASTER_PAYLOAD_MAX_SIZE];
                if let Some(meta) = kernel_manager.message_get_slice(&mut data_slice) {
                    // route and not send immediately as ACKs are not a beginning of a transaction
                    router.route(
                        drtioaux::Packet::SubkernelMessage {
                            source: *self_destination,
                            destination: meta.destination,
                            id: kernel_manager.get_current_id().unwrap(),
                            status: meta.status,
                            length: meta.len as u16,
                            data: data_slice,
                        },
                        _routing_table,
                        *rank,
                        *self_destination,
                    );
                } else {
                    error!("Error receiving message slice");
                }
            }
            Ok(())
        }

        p => {
            warn!("received unexpected aux packet: {:?}", p);
            Ok(())
        }
    }
}

fn process_aux_packets(
    repeaters: &mut [repeater::Repeater],
    routing_table: &mut drtio_routing::RoutingTable,
    rank: &mut u8,
    self_destination: &mut u8,
    timer: &mut GlobalTimer,
    i2c: &mut I2c,
    dma_manager: &mut DmaManager,
    analyzer: &mut Analyzer,
    kernel_manager: &mut KernelManager,
    router: &mut Router,
) {
    let result = drtioaux::recv(0).and_then(|packet| {
        if let Some(packet) = packet.or_else(|| router.get_local_packet()) {
            process_aux_packet(
                repeaters,
                routing_table,
                rank,
                self_destination,
                packet,
                timer,
                i2c,
                dma_manager,
                analyzer,
                kernel_manager,
                router,
            )
        } else {
            Ok(())
        }
    });
    if let Err(e) = result {
        warn!("aux packet error ({:?})", e);
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

    ksupport::i2c::init();
    let mut i2c = unsafe { (ksupport::i2c::I2C_BUS).as_mut().unwrap() };

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

        // Drive CLK_SEL to true
        #[cfg(has_si549)]
        io_expander0.set(1, 7, true);

        // Drive TX_DISABLE to false on SFP0..3
        io_expander0.set(0, 1, false);
        io_expander1.set(0, 1, false);
        io_expander0.set(1, 1, false);
        io_expander1.set(1, 1, false);
        io_expander0.service(&mut i2c).unwrap();
        io_expander1.service(&mut i2c).unwrap();
    }

    #[cfg(has_si5324)]
    si5324::setup(&mut i2c, &SI5324_SETTINGS, si5324::Input::Ckin1, &mut timer).expect("cannot initialize Si5324");
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
    #[cfg(has_si549)]
    si549::helper_setup(&mut timer, &SI549_SETTINGS).expect("cannot initialize helper Si549");

    let cfg = match Config::new() {
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
                io_expander0
                    .service(&mut i2c)
                    .expect("I2C I/O expander #0 service failed");
                io_expander1
                    .service(&mut i2c)
                    .expect("I2C I/O expander #1 service failed");
            }

            hardware_tick(&mut hardware_tick_ts, &mut timer);
        }

        info!("uplink is up, switching to recovered clock");
        #[cfg(has_siphaser)]
        {
            si5324::siphaser::select_recovered_clock(&mut i2c, true, &mut timer).expect("failed to switch clocks");
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
                &mut i2c,
                &mut dma_manager,
                &mut analyzer,
                &mut kernel_manager,
                &mut router,
            );
            #[allow(unused_mut)]
            for mut rep in repeaters.iter_mut() {
                rep.service(&routing_table, rank, destination, &mut router, &mut timer);
            }
            #[cfg(feature = "target_kasli_soc")]
            {
                io_expander0
                    .service(&mut i2c)
                    .expect("I2C I/O expander #0 service failed");
                io_expander1
                    .service(&mut i2c)
                    .expect("I2C I/O expander #1 service failed");
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
        si5324::siphaser::select_recovered_clock(&mut i2c, false, &mut timer).expect("failed to switch clocks");
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
    #[cfg(feature = "target_kasli_soc")]
    {
        let mut err_led = ErrorLED::error_led();
        err_led.toggle(true);
    }

    loop {}
}
