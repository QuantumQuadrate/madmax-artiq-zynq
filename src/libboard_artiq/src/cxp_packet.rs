use core::slice;

use byteorder::{ByteOrder, NetworkEndian};
use io::Cursor;
use libboard_zynq::{time::Milliseconds, timer::GlobalTimer};

use crate::{cxp_ctrl::{Error, RXCTRLPacket, TXCTRLPacket, CTRL_PACKET_MAXSIZE, DATA_MAXSIZE},
            mem::mem,
            pl::csr};

const TRANSMISSION_TIMEOUT: u64 = 200;

// Section 9.6.1.2 (CXP-001-2021)
// CTRL packet need to be tagged for CXP 2.0 or greater
static mut TAG: u8 = 0;

pub fn reset_tag() {
    unsafe { TAG = 0 };
}

fn increment_tag() {
    unsafe { TAG = TAG.wrapping_add(1) };
}

fn check_tag(tag: Option<u8>) -> Result<(), Error> {
    unsafe {
        if tag.is_some() && tag != Some(TAG) {
            Err(Error::TagMismatch)
        } else {
            Ok(())
        }
    }
}

fn receive_ctrl_packet() -> Result<Option<RXCTRLPacket>, Error> {
    if unsafe { csr::cxp_grabber::core_rx_pending_packet_read() == 1 } {
        unsafe {
            let read_buffer_ptr = csr::cxp_grabber::core_rx_read_ptr_read() as usize;
            let ptr = (mem::CXP_MEM_BASE + mem::CXP_MEM_SIZE / 2 + read_buffer_ptr * CTRL_PACKET_MAXSIZE) as *mut u32;

            let mut reader = Cursor::new(slice::from_raw_parts_mut(ptr as *mut u8, CTRL_PACKET_MAXSIZE));
            let packet = RXCTRLPacket::read_from(&mut reader);

            csr::cxp_grabber::core_rx_pending_packet_write(1);
            Ok(Some(packet?))
        }
    } else {
        Ok(None)
    }
}

fn receive_ctrl_packet_timeout(timeout_ms: u64) -> Result<RXCTRLPacket, Error> {
    // assume timer was initialized successfully
    let timer = unsafe { GlobalTimer::get() };
    let limit = timer.get_time() + Milliseconds(timeout_ms);
    while timer.get_time() < limit {
        match receive_ctrl_packet()? {
            None => (),
            Some(packet) => return Ok(packet),
        }
    }
    Err(Error::TimedOut)
}

fn send_ctrl_packet(packet: &TXCTRLPacket) -> Result<(), Error> {
    unsafe {
        while csr::cxp_grabber::core_tx_writer_busy_read() == 1 {}
        let ptr = mem::CXP_MEM_BASE as *mut u32;
        let mut writer = Cursor::new(slice::from_raw_parts_mut(ptr as *mut u8, CTRL_PACKET_MAXSIZE));

        packet.write_to(&mut writer)?;

        csr::cxp_grabber::core_tx_writer_word_len_write((writer.position() / 4) as u8);
        csr::cxp_grabber::core_tx_writer_stb_write(1);
    }

    Ok(())
}

pub fn send_test_packet() -> Result<(), Error> {
    unsafe {
        while csr::cxp_grabber::core_tx_writer_busy_read() == 1 {}
        csr::cxp_grabber::core_tx_writer_stb_testseq_write(1);
    }
    Ok(())
}

fn get_ctrl_ack(timeout: u64) -> Result<(), Error> {
    match receive_ctrl_packet_timeout(timeout) {
        Ok(RXCTRLPacket::CtrlAck { tag }) => {
            check_tag(tag)?;
            Ok(())
        }
        Ok(RXCTRLPacket::CtrlDelay { tag, time }) => {
            check_tag(tag)?;
            get_ctrl_ack(time as u64)
        }
        Ok(_) => Err(Error::UnexpectedReply),
        Err(e) => Err(e),
    }
}

fn get_ctrl_reply(timeout: u64, expected_length: u32) -> Result<[u8; DATA_MAXSIZE], Error> {
    match receive_ctrl_packet_timeout(timeout) {
        Ok(RXCTRLPacket::CtrlReply { tag, length, data }) => {
            check_tag(tag)?;
            if length != expected_length {
                return Err(Error::UnexpectedReply);
            };
            Ok(data)
        }
        Ok(RXCTRLPacket::CtrlDelay { tag, time }) => {
            check_tag(tag)?;
            get_ctrl_reply(time as u64, expected_length)
        }
        Ok(_) => Err(Error::UnexpectedReply),
        Err(e) => Err(e),
    }
}

fn check_length(length: u32) -> Result<(), Error> {
    if length > DATA_MAXSIZE as u32 || length == 0 {
        Err(Error::LengthOutOfRange(length))
    } else {
        Ok(())
    }
}

pub fn write_bytes_no_ack(addr: u32, val: &[u8], with_tag: bool) -> Result<(), Error> {
    let length = val.len() as u32;
    check_length(length)?;

    let mut data: [u8; DATA_MAXSIZE] = [0; DATA_MAXSIZE];
    data[..length as usize].clone_from_slice(val);

    let tag: Option<u8> = if with_tag { Some(unsafe { TAG }) } else { None };
    send_ctrl_packet(&TXCTRLPacket::CtrlWrite {
        tag,
        addr,
        length,
        data,
    })
}

pub fn write_bytes(addr: u32, val: &[u8], with_tag: bool) -> Result<(), Error> {
    write_bytes_no_ack(addr, val, with_tag)?;
    get_ctrl_ack(TRANSMISSION_TIMEOUT)?;

    if with_tag {
        increment_tag();
    };
    Ok(())
}

pub fn write_u32(addr: u32, val: u32, with_tag: bool) -> Result<(), Error> {
    write_bytes(addr, &val.to_be_bytes(), with_tag)
}

pub fn write_u64(addr: u32, val: u64, with_tag: bool) -> Result<(), Error> {
    write_bytes(addr, &val.to_be_bytes(), with_tag)
}

pub fn read_bytes(addr: u32, bytes: &mut [u8], with_tag: bool) -> Result<(), Error> {
    let length = bytes.len() as u32;
    check_length(length)?;
    let tag: Option<u8> = if with_tag { Some(unsafe { TAG }) } else { None };
    send_ctrl_packet(&TXCTRLPacket::CtrlRead { tag, addr, length })?;

    let data = get_ctrl_reply(TRANSMISSION_TIMEOUT, length)?;
    bytes.clone_from_slice(&data[..length as usize]);

    if with_tag {
        increment_tag();
    };
    Ok(())
}

pub fn read_u32(addr: u32, with_tag: bool) -> Result<u32, Error> {
    let mut bytes: [u8; 4] = [0; 4];
    read_bytes(addr, &mut bytes, with_tag)?;
    let val = NetworkEndian::read_u32(&bytes);

    Ok(val)
}

pub fn read_u64(addr: u32, with_tag: bool) -> Result<u64, Error> {
    let mut bytes: [u8; 8] = [0; 8];
    read_bytes(addr, &mut bytes, with_tag)?;
    let val = NetworkEndian::read_u64(&bytes);

    Ok(val)
}
