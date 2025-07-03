use libboard_artiq::{drtio_routing, drtioaux, drtioaux_async,
                     drtioaux_proto::{MASTER_PAYLOAD_MAX_SIZE, SAT_PAYLOAD_MAX_SIZE},
                     logger,
                     pl::csr};
use libboard_zynq::{i2c::{Error as I2cError, I2c},
                    slcr, timer};

#[cfg(has_cxp_grabber)]
use crate::drtiosat_cxp;
use crate::{analyzer::Analyzer, dma::Manager as DmaManager, drtiosat_reset, mgmt, mgmt::Manager as CoreManager,
            repeater, routing::Router, subkernel::Manager as KernelManager};

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
    ) => {{
        let hop = $routing_table.0[$destination as usize][$rank as usize];
        if hop != 0 {
            let repno = (hop - 1) as usize;
            if repno < $repeaters.len() {
                if $packet.expects_response() {
                    return $repeaters[repno]
                        .aux_forward($packet, $router, $routing_table, $rank, $self_destination)
                        .await;
                } else {
                    return $repeaters[repno].aux_send($packet).await;
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
    ) => {};
}

async fn process_aux_packet<'a, 'b>(
    _repeaters: &mut [repeater::Repeater],
    _routing_table: &mut drtio_routing::RoutingTable,
    rank: &mut u8,
    self_destination: &mut u8,
    packet: drtioaux::Packet,
    i2c: &mut I2c,
    dma_manager: &mut DmaManager,
    analyzer: &mut Analyzer,
    kernel_manager: &mut KernelManager<'a>,
    core_manager: &mut CoreManager<'b>,
    router: &mut Router,
) -> Result<(), drtioaux::Error> {
    // In the code below, *_chan_sel_write takes an u8 if there are fewer than 256 channels,
    // and u16 otherwise; hence the `as _` conversion.
    match packet {
        drtioaux::Packet::EchoRequest => drtioaux_async::send(0, &drtioaux::Packet::EchoReply).await,
        drtioaux::Packet::ResetRequest => {
            info!("resetting RTIO");
            drtiosat_reset(true);
            timer::delay_us(100);
            drtiosat_reset(false);
            for rep in _repeaters.iter() {
                if let Err(e) = rep.rtio_reset().await {
                    error!("failed to issue RTIO reset ({:?})", e);
                }
            }
            drtioaux_async::send(0, &drtioaux::Packet::ResetAck).await
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
                    drtioaux_async::send(0, &drtioaux::Packet::DestinationSequenceErrorReply { channel }).await?;
                } else if errors & 2 != 0 {
                    let channel;
                    unsafe {
                        channel = csr::drtiosat::collision_channel_read();
                        csr::drtiosat::rtio_error_write(2);
                    }
                    drtioaux_async::send(0, &drtioaux::Packet::DestinationCollisionReply { channel }).await?;
                } else if errors & 4 != 0 {
                    let channel;
                    unsafe {
                        channel = csr::drtiosat::busy_channel_read();
                        csr::drtiosat::rtio_error_write(4);
                    }
                    drtioaux_async::send(0, &drtioaux::Packet::DestinationBusyReply { channel }).await?;
                } else {
                    drtioaux_async::send(0, &drtioaux::Packet::DestinationOkReply).await?;
                }
            }

            #[cfg(has_drtio_routing)]
            {
                if hop != 0 {
                    let hop = hop as usize;
                    if hop <= csr::DRTIOREP.len() {
                        let repno = hop - 1;
                        match _repeaters[repno]
                            .aux_forward(
                                &drtioaux::Packet::DestinationStatusRequest {
                                    destination: destination,
                                },
                                router,
                                _routing_table,
                                *rank,
                                *self_destination,
                            )
                            .await
                        {
                            Ok(()) => (),
                            Err(drtioaux::Error::LinkDown) => {
                                drtioaux_async::send(0, &drtioaux::Packet::DestinationDownReply).await?
                            }
                            Err(e) => {
                                drtioaux_async::send(0, &drtioaux::Packet::DestinationDownReply).await?;
                                error!("aux error when handling destination status request: {:?}", e);
                            }
                        }
                    } else {
                        drtioaux_async::send(0, &drtioaux::Packet::DestinationDownReply).await?;
                    }
                }
            }

            Ok(())
        }

        #[cfg(has_drtio_routing)]
        drtioaux::Packet::RoutingSetPath { destination, hops } => {
            _routing_table.0[destination as usize] = hops;
            for rep in _repeaters.iter() {
                if let Err(e) = rep.set_path(destination, &hops).await {
                    error!("failed to set path ({:?})", e);
                }
            }
            drtioaux_async::send(0, &drtioaux::Packet::RoutingAck).await
        }
        #[cfg(has_drtio_routing)]
        drtioaux::Packet::RoutingSetRank { rank: new_rank } => {
            *rank = new_rank;
            drtio_routing::interconnect_enable_all(_routing_table, new_rank);

            let rep_rank = new_rank + 1;
            for rep in _repeaters.iter() {
                if let Err(e) = rep.set_rank(rep_rank).await {
                    error!("failed to set rank ({:?})", e);
                }
            }

            info!("rank: {}", rank);
            info!("routing table: {}", _routing_table);

            drtioaux_async::send(0, &drtioaux::Packet::RoutingAck).await
        }

        #[cfg(not(has_drtio_routing))]
        drtioaux::Packet::RoutingSetPath {
            destination: _,
            hops: _,
        } => drtioaux_async::send(0, &drtioaux::Packet::RoutingAck).await,
        #[cfg(not(has_drtio_routing))]
        drtioaux::Packet::RoutingSetRank { rank: _ } => drtioaux_async::send(0, &drtioaux::Packet::RoutingAck).await,

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
            drtioaux_async::send(0, &reply).await
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
            drtioaux_async::send(0, &drtioaux::Packet::InjectionStatusReply { value: value }).await
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
            );
            let succeeded = i2c.start().is_ok();
            drtioaux_async::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded }).await
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
            );
            let succeeded = i2c.restart().is_ok();
            drtioaux_async::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded }).await
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
            );
            let succeeded = i2c.stop().is_ok();
            drtioaux_async::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded }).await
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
            );
            match i2c.write(data) {
                Ok(()) => {
                    drtioaux_async::send(
                        0,
                        &drtioaux::Packet::I2cWriteReply {
                            succeeded: true,
                            ack: true,
                        },
                    )
                    .await
                }
                Err(I2cError::Nack) => {
                    drtioaux_async::send(
                        0,
                        &drtioaux::Packet::I2cWriteReply {
                            succeeded: true,
                            ack: false,
                        },
                    )
                    .await
                }
                Err(_) => {
                    drtioaux_async::send(
                        0,
                        &drtioaux::Packet::I2cWriteReply {
                            succeeded: false,
                            ack: false,
                        },
                    )
                    .await
                }
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
            );
            match i2c.read(ack) {
                Ok(data) => {
                    drtioaux_async::send(
                        0,
                        &drtioaux::Packet::I2cReadReply {
                            succeeded: true,
                            data: data,
                        },
                    )
                    .await
                }
                Err(_) => {
                    drtioaux_async::send(
                        0,
                        &drtioaux::Packet::I2cReadReply {
                            succeeded: false,
                            data: 0xff,
                        },
                    )
                    .await
                }
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
                _ => return drtioaux_async::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: false }).await,
            };
            let succeeded = i2c.pca954x_select(address, ch).is_ok();
            drtioaux_async::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded }).await
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
            );
            // todo: reimplement when/if SPI is available
            //let succeeded = spi::set_config(busno, flags, length, div, cs).is_ok();
            drtioaux_async::send(0, &drtioaux::Packet::SpiBasicReply { succeeded: false }).await
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
            );
            // todo: reimplement when/if SPI is available
            //let succeeded = spi::write(busno, data).is_ok();
            drtioaux_async::send(0, &drtioaux::Packet::SpiBasicReply { succeeded: false }).await
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
            );
            // todo: reimplement when/if SPI is available
            // match spi::read(busno) {
            //     Ok(data) => drtioaux_async::send(0,
            //         &drtioaux::Packet::SpiReadReply { succeeded: true, data: data }).await,
            //     Err(_) => drtioaux_async::send(0,
            //         &drtioaux::Packet::SpiReadReply { succeeded: false, data: 0 }).await
            // }
            drtioaux_async::send(
                0,
                &drtioaux::Packet::SpiReadReply {
                    succeeded: false,
                    data: 0,
                },
            )
            .await
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
            );
            let header = analyzer.get_header();
            drtioaux_async::send(
                0,
                &drtioaux::Packet::AnalyzerHeader {
                    total_byte_count: header.total_byte_count,
                    sent_bytes: header.sent_bytes,
                    overflow_occurred: header.error,
                },
            )
            .await
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
            );
            let mut data_slice: [u8; SAT_PAYLOAD_MAX_SIZE] = [0; SAT_PAYLOAD_MAX_SIZE];
            let meta = analyzer.get_data(&mut data_slice);
            drtioaux_async::send(
                0,
                &drtioaux::Packet::AnalyzerData {
                    last: meta.last,
                    length: meta.len,
                    data: data_slice,
                },
            )
            .await
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
            );
            *self_destination = destination;
            let succeeded = dma_manager.add(source, id, status, &trace, length as usize).is_ok();
            router
                .send(
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
                .await
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
            );
            let succeeded = dma_manager.erase(source, id).is_ok();
            router
                .send(
                    drtioaux::Packet::DmaRemoveTraceReply {
                        destination: source,
                        succeeded: succeeded,
                    },
                    _routing_table,
                    *rank,
                    *self_destination,
                )
                .await
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
            );
            let succeeded = if !kernel_manager.running() {
                dma_manager.playback(source, id, timestamp).is_ok()
            } else {
                false
            };
            router
                .send(
                    drtioaux::Packet::DmaPlaybackReply {
                        destination: source,
                        succeeded: succeeded,
                    },
                    _routing_table,
                    *rank,
                    *self_destination,
                )
                .await
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
            );
            if !succeeded {
                kernel_manager.ddma_nack().await;
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
            );
            dma_manager
                .remote_finished(kernel_manager, id, error, channel, timestamp)
                .await;
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
            );
            *self_destination = destination;
            let succeeded = kernel_manager.add(id, status, &data, length as usize).is_ok();
            drtioaux_async::send(0, &drtioaux::Packet::SubkernelAddDataReply { succeeded: succeeded }).await
        }
        drtioaux::Packet::SubkernelLoadRunRequest {
            source,
            destination: _destination,
            id,
            run,
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
            );
            let mut succeeded = kernel_manager.load(id).await.is_ok();
            // allow preloading a kernel with delayed run
            if run {
                if dma_manager.running() {
                    // cannot run kernel while DDMA is running
                    succeeded = false;
                } else {
                    succeeded |= kernel_manager.run(source, id, timestamp).await.is_ok();
                }
            }
            router
                .send(
                    drtioaux::Packet::SubkernelLoadRunReply {
                        destination: source,
                        succeeded: succeeded,
                    },
                    _routing_table,
                    *rank,
                    *self_destination,
                )
                .await
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
            );
            kernel_manager.remote_subkernel_finished(id, with_exception, exception_src);
            Ok(())
        }
        drtioaux::Packet::SubkernelExceptionRequest {
            source,
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
            );
            let mut data_slice: [u8; MASTER_PAYLOAD_MAX_SIZE] = [0; MASTER_PAYLOAD_MAX_SIZE];
            let meta = kernel_manager.exception_get_slice(&mut data_slice);
            router
                .send(
                    drtioaux::Packet::SubkernelException {
                        destination: source,
                        last: meta.status.is_last(),
                        length: meta.len,
                        data: data_slice,
                    },
                    _routing_table,
                    *rank,
                    *self_destination,
                )
                .await
        }
        drtioaux::Packet::SubkernelException {
            destination: _destination,
            last,
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
            );
            kernel_manager.received_exception(
                &data[..length as usize],
                last,
                router,
                _routing_table,
                *rank,
                *self_destination,
            );
            Ok(())
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
            );
            kernel_manager.message_handle_incoming(status, id, length as usize, &data);
            router
                .send(
                    drtioaux::Packet::SubkernelMessageAck { destination: source },
                    _routing_table,
                    *rank,
                    *self_destination,
                )
                .await
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
        drtioaux::Packet::CoreMgmtGetLogRequest {
            destination: _destination,
            clear,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
            );
            let mut data_slice = [0; SAT_PAYLOAD_MAX_SIZE];
            let meta = core_manager.log_get_slice(&mut data_slice, clear);
            drtioaux_async::send(
                0,
                &drtioaux::Packet::CoreMgmtGetLogReply {
                    last: meta.status.is_last(),
                    length: meta.len as u16,
                    data: data_slice,
                },
            )
            .await
        }
        drtioaux::Packet::CoreMgmtClearLogRequest {
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
            );
            mgmt::clear_log();
            drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded: true }).await
        }
        drtioaux::Packet::CoreMgmtSetLogLevelRequest {
            destination: _destination,
            log_level,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
            );

            if let Ok(level_filter) = mgmt::byte_to_level_filter(log_level) {
                info!("Changing log level to {}", level_filter);
                log::set_max_level(level_filter);
                drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded: true }).await
            } else {
                drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded: false }).await
            }
        }
        drtioaux::Packet::CoreMgmtSetUartLogLevelRequest {
            destination: _destination,
            log_level,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
            );

            if let Ok(level_filter) = mgmt::byte_to_level_filter(log_level) {
                info!("Changing UART log level to {}", level_filter);
                logger::BufferLogger::get_logger().set_uart_log_level(level_filter);
                drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded: true }).await
            } else {
                drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded: false }).await
            }
        }
        drtioaux::Packet::CoreMgmtConfigReadRequest {
            destination: _destination,
            length,
            key,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
            );

            let mut value_slice = [0; SAT_PAYLOAD_MAX_SIZE];

            let key_slice = &key[..length as usize];
            if !key_slice.is_ascii() {
                error!("invalid key");
                drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded: false }).await
            } else {
                let key = core::str::from_utf8(key_slice).unwrap();
                if core_manager.fetch_config_value(key).is_ok() {
                    let meta = core_manager.get_config_value_slice(&mut value_slice);
                    drtioaux_async::send(
                        0,
                        &drtioaux::Packet::CoreMgmtConfigReadReply {
                            last: meta.status.is_last(),
                            length: meta.len as u16,
                            value: value_slice,
                        },
                    )
                    .await
                } else {
                    drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded: false }).await
                }
            }
        }
        drtioaux::Packet::CoreMgmtConfigReadContinue {
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
            );

            let mut value_slice = [0; SAT_PAYLOAD_MAX_SIZE];
            let meta = core_manager.get_config_value_slice(&mut value_slice);
            drtioaux_async::send(
                0,
                &drtioaux::Packet::CoreMgmtConfigReadReply {
                    last: meta.status.is_last(),
                    length: meta.len as u16,
                    value: value_slice,
                },
            )
            .await
        }
        drtioaux::Packet::CoreMgmtConfigWriteRequest {
            destination: _destination,
            last,
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
            );

            core_manager.add_config_data(&data, length as usize);

            let mut succeeded = true;
            if last {
                succeeded = core_manager.write_config().is_ok();
                core_manager.clear_config_data();
            }

            drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded }).await
        }
        drtioaux::Packet::CoreMgmtConfigRemoveRequest {
            destination: _destination,
            length,
            key,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
            );

            let key_slice = &key[..length as usize];
            if !key_slice.is_ascii() {
                error!("invalid key");
                drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded: false }).await
            } else {
                let key = core::str::from_utf8(key_slice).unwrap();
                let succeeded = core_manager.remove_config(key).is_ok();
                drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded }).await
            }
        }
        drtioaux::Packet::CoreMgmtConfigEraseRequest {
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
            );

            error!("config erase not supported on zynq device");
            drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded: false }).await
        }
        drtioaux::Packet::CoreMgmtRebootRequest {
            destination: _destination,
        } => {
            info!("received reboot request");
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
            );

            drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded: true }).await?;
            info!("reboot imminent");
            slcr::reboot();

            unreachable!();
        }
        drtioaux::Packet::CoreMgmtAllocatorDebugRequest {
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
            );

            error!("debug allocator not supported on zynq device");
            drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded: false }).await
        }
        drtioaux::Packet::CoreMgmtFlashRequest {
            destination: _destination,
            payload_length,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
            );

            core_manager.allocate_image_buffer(payload_length as usize);
            drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded: true }).await
        }
        drtioaux::Packet::CoreMgmtFlashAddDataRequest {
            destination: _destination,
            last,
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
            );

            core_manager.add_image_data(&data, length as usize);

            if last {
                drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtDropLink).await
            } else {
                drtioaux_async::send(0, &drtioaux::Packet::CoreMgmtReply { succeeded: true }).await
            }
        }
        drtioaux::Packet::CoreMgmtDropLinkAck {
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
            );

            unsafe {
                csr::gt_drtio::txenable_write(0);
            }

            #[cfg(has_drtio_eem)]
            unsafe {
                csr::eem_transceiver::txenable_write(0);
            }

            core_manager.write_image();
            info!("reboot imminent");
            slcr::reboot();
            Ok(())
        }
        drtioaux::Packet::CXPReadRequest {
            destination: _destination,
            address: _address,
            length: _length,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
            );
            #[cfg(has_cxp_grabber)]
            drtiosat_cxp::process_read_request(_address, _length).await?;
            Ok(())
        }
        #[cfg(has_cxp_grabber)]
        drtioaux::Packet::CXPWrite32Request {
            destination: _destination,
            address: _address,
            value: _value,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
            );
            #[cfg(has_cxp_grabber)]
            drtiosat_cxp::process_write32_request(_address, _value).await?;
            Ok(())
        }
        drtioaux::Packet::CXPROIViewerSetupRequest {
            destination: _destination,
            x0: _x0,
            y0: _y0,
            x1: _x1,
            y1: _y1,
        } => {
            forward!(
                router,
                _routing_table,
                _destination,
                *rank,
                *self_destination,
                _repeaters,
                &packet,
            );
            #[cfg(has_cxp_grabber)]
            drtiosat_cxp::process_roi_viewer_setup_request(_x0, _y0, _x1, _y1).await?;
            Ok(())
        }
        drtioaux::Packet::CXPROIViewerDataRequest {
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
            );
            #[cfg(has_cxp_grabber)]
            drtiosat_cxp::process_roi_viewer_data_request().await?;
            Ok(())
        }

        p => {
            warn!("received unexpected aux packet: {:?}", p);
            Ok(())
        }
    }
}

pub async fn process_aux_packets<'a, 'b>(
    repeaters: &mut [repeater::Repeater],
    routing_table: &mut drtio_routing::RoutingTable,
    rank: &mut u8,
    self_destination: &mut u8,
    i2c: &mut I2c,
    dma_manager: &mut DmaManager,
    analyzer: &mut Analyzer,
    kernel_manager: &mut KernelManager<'a>,
    core_manager: &mut CoreManager<'b>,
    router: &mut Router,
) {
    let result = match drtioaux::recv(0) {
        Ok(packet) => {
            if let Some(packet) = packet.or_else(|| router.get_local_packet()) {
                process_aux_packet(
                    repeaters,
                    routing_table,
                    rank,
                    self_destination,
                    packet,
                    i2c,
                    dma_manager,
                    analyzer,
                    kernel_manager,
                    core_manager,
                    router,
                )
                .await
            } else {
                Ok(())
            }
        }
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        warn!("aux packet error ({:?})", e);
    }
}
