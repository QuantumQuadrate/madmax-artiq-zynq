use alloc::rc::Rc;
use core::cell::RefCell;

use libboard_artiq::{drtio_routing, drtio_routing::RoutingTable, pl::csr};
use libboard_zynq::timer::GlobalTimer;
use libconfig::Config;
use libcortex_a9::mutex::Mutex;
use log::{info, warn};

#[cfg(has_drtio)]
pub mod drtio {
    use alloc::vec::Vec;
    use core::fmt;

    use embedded_hal::blocking::delay::DelayMs;
    use ksupport::{resolve_channel_name, ASYNC_ERROR_BUSY, ASYNC_ERROR_COLLISION, ASYNC_ERROR_SEQUENCE_ERROR,
                   SEEN_ASYNC_ERRORS};
    use libasync::{delay, task};
    use libboard_artiq::{drtioaux::Error as DrtioError,
                         drtioaux_async,
                         drtioaux_async::Packet,
                         drtioaux_proto::{PayloadStatus, MASTER_PAYLOAD_MAX_SIZE}};
    use libboard_zynq::time::Milliseconds;
    use log::{error, info, warn};

    use super::*;
    use crate::{analyzer::remote_analyzer::RemoteBuffer, rtio_dma::remote_dma, subkernel};

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    pub enum Error {
        Timeout,
        AuxError,
        LinkDown,
        UnexpectedReply,
        DmaAddTraceFail(u8),
        DmaEraseFail(u8),
        DmaPlaybackFail(u8),
        SubkernelAddFail(u8),
        SubkernelRunFail(u8),
    }

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            match self {
                Error::Timeout => write!(f, "timed out"),
                Error::AuxError => write!(f, "aux packet error"),
                Error::LinkDown => write!(f, "link down"),
                Error::UnexpectedReply => write!(f, "unexpected reply"),
                Error::DmaAddTraceFail(dest) => write!(f, "error adding DMA trace on satellite #{}", dest),
                Error::DmaEraseFail(dest) => write!(f, "error erasing DMA trace on satellite #{}", dest),
                Error::DmaPlaybackFail(dest) => write!(f, "error playing back DMA trace on satellite #{}", dest),
                Error::SubkernelAddFail(dest) => write!(f, "error adding subkernel on satellite #{}", dest),
                Error::SubkernelRunFail(dest) => write!(f, "error on subkernel run request on satellite #{}", dest),
            }
        }
    }

    impl From<DrtioError> for Error {
        fn from(_error: DrtioError) -> Self {
            Error::AuxError
        }
    }

    pub fn startup(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &Rc<RefCell<RoutingTable>>,
        up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
        timer: GlobalTimer,
    ) {
        let aux_mutex = aux_mutex.clone();
        let routing_table = routing_table.clone();
        let up_destinations = up_destinations.clone();
        task::spawn(async move {
            let routing_table = routing_table.borrow();
            link_task(&aux_mutex, &routing_table, &up_destinations, timer).await;
        });
    }

    async fn link_rx_up(linkno: u8) -> bool {
        let linkno = linkno as usize;
        unsafe { (csr::DRTIO[linkno].rx_up_read)() == 1 }
    }

    async fn process_async_packets(linkno: u8, routing_table: &RoutingTable, packet: Packet) -> Option<Packet> {
        match packet {
            Packet::DmaPlaybackStatus {
                id,
                source,
                destination: 0,
                error,
                channel,
                timestamp,
            } => {
                remote_dma::playback_done(id, source, error, channel, timestamp).await;
                None
            }
            Packet::SubkernelFinished {
                id,
                destination: 0,
                with_exception,
                exception_src,
            } => {
                subkernel::subkernel_finished(id, with_exception, exception_src).await;
                None
            }
            Packet::SubkernelMessage {
                id,
                source,
                destination: 0,
                status,
                length,
                data,
            } => {
                subkernel::message_handle_incoming(id, status, length as usize, &data).await;
                // acknowledge receiving part of the message
                drtioaux_async::send(linkno, &Packet::SubkernelMessageAck { destination: source })
                    .await
                    .unwrap();
                None
            }
            // routable packets
            Packet::DmaAddTraceRequest { destination, .. }
            | Packet::DmaAddTraceReply { destination, .. }
            | Packet::DmaRemoveTraceRequest { destination, .. }
            | Packet::DmaRemoveTraceReply { destination, .. }
            | Packet::DmaPlaybackRequest { destination, .. }
            | Packet::DmaPlaybackReply { destination, .. }
            | Packet::SubkernelLoadRunRequest { destination, .. }
            | Packet::SubkernelLoadRunReply { destination, .. }
            | Packet::SubkernelMessage { destination, .. }
            | Packet::SubkernelMessageAck { destination, .. }
            | Packet::DmaPlaybackStatus { destination, .. }
            | Packet::SubkernelFinished { destination, .. } => {
                if destination == 0 {
                    Some(packet)
                } else {
                    let dest_link = routing_table.0[destination as usize][0] - 1;
                    if dest_link == linkno {
                        warn!(
                            "[LINK#{}] Re-routed packet would return to the same link, dropping: {:?}",
                            linkno, packet
                        );
                    } else {
                        drtioaux_async::send(dest_link, &packet).await.unwrap();
                    }
                    None
                }
            }
            other => Some(other),
        }
    }

    async fn recv_aux_timeout(linkno: u8, timeout: u64, timer: GlobalTimer) -> Result<Packet, Error> {
        if !link_rx_up(linkno).await {
            return Err(Error::LinkDown);
        }
        match drtioaux_async::recv_timeout(linkno, Some(timeout), timer).await {
            Ok(packet) => return Ok(packet),
            Err(DrtioError::TimedOut) => return Err(Error::Timeout),
            Err(_) => return Err(Error::AuxError),
        }
    }

    pub async fn aux_transact(
        aux_mutex: &Mutex<bool>,
        linkno: u8,
        routing_table: &RoutingTable,
        request: &Packet,
        timer: GlobalTimer,
    ) -> Result<Packet, Error> {
        if !link_rx_up(linkno).await {
            return Err(Error::LinkDown);
        }
        let _lock = aux_mutex.async_lock().await;
        drtioaux_async::send(linkno, request).await.unwrap();
        loop {
            let packet = recv_aux_timeout(linkno, 200, timer).await?;
            if let Some(packet) = process_async_packets(linkno, routing_table, packet).await {
                return Ok(packet);
            }
        }
    }

    async fn drain_buffer(linkno: u8, draining_time: Milliseconds, timer: GlobalTimer) {
        let max_time = timer.get_time() + draining_time;
        loop {
            if timer.get_time() > max_time {
                return;
            }
            let _ = drtioaux_async::recv(linkno).await;
        }
    }

    async fn ping_remote(
        aux_mutex: &Rc<Mutex<bool>>,
        linkno: u8,
        routing_table: &RoutingTable,
        timer: GlobalTimer,
    ) -> u32 {
        let mut count = 0;
        loop {
            if !link_rx_up(linkno).await {
                return 0;
            }
            count += 1;
            if count > 100 {
                return 0;
            }
            let reply = aux_transact(aux_mutex, linkno, routing_table, &Packet::EchoRequest, timer).await;
            match reply {
                Ok(Packet::EchoReply) => {
                    // make sure receive buffer is drained
                    let draining_time = Milliseconds(200);
                    drain_buffer(linkno, draining_time, timer).await;
                    return count;
                }
                _ => {}
            }
        }
    }

    async fn sync_tsc(aux_mutex: &Rc<Mutex<bool>>, linkno: u8, timer: GlobalTimer) -> Result<(), Error> {
        let _lock = aux_mutex.async_lock().await;

        unsafe {
            (csr::DRTIO[linkno as usize].set_time_write)(1);
            while (csr::DRTIO[linkno as usize].set_time_read)() == 1 {}
        }
        // TSCAck is the only aux packet that is sent spontaneously
        // by the satellite, in response to a TSC set on the RT link.
        let reply = recv_aux_timeout(linkno, 10000, timer).await?;
        if reply == Packet::TSCAck {
            Ok(())
        } else {
            Err(Error::UnexpectedReply)
        }
    }

    async fn load_routing_table(
        aux_mutex: &Rc<Mutex<bool>>,
        linkno: u8,
        routing_table: &RoutingTable,
        timer: GlobalTimer,
    ) -> Result<(), Error> {
        for i in 0..drtio_routing::DEST_COUNT {
            let reply = aux_transact(
                aux_mutex,
                linkno,
                routing_table,
                &Packet::RoutingSetPath {
                    destination: i as u8,
                    hops: routing_table.0[i],
                },
                timer,
            )
            .await?;
            if reply != Packet::RoutingAck {
                return Err(Error::UnexpectedReply);
            }
        }
        Ok(())
    }

    async fn set_rank(
        aux_mutex: &Rc<Mutex<bool>>,
        linkno: u8,
        rank: u8,
        routing_table: &RoutingTable,
        timer: GlobalTimer,
    ) -> Result<(), Error> {
        let reply = aux_transact(
            aux_mutex,
            linkno,
            routing_table,
            &Packet::RoutingSetRank { rank: rank },
            timer,
        )
        .await?;
        match reply {
            Packet::RoutingAck => Ok(()),
            _ => Err(Error::UnexpectedReply),
        }
    }

    async fn init_buffer_space(destination: u8, linkno: u8) {
        let linkno = linkno as usize;
        unsafe {
            (csr::DRTIO[linkno].destination_write)(destination);
            (csr::DRTIO[linkno].force_destination_write)(1);
            (csr::DRTIO[linkno].o_get_buffer_space_write)(1);
            while (csr::DRTIO[linkno].o_wait_read)() == 1 {}
            info!(
                "[DEST#{}] buffer space is {}",
                destination,
                (csr::DRTIO[linkno].o_dbg_buffer_space_read)()
            );
            (csr::DRTIO[linkno].force_destination_write)(0);
        }
    }

    async fn process_unsolicited_aux(aux_mutex: &Mutex<bool>, linkno: u8, routing_table: &RoutingTable) {
        let _lock = aux_mutex.async_lock().await;
        match drtioaux_async::recv(linkno).await {
            Ok(Some(packet)) => {
                if let Some(packet) = process_async_packets(linkno, routing_table, packet).await {
                    warn!("[LINK#{}] unsolicited aux packet: {:?}", linkno, packet);
                }
            }
            Ok(None) => (),
            Err(_) => warn!("[LINK#{}] aux packet error", linkno),
        }
    }

    async fn process_local_errors(linkno: u8) {
        let errors;
        let linkidx = linkno as usize;
        unsafe {
            errors = (csr::DRTIO[linkidx].protocol_error_read)();
            (csr::DRTIO[linkidx].protocol_error_write)(errors);
        }
        if errors != 0 {
            error!("[LINK#{}] error(s) found (0x{:02x}):", linkno, errors);
            if errors & 1 != 0 {
                error!("[LINK#{}] received packet of an unknown type", linkno);
            }
            if errors & 2 != 0 {
                error!("[LINK#{}] received truncated packet", linkno);
            }
            if errors & 4 != 0 {
                error!("[LINK#{}] timeout attempting to get remote buffer space", linkno);
            }
        }
    }

    async fn destination_set_up(
        routing_table: &RoutingTable,
        up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
        destination: u8,
        up: bool,
    ) {
        let mut up_destinations = up_destinations.borrow_mut();
        up_destinations[destination as usize] = up;
        if up {
            drtio_routing::interconnect_enable(routing_table, 0, destination);
            info!("[DEST#{}] destination is up", destination);
        } else {
            drtio_routing::interconnect_disable(destination);
            info!("[DEST#{}] destination is down", destination);
        }
    }

    async fn destination_up(up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>, destination: u8) -> bool {
        let up_destinations = up_destinations.borrow();
        up_destinations[destination as usize]
    }

    async fn destination_survey(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &RoutingTable,
        up_links: &[bool],
        up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
        timer: GlobalTimer,
    ) {
        for destination in 0..drtio_routing::DEST_COUNT {
            let hop = routing_table.0[destination][0];
            let destination = destination as u8;

            if hop == 0 {
                /* local RTIO */
                if !destination_up(up_destinations, destination).await {
                    destination_set_up(routing_table, up_destinations, destination, true).await;
                }
            } else if hop as usize <= csr::DRTIO.len() {
                let linkno = hop - 1;
                if destination_up(up_destinations, destination).await {
                    if up_links[linkno as usize] {
                        let reply = aux_transact(
                            aux_mutex,
                            linkno,
                            routing_table,
                            &Packet::DestinationStatusRequest {
                                destination: destination,
                            },
                            timer,
                        )
                        .await;
                        match reply {
                            Ok(Packet::DestinationDownReply) => {
                                destination_set_up(routing_table, up_destinations, destination, false).await;
                                remote_dma::destination_changed(aux_mutex, routing_table, timer, destination, false)
                                    .await;
                                subkernel::destination_changed(aux_mutex, routing_table, timer, destination, false)
                                    .await;
                            }
                            Ok(Packet::DestinationOkReply) => (),
                            Ok(Packet::DestinationSequenceErrorReply { channel }) => {
                                error!(
                                    "[DEST#{}] RTIO sequence error involving channel 0x{:04x}:{}",
                                    destination,
                                    channel,
                                    resolve_channel_name(channel as u32)
                                );
                                unsafe { SEEN_ASYNC_ERRORS |= ASYNC_ERROR_SEQUENCE_ERROR };
                            }
                            Ok(Packet::DestinationCollisionReply { channel }) => {
                                error!(
                                    "[DEST#{}] RTIO collision involving channel 0x{:04x}:{}",
                                    destination,
                                    channel,
                                    resolve_channel_name(channel as u32)
                                );
                                unsafe { SEEN_ASYNC_ERRORS |= ASYNC_ERROR_COLLISION };
                            }
                            Ok(Packet::DestinationBusyReply { channel }) => {
                                error!(
                                    "[DEST#{}] RTIO busy error involving channel 0x{:04x}:{}",
                                    destination,
                                    channel,
                                    resolve_channel_name(channel as u32)
                                );
                                unsafe { SEEN_ASYNC_ERRORS |= ASYNC_ERROR_BUSY };
                            }
                            Ok(packet) => error!("[DEST#{}] received unexpected aux packet: {:?}", destination, packet),
                            Err(e) => error!("[DEST#{}] communication failed ({})", destination, e),
                        }
                    } else {
                        destination_set_up(routing_table, up_destinations, destination, false).await;
                        remote_dma::destination_changed(aux_mutex, routing_table, timer, destination, false).await;
                        subkernel::destination_changed(aux_mutex, routing_table, timer, destination, false).await;
                    }
                } else {
                    if up_links[linkno as usize] {
                        let reply = aux_transact(
                            aux_mutex,
                            linkno,
                            routing_table,
                            &Packet::DestinationStatusRequest {
                                destination: destination,
                            },
                            timer,
                        )
                        .await;
                        match reply {
                            Ok(Packet::DestinationDownReply) => (),
                            Ok(Packet::DestinationOkReply) => {
                                destination_set_up(routing_table, up_destinations, destination, true).await;
                                init_buffer_space(destination as u8, linkno).await;
                                remote_dma::destination_changed(aux_mutex, routing_table, timer, destination, true)
                                    .await;
                                subkernel::destination_changed(aux_mutex, routing_table, timer, destination, true)
                                    .await;
                            }
                            Ok(packet) => error!("[DEST#{}] received unexpected aux packet: {:?}", destination, packet),
                            Err(e) => error!("[DEST#{}] communication failed ({})", destination, e),
                        }
                    }
                }
            }
        }
    }

    pub async fn link_task(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &RoutingTable,
        up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
        timer: GlobalTimer,
    ) {
        let mut up_links = [false; csr::DRTIO.len()];
        loop {
            for linkno in 0..csr::DRTIO.len() {
                let linkno = linkno as u8;
                if up_links[linkno as usize] {
                    /* link was previously up */
                    if link_rx_up(linkno).await {
                        process_unsolicited_aux(aux_mutex, linkno, routing_table).await;
                        process_local_errors(linkno).await;
                    } else {
                        info!("[LINK#{}] link is down", linkno);
                        up_links[linkno as usize] = false;
                    }
                } else {
                    /* link was previously down */
                    if link_rx_up(linkno).await {
                        info!("[LINK#{}] link RX became up, pinging", linkno);
                        let ping_count = ping_remote(aux_mutex, linkno, routing_table, timer).await;
                        if ping_count > 0 {
                            info!("[LINK#{}] remote replied after {} packets", linkno, ping_count);
                            up_links[linkno as usize] = true;
                            if let Err(e) = sync_tsc(aux_mutex, linkno, timer).await {
                                error!("[LINK#{}] failed to sync TSC ({})", linkno, e);
                            }
                            if let Err(e) = load_routing_table(aux_mutex, linkno, routing_table, timer).await {
                                error!("[LINK#{}] failed to load routing table ({})", linkno, e);
                            }
                            if let Err(e) = set_rank(aux_mutex, linkno, 1 as u8, routing_table, timer).await {
                                error!("[LINK#{}] failed to set rank ({})", linkno, e);
                            }
                            info!("[LINK#{}] link initialization completed", linkno);
                        } else {
                            error!("[LINK#{}] ping failed", linkno);
                        }
                    }
                }
            }
            destination_survey(aux_mutex, routing_table, &up_links, up_destinations, timer).await;
            let mut countdown = timer.countdown();
            delay(&mut countdown, Milliseconds(200)).await;
        }
    }

    #[allow(dead_code)]
    pub fn reset(aux_mutex: Rc<Mutex<bool>>, routing_table: &RoutingTable, mut timer: GlobalTimer) {
        for linkno in 0..csr::DRTIO.len() {
            unsafe {
                (csr::DRTIO[linkno].reset_write)(1);
            }
        }
        timer.delay_ms(1);
        for linkno in 0..csr::DRTIO.len() {
            unsafe {
                (csr::DRTIO[linkno].reset_write)(0);
            }
        }

        for linkno in 0..csr::DRTIO.len() {
            let linkno = linkno as u8;
            if task::block_on(link_rx_up(linkno)) {
                let reply = task::block_on(aux_transact(
                    &aux_mutex,
                    linkno,
                    routing_table,
                    &Packet::ResetRequest,
                    timer,
                ));
                match reply {
                    Ok(Packet::ResetAck) => (),
                    Ok(_) => error!("[LINK#{}] reset failed, received unexpected aux packet", linkno),
                    Err(e) => error!("[LINK#{}] reset failed, aux packet error ({})", linkno, e),
                }
            }
        }
    }

    async fn partition_data<PacketF, HandlerF>(
        linkno: u8,
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &RoutingTable,
        timer: GlobalTimer,
        data: &[u8],
        packet_f: PacketF,
        reply_handler_f: HandlerF,
    ) -> Result<(), Error>
    where
        PacketF: Fn(&[u8; MASTER_PAYLOAD_MAX_SIZE], PayloadStatus, usize) -> Packet,
        HandlerF: Fn(&Packet) -> Result<(), Error>,
    {
        let mut i = 0;
        while i < data.len() {
            let mut slice: [u8; MASTER_PAYLOAD_MAX_SIZE] = [0; MASTER_PAYLOAD_MAX_SIZE];
            let len: usize = if i + MASTER_PAYLOAD_MAX_SIZE < data.len() {
                MASTER_PAYLOAD_MAX_SIZE
            } else {
                data.len() - i
            } as usize;
            let first = i == 0;
            let last = i + len == data.len();
            slice[..len].clone_from_slice(&data[i..i + len]);
            i += len;
            let status = PayloadStatus::from_status(first, last);
            let packet = packet_f(&slice, status, len);
            let reply = aux_transact(aux_mutex, linkno, routing_table, &packet, timer).await?;
            reply_handler_f(&reply)?;
        }
        Ok(())
    }

    pub async fn ddma_upload_trace(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &RoutingTable,
        timer: GlobalTimer,
        id: u32,
        destination: u8,
        trace: &Vec<u8>,
    ) -> Result<(), Error> {
        let linkno = routing_table.0[destination as usize][0] - 1;
        partition_data(
            linkno,
            aux_mutex,
            routing_table,
            timer,
            trace,
            |slice, status, len| Packet::DmaAddTraceRequest {
                id: id,
                source: 0,
                destination: destination,
                status: status,
                length: len as u16,
                trace: *slice,
            },
            |reply| match reply {
                Packet::DmaAddTraceReply {
                    destination: 0,
                    succeeded: true,
                    ..
                } => Ok(()),
                Packet::DmaAddTraceReply {
                    destination: 0,
                    succeeded: false,
                    ..
                } => Err(Error::DmaAddTraceFail(destination)),
                _ => Err(Error::UnexpectedReply),
            },
        )
        .await
    }

    pub async fn ddma_send_erase(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &RoutingTable,
        timer: GlobalTimer,
        id: u32,
        destination: u8,
    ) -> Result<(), Error> {
        let linkno = routing_table.0[destination as usize][0] - 1;
        let reply = aux_transact(
            aux_mutex,
            linkno,
            routing_table,
            &Packet::DmaRemoveTraceRequest {
                id: id,
                source: 0,
                destination: destination,
            },
            timer,
        )
        .await?;
        match reply {
            Packet::DmaRemoveTraceReply {
                destination: 0,
                succeeded: true,
            } => Ok(()),
            Packet::DmaRemoveTraceReply {
                destination: 0,
                succeeded: false,
            } => Err(Error::DmaEraseFail(destination)),
            _ => Err(Error::UnexpectedReply),
        }
    }

    pub async fn ddma_send_playback(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &RoutingTable,
        timer: GlobalTimer,
        id: u32,
        destination: u8,
        timestamp: u64,
    ) -> Result<(), Error> {
        let linkno = routing_table.0[destination as usize][0] - 1;
        let reply = aux_transact(
            aux_mutex,
            linkno,
            routing_table,
            &Packet::DmaPlaybackRequest {
                id: id,
                source: 0,
                destination: destination,
                timestamp: timestamp,
            },
            timer,
        )
        .await?;
        match reply {
            Packet::DmaPlaybackReply {
                destination: 0,
                succeeded: true,
            } => Ok(()),
            Packet::DmaPlaybackReply {
                destination: 0,
                succeeded: false,
            } => Err(Error::DmaPlaybackFail(destination)),
            _ => Err(Error::UnexpectedReply),
        }
    }

    async fn analyzer_get_data(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &RoutingTable,
        timer: GlobalTimer,
        destination: u8,
    ) -> Result<RemoteBuffer, Error> {
        let linkno = routing_table.0[destination as usize][0] - 1;
        let reply = aux_transact(
            aux_mutex,
            linkno,
            routing_table,
            &Packet::AnalyzerHeaderRequest {
                destination: destination,
            },
            timer,
        )
        .await?;
        let (sent, total, overflow) = match reply {
            Packet::AnalyzerHeader {
                sent_bytes,
                total_byte_count,
                overflow_occurred,
            } => (sent_bytes, total_byte_count, overflow_occurred),
            _ => return Err(Error::UnexpectedReply),
        };

        let mut remote_data: Vec<u8> = Vec::new();
        if sent > 0 {
            let mut last_packet = false;
            while !last_packet {
                let reply = aux_transact(
                    aux_mutex,
                    linkno,
                    routing_table,
                    &Packet::AnalyzerDataRequest {
                        destination: destination,
                    },
                    timer,
                )
                .await?;
                match reply {
                    Packet::AnalyzerData { last, length, data } => {
                        last_packet = last;
                        remote_data.extend(&data[0..length as usize]);
                    }
                    _ => return Err(Error::UnexpectedReply),
                }
            }
        }

        Ok(RemoteBuffer {
            sent_bytes: sent,
            total_byte_count: total,
            error: overflow,
            data: remote_data,
        })
    }

    pub async fn analyzer_query(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &RoutingTable,
        up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
        timer: GlobalTimer,
    ) -> Result<Vec<RemoteBuffer>, Error> {
        let mut remote_buffers: Vec<RemoteBuffer> = Vec::new();
        for i in 1..drtio_routing::DEST_COUNT {
            if destination_up(up_destinations, i as u8).await {
                remote_buffers.push(analyzer_get_data(aux_mutex, routing_table, timer, i as u8).await?);
            }
        }
        Ok(remote_buffers)
    }

    pub async fn subkernel_upload(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &RoutingTable,
        timer: GlobalTimer,
        id: u32,
        destination: u8,
        data: &Vec<u8>,
    ) -> Result<(), Error> {
        let linkno = routing_table.0[destination as usize][0] - 1;
        partition_data(
            linkno,
            aux_mutex,
            routing_table,
            timer,
            data,
            |slice, status, len| Packet::SubkernelAddDataRequest {
                id: id,
                destination: destination,
                status: status,
                length: len as u16,
                data: *slice,
            },
            |reply| match reply {
                Packet::SubkernelAddDataReply { succeeded: true } => Ok(()),
                Packet::SubkernelAddDataReply { succeeded: false } => Err(Error::SubkernelAddFail(destination)),
                _ => Err(Error::UnexpectedReply),
            },
        )
        .await
    }

    pub async fn subkernel_load(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &RoutingTable,
        timer: GlobalTimer,
        id: u32,
        destination: u8,
        run: bool,
    ) -> Result<(), Error> {
        let linkno = routing_table.0[destination as usize][0] - 1;
        let reply = aux_transact(
            aux_mutex,
            linkno,
            routing_table,
            &Packet::SubkernelLoadRunRequest {
                id: id,
                source: 0,
                destination: destination,
                run: run,
            },
            timer,
        )
        .await?;
        match reply {
            Packet::SubkernelLoadRunReply {
                destination: 0,
                succeeded: true,
            } => return Ok(()),
            Packet::SubkernelLoadRunReply {
                destination: 0,
                succeeded: false,
            } => return Err(Error::SubkernelRunFail(destination)),
            _ => Err(Error::UnexpectedReply),
        }
    }

    pub async fn subkernel_retrieve_exception(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &RoutingTable,
        timer: GlobalTimer,
        destination: u8,
    ) -> Result<Vec<u8>, Error> {
        let linkno = routing_table.0[destination as usize][0] - 1;
        let mut remote_data: Vec<u8> = Vec::new();
        loop {
            let reply = aux_transact(
                aux_mutex,
                linkno,
                routing_table,
                &Packet::SubkernelExceptionRequest {
                    destination: destination,
                },
                timer,
            )
            .await?;
            match reply {
                Packet::SubkernelException { last, length, data } => {
                    remote_data.extend(&data[0..length as usize]);
                    if last {
                        return Ok(remote_data);
                    }
                }
                _ => return Err(Error::UnexpectedReply),
            }
        }
    }

    pub async fn subkernel_send_message(
        aux_mutex: &Rc<Mutex<bool>>,
        routing_table: &RoutingTable,
        timer: GlobalTimer,
        id: u32,
        destination: u8,
        message: &[u8],
    ) -> Result<(), Error> {
        let linkno = routing_table.0[destination as usize][0] - 1;
        partition_data(
            linkno,
            aux_mutex,
            routing_table,
            timer,
            message,
            |slice, status, len| Packet::SubkernelMessage {
                source: 0,
                destination: destination,
                id: id,
                status: status,
                length: len as u16,
                data: *slice,
            },
            |reply| match reply {
                Packet::SubkernelMessageAck { .. } => Ok(()),
                _ => Err(Error::UnexpectedReply),
            },
        )
        .await
    }
}

