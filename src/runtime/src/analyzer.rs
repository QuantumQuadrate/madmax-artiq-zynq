use alloc::rc::Rc;
#[cfg(has_drtio)]
use alloc::vec::Vec;
use core::cell::RefCell;

use libasync::{smoltcp::TcpStream, task};
use libboard_artiq::drtio_routing;
use libboard_zynq::smoltcp::Error;
use libcortex_a9::cache;
use log::{debug, info, warn};

use crate::{pl, proto_async::*};

const BUFFER_SIZE: usize = 512 * 1024;

#[repr(align(64))]
struct Buffer {
    data: [u8; BUFFER_SIZE],
}

static BUFFER: Buffer = Buffer { data: [0; BUFFER_SIZE] };

fn arm() {
    debug!("arming RTIO analyzer");
    unsafe {
        let base_addr = (&raw const BUFFER.data[0]).addr();
        let last_addr = (&raw const BUFFER.data[BUFFER_SIZE - 1]).addr();
        pl::csr::rtio_analyzer::message_encoder_overflow_reset_write(1);
        pl::csr::rtio_analyzer::dma_base_address_write(base_addr as u32);
        pl::csr::rtio_analyzer::dma_last_address_write(last_addr as u32);
        pl::csr::rtio_analyzer::dma_reset_write(1);
        pl::csr::rtio_analyzer::enable_write(1);
    }
}

fn disarm() {
    debug!("disarming RTIO analyzer");
    unsafe {
        pl::csr::rtio_analyzer::enable_write(0);
        while pl::csr::rtio_analyzer::busy_read() != 0 {}
        cache::dcci_slice(&BUFFER.data);
    }
    debug!("RTIO analyzer disarmed");
}

#[cfg(has_drtio)]
pub mod remote_analyzer {
    use super::*;
    use crate::rtio_mgt::drtio;

    pub struct RemoteBuffer {
        pub total_byte_count: u64,
        pub sent_bytes: u32,
        pub error: bool,
        pub data: Vec<u8>,
    }

    pub async fn get_data(
        up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
    ) -> Result<RemoteBuffer, drtio::Error> {
        // gets data from satellites and returns consolidated data
        let mut remote_data: Vec<u8> = Vec::new();
        let mut remote_error = false;
        let mut remote_sent_bytes = 0;
        let mut remote_total_bytes = 0;

        let data_vec = match drtio::analyzer_query(up_destinations).await {
            Ok(data_vec) => data_vec,
            Err(e) => return Err(e),
        };
        for data in data_vec {
            remote_total_bytes += data.total_byte_count;
            remote_sent_bytes += data.sent_bytes;
            remote_error |= data.error;
            remote_data.extend(data.data);
        }

        Ok(RemoteBuffer {
            total_byte_count: remote_total_bytes,
            sent_bytes: remote_sent_bytes,
            error: remote_error,
            data: remote_data,
        })
    }
}

#[derive(Debug)]
struct Header {
    sent_bytes: u32,
    total_byte_count: u64,
    error_occurred: bool,
    log_channel: u8,
    dds_onehot_sel: bool,
}

async fn write_header(stream: &mut TcpStream, header: &Header) -> Result<(), Error> {
    stream.send_slice("e".as_bytes()).await?;
    write_i32(stream, header.sent_bytes as i32).await?;
    write_i64(stream, header.total_byte_count as i64).await?;
    write_i8(stream, header.error_occurred as i8).await?;
    write_i8(stream, header.log_channel as i8).await?;
    write_i8(stream, header.dds_onehot_sel as i8).await?;
    Ok(())
}

async fn handle_connection(
    stream: &mut TcpStream,
    _up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
) -> Result<(), Error> {
    info!("received connection");

    let data = &BUFFER.data[..];
    let overflow_occurred = unsafe { pl::csr::rtio_analyzer::message_encoder_overflow_read() != 0 };
    let bus_error_occurred = unsafe { pl::csr::rtio_analyzer::dma_bus_error_read() != 0 };
    let total_byte_count = unsafe { pl::csr::rtio_analyzer::dma_byte_count_read() as u64 };
    let pointer = (total_byte_count % BUFFER_SIZE as u64) as usize;
    let wraparound = total_byte_count >= BUFFER_SIZE as u64;
    let sent_bytes = if wraparound {
        BUFFER_SIZE as u32
    } else {
        total_byte_count as u32
    };

    if overflow_occurred {
        warn!("overflow occured");
    }
    if bus_error_occurred {
        warn!("bus error occured");
    }

    #[cfg(has_drtio)]
    let remote = remote_analyzer::get_data(_up_destinations).await;
    #[cfg(has_drtio)]
    let (header, remote_data) = match remote {
        Ok(remote) => (
            Header {
                total_byte_count: total_byte_count + remote.total_byte_count,
                sent_bytes: sent_bytes + remote.sent_bytes,
                error_occurred: overflow_occurred | bus_error_occurred | remote.error,
                log_channel: pl::csr::CONFIG_RTIO_LOG_CHANNEL as u8,
                dds_onehot_sel: true,
            },
            remote.data,
        ),
        Err(e) => {
            warn!("Error getting remote analyzer data: {}", e);
            (
                Header {
                    total_byte_count: total_byte_count,
                    sent_bytes: sent_bytes,
                    error_occurred: true,
                    log_channel: pl::csr::CONFIG_RTIO_LOG_CHANNEL as u8,
                    dds_onehot_sel: true,
                },
                Vec::new(),
            )
        }
    };

    #[cfg(not(has_drtio))]
    let header = Header {
        total_byte_count: total_byte_count,
        sent_bytes: sent_bytes,
        error_occurred: overflow_occurred | bus_error_occurred,
        log_channel: pl::csr::CONFIG_RTIO_LOG_CHANNEL as u8,
        dds_onehot_sel: true, // kept for backward compatibility of analyzer dumps
    };
    debug!("{:?}", header);

    write_header(stream, &header).await?;
    if wraparound {
        stream.send(data[pointer..].iter().copied()).await?;
        stream.send(data[..pointer].iter().copied()).await?;
    } else {
        stream.send(data[..pointer].iter().copied()).await?;
    }
    #[cfg(has_drtio)]
    stream.send(remote_data.iter().copied()).await?;

    Ok(())
}

pub fn start(up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>) {
    let up_destinations = up_destinations.clone();
    task::spawn(async move {
        loop {
            arm();
            let mut stream = TcpStream::accept(1382, 2048, 2048).await.unwrap();
            disarm();
            let _ = handle_connection(&mut stream, &up_destinations)
                .await
                .map_err(|e| warn!("connection terminated: {:?}", e));
            let _ = stream.flush().await;
            let _ = stream.close().await;
        }
    });
}
