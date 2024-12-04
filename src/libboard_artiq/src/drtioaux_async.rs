use core::slice;

use core_io::{Error as IoError, ErrorKind as IoErrorKind};
use crc;
use io::{proto::{ProtoRead, ProtoWrite},
         Cursor};
use libasync::{block_async, task};
use libboard_zynq::{time::Milliseconds, timer::GlobalTimer};
use nb;
use void::Void;

pub use crate::drtioaux_proto::{Packet, MAX_PACKET};
use crate::{drtioaux::{copy_work_buffer, has_rx_error, Error},
            mem::mem::DRTIOAUX_MEM,
            pl::csr::DRTIOAUX};

pub async fn reset(linkno: u8) {
    let linkno = linkno as usize;
    unsafe {
        // clear buffer first to limit race window with buffer overflow
        // error. We assume the CPU is fast enough so that no two packets
        // will be received between the buffer and the error flag are cleared.
        (DRTIOAUX[linkno].aux_rx_present_write)(1);
        (DRTIOAUX[linkno].aux_rx_error_write)(1);
    }
}

fn tx_ready(linkno: usize) -> nb::Result<(), Void> {
    unsafe {
        if (DRTIOAUX[linkno].aux_tx_read)() != 0 {
            Err(nb::Error::WouldBlock)
        } else {
            Ok(())
        }
    }
}

async fn receive<F, T>(linkno: u8, f: F) -> Result<Option<T>, Error>
where F: FnOnce(&[u8]) -> Result<T, Error> {
    let linkidx = linkno as usize;
    unsafe {
        if (DRTIOAUX[linkidx].aux_rx_present_read)() == 1 {
            let read_ptr = (DRTIOAUX[linkidx].aux_read_pointer_read)() as usize;
            let ptr = (DRTIOAUX_MEM[linkidx].base + DRTIOAUX_MEM[linkidx].size / 2 + read_ptr * 0x400) as *mut u32;
            let result = f(slice::from_raw_parts(ptr as *mut u8, 0x400 as usize));
            (DRTIOAUX[linkidx].aux_rx_present_write)(1);
            Ok(Some(result?))
        } else {
            Ok(None)
        }
    }
}

pub async fn recv(linkno: u8) -> Result<Option<Packet>, Error> {
    if has_rx_error(linkno) {
        return Err(Error::GatewareError);
    }

    receive(linkno, |buffer| {
        if buffer.len() < 8 {
            return Err(IoError::new(IoErrorKind::UnexpectedEof, "Unexpected end").into());
        }

        let mut reader = Cursor::new(buffer);

        let packet = Packet::read_from(&mut reader)?;
        let padding = (12 - (reader.position() % 8)) % 8;
        let checksum_at = reader.position() + padding;
        let checksum = crc::crc32::checksum_ieee(&reader.get_ref()[0..checksum_at]);
        reader.set_position(checksum_at);
        if reader.read_u32()? != checksum {
            return Err(Error::CorruptedPacket);
        }
        Ok(packet)
    })
    .await
}

pub async fn recv_timeout(linkno: u8, timeout_ms: Option<u64>, timer: GlobalTimer) -> Result<Packet, Error> {
    let timeout_ms = Milliseconds(timeout_ms.unwrap_or(10));
    let limit = timer.get_time() + timeout_ms;
    let mut would_block = false;
    while timer.get_time() < limit {
        // to ensure one last time recv would run one last time
        // in case async would return after timeout
        if would_block {
            task::r#yield().await;
        }
        match recv(linkno).await? {
            None => {
                would_block = true;
            }
            Some(packet) => return Ok(packet),
        }
    }
    Err(Error::TimedOut)
}

async fn transmit<F>(linkno: u8, f: F) -> Result<(), Error>
where F: FnOnce(&mut [u8]) -> Result<usize, Error> {
    let linkno = linkno as usize;
    unsafe {
        let _ = block_async!(tx_ready(linkno)).await;
        let ptr = DRTIOAUX_MEM[linkno].base as *mut u32;
        let mut buf: [u8; MAX_PACKET] = [0; MAX_PACKET];
        let len = f(&mut buf)?;
        copy_work_buffer(buf.as_mut_ptr() as *mut u32, ptr, len as isize);
        (DRTIOAUX[linkno].aux_tx_length_write)(len as u16);
        (DRTIOAUX[linkno].aux_tx_write)(1);
        Ok(())
    }
}

pub async fn send(linkno: u8, packet: &Packet) -> Result<(), Error> {
    transmit(linkno, |buffer| {
        let mut writer = Cursor::new(buffer);

        packet.write_to(&mut writer)?;

        // Pad till offset 4, insert checksum there
        let padding = (12 - (writer.position() % 8)) % 8;
        for _ in 0..padding {
            writer.write_u8(0)?;
        }

        let checksum = crc::crc32::checksum_ieee(&writer.get_ref()[0..writer.position()]);
        writer.write_u32(checksum)?;

        Ok(writer.position())
    })
    .await
}
