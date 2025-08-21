use alloc::rc::Rc;
use core::cell::RefCell;

use libboard_artiq::{drtio_routing, pl::csr};
use libconfig;
use log::{info, warn};

#[cfg(has_drtio)]
pub mod drtio {
    use alloc::vec::Vec;
    use core::fmt;

    use ksupport::{ASYNC_ERROR_BUSY, ASYNC_ERROR_COLLISION, ASYNC_ERROR_SEQUENCE_ERROR, SEEN_ASYNC_ERRORS,
                   kernel::Message as KernelMessage};
    use libasync::task;
    #[cfg(has_drtio_eem)]
    use libboard_artiq::drtio_eem;
    use libboard_artiq::{drtioaux::Error as DrtioError,
                         drtioaux_async,
                         drtioaux_async::Packet,
                         drtioaux_proto::{MASTER_PAYLOAD_MAX_SIZE, PayloadStatus},
                         resolve_channel_name};
    use libboard_zynq::timer;
    use libcortex_a9::mutex::Mutex;
    use log::{error, info, warn};

    use super::*;
    use crate::{analyzer::remote_analyzer::RemoteBuffer, comms::ROUTING_TABLE, rtio_dma::remote_dma, subkernel};

    #[cfg(has_drtio_eem)]
    const DRTIO_EEM_LINKNOS: core::ops::Range<usize> =
        (csr::DRTIO.len() - csr::CONFIG_EEM_DRTIO_COUNT as usize)..csr::DRTIO.len();

    pub static AUX_MUTEX: Mutex<bool> = Mutex::new(false);

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

    pub fn startup(up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>) {
        let up_destinations = up_destinations.clone();
        task::spawn(async move {
            link_task(&up_destinations).await;
        });
    }

    async fn link_rx_up(linkno: u8) -> bool {
        let linkno = linkno as usize;
        #[cfg(has_drtio_eem)]
        if DRTIO_EEM_LINKNOS.contains(&linkno) {
            let eem_trx_no = linkno - DRTIO_EEM_LINKNOS.start;
            unsafe {
                csr::eem_transceiver::transceiver_sel_write(eem_trx_no as u8);
                csr::eem_transceiver::comma_align_reset_write(1);
            }
            timer::delay_us(100);
            return unsafe { csr::eem_transceiver::comma_read() == 1 };
        }
        unsafe { (csr::DRTIO[linkno].rx_up_read)() == 1 }
    }

    fn get_master_destination() -> u8 {
        for i in 0..drtio_routing::DEST_COUNT {
            if ROUTING_TABLE.get().unwrap().0[i][0] == 0 {
                return i as u8;
            }
        }
        error!("Master is not defined in the routing table");
        0
    }

    async fn route_packet(linkno: u8, packet: Packet, destination: u8) {
        let dest_link = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
        if dest_link == linkno {
            warn!(
                "[LINK#{}] Re-routed packet would return to the same link, dropping: {:?}",
                linkno, packet
            );
        } else {
            drtioaux_async::send(dest_link, &packet).await.unwrap();
        }
    }