#[cfg(not(has_drtio))]
pub mod drtio {
    use super::*;

    pub fn startup(
        _aux_mutex: &Rc<Mutex<bool>>,
        _routing_table: &Rc<RefCell<RoutingTable>>,
        _up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
        _timer: GlobalTimer,
    ) {
    }

    #[allow(dead_code)]
    pub fn reset(_aux_mutex: Rc<Mutex<bool>>, _routing_table: &RoutingTable, mut _timer: GlobalTimer) {}
}

fn toggle_sed_spread(val: u8) {
    unsafe {
        csr::rtio_core::sed_spread_enable_write(val);
    }
}

fn setup_sed_spread(cfg: &Config) {
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
}

pub fn startup(
    aux_mutex: &Rc<Mutex<bool>>,
    routing_table: &Rc<RefCell<RoutingTable>>,
    up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
    cfg: &Config,
    timer: GlobalTimer,
) {
    setup_sed_spread(cfg);
    drtio::startup(aux_mutex, routing_table, up_destinations, timer);
    unsafe {
        csr::rtio_core::reset_phy_write(1);
    }
}

#[allow(dead_code)]
pub fn reset(aux_mutex: Rc<Mutex<bool>>, routing_table: &RoutingTable, timer: GlobalTimer) {
    unsafe {
        csr::rtio_core::reset_write(1);
    }
    drtio::reset(aux_mutex, routing_table, timer)
}