    async fn process_async_packets(linkno: u8, packet: Packet) -> Option<Packet> {
        let master_destination = get_master_destination();
        match packet {
            Packet::DmaPlaybackStatus {
                id,
                source,
                destination,
                error,
                channel,
                timestamp,
            } => {
                if destination == master_destination {
                    remote_dma::playback_done(id, source, error, channel, timestamp).await;
                } else {
                    route_packet(linkno, packet, destination).await;
                }
                None
            }
            Packet::SubkernelFinished {
                id,
                destination,
                with_exception,
                exception_src,
            } => {
                if destination == master_destination {
                    subkernel::subkernel_finished(id, with_exception, exception_src).await;
                } else {
                    route_packet(linkno, packet, destination).await;
                }
                None
            }
            Packet::SubkernelMessage {
                id,
                source,
                destination,
                status,
                length,
                data,
            } => {
                if destination == master_destination {
                    subkernel::message_handle_incoming(id, status, length as usize, &data).await;
                    // acknowledge receiving part of the message
                    drtioaux_async::send(linkno, &Packet::SubkernelMessageAck { destination: source })
                        .await
                        .unwrap();
                } else {
                    route_packet(linkno, packet, destination).await;
                }
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
            | Packet::SubkernelMessageAck { destination, .. }
            | Packet::SubkernelException { destination, .. }
            | Packet::SubkernelExceptionRequest { destination, .. } => {
                if destination == master_destination {
                    Some(packet)
                } else {
                    route_packet(linkno, packet, destination).await;
                    None
                }
            }
            other => Some(other),
        }
    }

    async fn recv_aux_timeout(linkno: u8, timeout: u64) -> Result<Packet, Error> {
        if !link_rx_up(linkno).await {
            return Err(Error::LinkDown);
        }
        match drtioaux_async::recv_timeout(linkno, Some(timeout)).await {
            Ok(packet) => return Ok(packet),
            Err(DrtioError::TimedOut) => return Err(Error::Timeout),
            Err(_) => return Err(Error::AuxError),
        }
    }

    pub async fn aux_transact(linkno: u8, request: &Packet) -> Result<Packet, Error> {
        if !link_rx_up(linkno).await {
            return Err(Error::LinkDown);
        }
        let _lock = AUX_MUTEX.async_lock().await;
        drtioaux_async::send(linkno, request).await.unwrap();
        loop {
            let packet = recv_aux_timeout(linkno, 200).await?;
            if let Some(packet) = process_async_packets(linkno, packet).await {
                return Ok(packet);
            }
        }
    }

    async fn drain_buffer(linkno: u8, draining_time: u64) {
        let max_time = timer::get_ms() + draining_time;
        while timer::get_ms() < max_time {
            let _ = drtioaux_async::recv(linkno).await;
        }
    }

    async fn ping_remote(linkno: u8) -> u32 {
        let mut count = 0;
        loop {
            if !link_rx_up(linkno).await {
                return 0;
            }
            count += 1;
            if count > 100 {
                return 0;
            }
            let reply = aux_transact(linkno, &Packet::EchoRequest).await;
            match reply {
                Ok(Packet::EchoReply) => {
                    // make sure receive buffer is drained
                    drain_buffer(linkno, 200).await;
                    return count;
                }
                _ => {}
            }
        }
    }

    async fn sync_tsc(linkno: u8) -> Result<(), Error> {
        let _lock = AUX_MUTEX.async_lock().await;

        unsafe {
            (csr::DRTIO[linkno as usize].set_time_write)(1);
            while (csr::DRTIO[linkno as usize].set_time_read)() == 1 {}
        }
        // TSCAck is the only aux packet that is sent spontaneously
        // by the satellite, in response to a TSC set on the RT link.
        let reply = recv_aux_timeout(linkno, 10000).await?;
        if reply == Packet::TSCAck {
            Ok(())
        } else {
            Err(Error::UnexpectedReply)
        }
    }

    async fn load_routing_table(linkno: u8) -> Result<(), Error> {
        for i in 0..drtio_routing::DEST_COUNT {
            let reply = aux_transact(
                linkno,
                &Packet::RoutingSetPath {
                    destination: i as u8,
                    hops: ROUTING_TABLE.get().unwrap().0[i],
                },
            )
            .await?;
            if reply != Packet::RoutingAck {
                return Err(Error::UnexpectedReply);
            }
        }
        Ok(())
    }

    async fn set_rank(linkno: u8, rank: u8) -> Result<(), Error> {
        let reply = aux_transact(linkno, &Packet::RoutingSetRank { rank: rank }).await?;
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

    async fn process_unsolicited_aux(linkno: u8) {
        let _lock = AUX_MUTEX.async_lock().await;
        match drtioaux_async::recv(linkno).await {
            Ok(Some(packet)) => {
                if let Some(packet) = process_async_packets(linkno, packet).await {
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
        up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
        destination: u8,
        up: bool,
    ) {
        let mut up_destinations = up_destinations.borrow_mut();
        up_destinations[destination as usize] = up;
        if up {
            drtio_routing::interconnect_enable(ROUTING_TABLE.get().unwrap(), 0, destination);
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

    async fn destination_survey(up_links: &[bool], up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>) {
        for destination in 0..drtio_routing::DEST_COUNT {
            let hop = ROUTING_TABLE.get().unwrap().0[destination][0];
            let destination = destination as u8;

            if hop > 0 && hop as usize <= csr::DRTIO.len() {
                let linkno = hop - 1;
                if destination_up(up_destinations, destination).await {
                    if up_links[linkno as usize] {
                        let reply = aux_transact(
                            linkno,
                            &Packet::DestinationStatusRequest {
                                destination: destination,
                            },
                        )
                        .await;
                        match reply {
                            Ok(Packet::DestinationDownReply) => {
                                destination_set_up(up_destinations, destination, false).await;
                                remote_dma::destination_changed(destination, false).await;
                                subkernel::destination_changed(destination, false).await;
                            }
                            Ok(Packet::DestinationOkReply) => (),
                            Ok(Packet::DestinationSequenceErrorReply { channel }) => {
                                let global_ch = ((destination as u32) << 16) | channel as u32;
                                error!(
                                    "[DEST#{}] RTIO sequence error involving channel 0x{:04x}:{}",
                                    destination,
                                    channel,
                                    resolve_channel_name(global_ch)
                                );
                                unsafe { SEEN_ASYNC_ERRORS |= ASYNC_ERROR_SEQUENCE_ERROR };
                            }
                            Ok(Packet::DestinationCollisionReply { channel }) => {
                                let global_ch = ((destination as u32) << 16) | channel as u32;
                                error!(
                                    "[DEST#{}] RTIO collision involving channel 0x{:04x}:{}",
                                    destination,
                                    channel,
                                    resolve_channel_name(global_ch)
                                );
                                unsafe { SEEN_ASYNC_ERRORS |= ASYNC_ERROR_COLLISION };
                            }
                            Ok(Packet::DestinationBusyReply { channel }) => {
                                let global_ch = ((destination as u32) << 16) | channel as u32;
                                error!(
                                    "[DEST#{}] RTIO busy error involving channel 0x{:04x}:{}",
                                    destination,
                                    channel,
                                    resolve_channel_name(global_ch)
                                );
                                unsafe { SEEN_ASYNC_ERRORS |= ASYNC_ERROR_BUSY };
                            }
                            Ok(packet) => error!("[DEST#{}] received unexpected aux packet: {:?}", destination, packet),
                            Err(e) => error!("[DEST#{}] communication failed ({})", destination, e),
                        }
                    } else {
                        destination_set_up(up_destinations, destination, false).await;
                        remote_dma::destination_changed(destination, false).await;
                        subkernel::destination_changed(destination, false).await;
                    }
                } else {
                    if up_links[linkno as usize] {
                        let reply = aux_transact(
                            linkno,
                            &Packet::DestinationStatusRequest {
                                destination: destination,
                            },
                        )
                        .await;
                        match reply {
                            Ok(Packet::DestinationDownReply) => (),
                            Ok(Packet::DestinationOkReply) => {
                                destination_set_up(up_destinations, destination, true).await;
                                init_buffer_space(destination as u8, linkno).await;
                                remote_dma::destination_changed(destination, true).await;
                                subkernel::destination_changed(destination, true).await;
                            }
                            Ok(packet) => error!("[DEST#{}] received unexpected aux packet: {:?}", destination, packet),
                            Err(e) => error!("[DEST#{}] communication failed ({})", destination, e),
                        }
                    }
                }
            }
        }
    }

    pub async fn link_task(up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>) {
        let mut up_links = [false; csr::DRTIO.len()];
        // set up local RTIO
        let master_destination = get_master_destination();

        destination_set_up(up_destinations, master_destination, true).await;
        loop {
            for linkno in 0..csr::DRTIO.len() {
                let linkno = linkno as u8;
                if up_links[linkno as usize] {
                    /* link was previously up */
                    if link_rx_up(linkno).await {
                        process_unsolicited_aux(linkno).await;
                        process_local_errors(linkno).await;
                    } else {
                        info!("[LINK#{}] link is down", linkno);
                        up_links[linkno as usize] = false;

                        #[cfg(has_drtio_eem)]
                        if DRTIO_EEM_LINKNOS.contains(&(linkno as usize)) {
                            unsafe {
                                csr::eem_transceiver::rx_ready_write(0);
                            }
                            while !matches!(drtioaux_async::recv(linkno).await, Ok(None)) {}
                        }
                    }
                } else {
                    /* link was previously down */
                    #[cfg(has_drtio_eem)]
                    if DRTIO_EEM_LINKNOS.contains(&(linkno as usize)) {
                        let eem_trx_no = linkno - DRTIO_EEM_LINKNOS.start as u8;
                        if !unsafe { drtio_eem::align_wordslip(eem_trx_no) } {
                            continue;
                        }
                        unsafe {
                            csr::eem_transceiver::rx_ready_write(1);
                        }
                    }

                    if link_rx_up(linkno).await {
                        info!("[LINK#{}] link RX became up, pinging", linkno);
                        let ping_count = ping_remote(linkno).await;
                        if ping_count > 0 {
                            info!("[LINK#{}] remote replied after {} packets", linkno, ping_count);
                            up_links[linkno as usize] = true;
                            if let Err(e) = sync_tsc(linkno).await {
                                error!("[LINK#{}] failed to sync TSC ({})", linkno, e);
                            }
                            if let Err(e) = load_routing_table(linkno).await {
                                error!("[LINK#{}] failed to load routing table ({})", linkno, e);
                            }
                            if let Err(e) = set_rank(linkno, 1 as u8).await {
                                error!("[LINK#{}] failed to set rank ({})", linkno, e);
                            }
                            info!("[LINK#{}] link initialization completed", linkno);
                        } else {
                            error!("[LINK#{}] ping failed", linkno);
                        }
                    }
                }
            }
            destination_survey(&up_links, up_destinations).await;
            timer::async_delay_ms(200).await;
        }
    }

    pub async fn reset() {
        for linkno in 0..csr::DRTIO.len() {
            unsafe {
                (csr::DRTIO[linkno].reset_write)(1);
            }
        }
        timer::delay_ms(1);
        for linkno in 0..csr::DRTIO.len() {
            unsafe {
                (csr::DRTIO[linkno].reset_write)(0);
            }
        }

        for linkno in 0..csr::DRTIO.len() {
            let linkno = linkno as u8;
            if link_rx_up(linkno).await {
                let reply = aux_transact(linkno, &Packet::ResetRequest).await;
                match reply {
                    Ok(Packet::ResetAck) => (),
                    Ok(_) => error!("[LINK#{}] reset failed, received unexpected aux packet", linkno),
                    Err(e) => error!("[LINK#{}] reset failed, aux packet error ({})", linkno, e),
                }
            }
        }
    }

    pub async fn partition_data<PacketF, HandlerF>(
        linkno: u8,
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
            let reply = aux_transact(linkno, &packet).await?;
            reply_handler_f(&reply)?;
        }
        Ok(())
    }

    pub async fn ddma_upload_trace(id: u32, destination: u8, trace: &Vec<u8>) -> Result<(), Error> {
        let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
        let master_destination = get_master_destination();
        partition_data(
            linkno,
            trace,
            |slice, status, len| Packet::DmaAddTraceRequest {
                id: id,
                source: master_destination,
                destination,
                status: status,
                length: len as u16,
                trace: *slice,
            },
            |reply| match reply {
                Packet::DmaAddTraceReply {
                    destination,
                    succeeded: true,
                    ..
                } => {
                    if *destination == master_destination {
                        Ok(())
                    } else {
                        Err(Error::UnexpectedReply)
                    }
                }
                Packet::DmaAddTraceReply {
                    destination,
                    succeeded: false,
                    ..
                } => {
                    if *destination == master_destination {
                        Err(Error::DmaAddTraceFail(*destination))
                    } else {
                        Err(Error::UnexpectedReply)
                    }
                }
                _ => Err(Error::UnexpectedReply),
            },
        )
        .await
    }

    pub async fn ddma_send_erase(id: u32, destination: u8) -> Result<(), Error> {
        let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
        let master_destination = get_master_destination();
        let reply = aux_transact(
            linkno,
            &Packet::DmaRemoveTraceRequest {
                id: id,
                source: master_destination,
                destination: destination,
            },
        )
        .await?;
        match reply {
            Packet::DmaRemoveTraceReply {
                destination,
                succeeded: true,
            } => {
                if destination == master_destination {
                    Ok(())
                } else {
                    Err(Error::UnexpectedReply)
                }
            }
            Packet::DmaRemoveTraceReply {
                destination,
                succeeded: false,
            } => {
                if destination == master_destination {
                    Err(Error::DmaEraseFail(destination))
                } else {
                    Err(Error::UnexpectedReply)
                }
            }
            _ => Err(Error::UnexpectedReply),
        }
    }

    pub async fn ddma_send_playback(id: u32, destination: u8, timestamp: u64) -> Result<(), Error> {
        let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
        let master_destination = get_master_destination();
        let reply = aux_transact(
            linkno,
            &Packet::DmaPlaybackRequest {
                id: id,
                source: master_destination,
                destination: destination,
                timestamp: timestamp,
            },
        )
        .await?;
        match reply {
            Packet::DmaPlaybackReply {
                destination,
                succeeded: true,
            } => {
                if destination == master_destination {
                    Ok(())
                } else {
                    Err(Error::UnexpectedReply)
                }
            }
            Packet::DmaPlaybackReply {
                destination,
                succeeded: false,
            } => {
                if destination == master_destination {
                    Err(Error::DmaPlaybackFail(destination))
                } else {
                    Err(Error::UnexpectedReply)
                }
            }
            _ => Err(Error::UnexpectedReply),
        }
    }

    async fn analyzer_get_data(destination: u8) -> Result<RemoteBuffer, Error> {
        let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
        let reply = aux_transact(
            linkno,
            &Packet::AnalyzerHeaderRequest {
                destination: destination,
            },
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
                    linkno,
                    &Packet::AnalyzerDataRequest {
                        destination: destination,
                    },
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
        up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
    ) -> Result<Vec<RemoteBuffer>, Error> {
        let mut remote_buffers: Vec<RemoteBuffer> = Vec::new();
        for i in 1..drtio_routing::DEST_COUNT {
            if destination_up(up_destinations, i as u8).await {
                remote_buffers.push(analyzer_get_data(i as u8).await?);
            }
        }
        Ok(remote_buffers)
    }

    pub async fn subkernel_upload(id: u32, destination: u8, data: &Vec<u8>) -> Result<(), Error> {
        let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
        partition_data(
            linkno,
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

    pub async fn subkernel_load(id: u32, destination: u8, run: bool, timestamp: u64) -> Result<(), Error> {
        let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
        let master_destination = get_master_destination();
        let reply = aux_transact(
            linkno,
            &Packet::SubkernelLoadRunRequest {
                id: id,
                source: master_destination,
                destination: destination,
                run: run,
                timestamp,
            },
        )
        .await?;
        match reply {
            Packet::SubkernelLoadRunReply {
                destination,
                succeeded: true,
            } => {
                if destination == master_destination {
                    Ok(())
                } else {
                    Err(Error::UnexpectedReply)
                }
            }
            Packet::SubkernelLoadRunReply {
                destination,
                succeeded: false,
            } => {
                if destination == master_destination {
                    Err(Error::SubkernelRunFail(destination))
                } else {
                    Err(Error::UnexpectedReply)
                }
            }
            _ => Err(Error::UnexpectedReply),
        }
    }

    pub async fn subkernel_retrieve_exception(destination: u8) -> Result<Vec<u8>, Error> {
        let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
        let mut remote_data: Vec<u8> = Vec::new();
        let master_destination = get_master_destination();
        loop {
            let reply = aux_transact(
                linkno,
                &Packet::SubkernelExceptionRequest {
                    source: master_destination,
                    destination: destination,
                },
            )
            .await?;
            match reply {
                Packet::SubkernelException {
                    destination,
                    last,
                    length,
                    data,
                } => {
                    if destination == master_destination {
                        remote_data.extend(&data[0..length as usize]);
                        if last {
                            return Ok(remote_data);
                        }
                    } else {
                        return Err(Error::UnexpectedReply);
                    }
                }
                _ => return Err(Error::UnexpectedReply),
            }
        }
    }

    pub async fn subkernel_send_message(id: u32, destination: u8, message: &[u8]) -> Result<(), Error> {
        let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
        let master_destination = get_master_destination();
        partition_data(
            linkno,
            message,
            |slice, status, len| Packet::SubkernelMessage {
                source: master_destination,
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

    pub async fn i2c_send_basic(request: &KernelMessage, busno: u32) -> Result<bool, Error> {
        let destination = (busno >> 16) as u8;
        let busno = busno as u8;
        let packet = match request {
            KernelMessage::I2cStartRequest(_) => Packet::I2cStartRequest { destination, busno },
            KernelMessage::I2cRestartRequest(_) => Packet::I2cRestartRequest { destination, busno },
            KernelMessage::I2cStopRequest(_) => Packet::I2cStopRequest { destination, busno },
            KernelMessage::I2cSwitchSelectRequest { address, mask, .. } => Packet::I2cSwitchSelectRequest {
                destination,
                busno,
                address: *address,
                mask: *mask,
            },
            _ => unreachable!(),
        };
        let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
        let reply = aux_transact(linkno, &packet).await?;
        match reply {
            Packet::I2cBasicReply { succeeded } => Ok(succeeded),
            _ => Err(Error::UnexpectedReply),
        }
    }

    pub async fn i2c_send_write(busno: u32, data: u8) -> Result<(bool, bool), Error> {
        let destination = (busno >> 16) as u8;
        let busno = busno as u8;
        let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
        let reply = aux_transact(
            linkno,
            &Packet::I2cWriteRequest {
                destination,
                busno,
                data,
            },
        )
        .await?;
        match reply {
            Packet::I2cWriteReply { succeeded, ack } => Ok((succeeded, ack)),
            _ => Err(Error::UnexpectedReply),
        }
    }

    pub async fn i2c_send_read(busno: u32, ack: bool) -> Result<(bool, u8), Error> {
        let destination = (busno >> 16) as u8;
        let busno = busno as u8;
        let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
        let reply = aux_transact(
            linkno,
            &Packet::I2cReadRequest {
                destination,
                busno,
                ack,
            },
        )
        .await?;
        match reply {
            Packet::I2cReadReply { succeeded, data } => Ok((succeeded, data)),
            _ => Err(Error::UnexpectedReply),
        }
    }
}

#[cfg(not(has_drtio))]
pub mod drtio {
    use super::*;

    pub fn startup(_up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>) {}

    #[allow(dead_code)]
    pub fn reset() {}
}

fn toggle_sed_spread(val: u8) {
    unsafe {
        csr::rtio_core::sed_spread_enable_write(val);
    }
}

fn setup_sed_spread() {
    if let Ok(spread_enable) = libconfig::read_str("sed_spread_enable") {
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

pub fn startup(up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>) {
    setup_sed_spread();
    drtio::startup(up_destinations);
    unsafe {
        csr::rtio_core::reset_phy_write(1);
    }
}
