use alloc::{rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;

use byteorder::{ByteOrder, NativeEndian};
use crc::crc32;
use futures::{future::poll_fn, task::Poll};
use libasync::{smoltcp::TcpStream, task};
#[cfg(has_drtio)]
use libboard_artiq::drtio_routing;
use libboard_artiq::logger::{BufferLogger, LogBufferRef};
use libboard_zynq::smoltcp;
use libconfig;
use log::{self, debug, error, info, warn};
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;

use crate::{comms::RESTART_IDLE, proto_async::*};
#[cfg(has_drtio)]
use crate::{comms::ROUTING_TABLE, rtio_mgt::drtio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NetworkError(smoltcp::Error),
    OvertakeError,
    UnknownLogLevel(u8),
    UnexpectedPattern,
    UnrecognizedPacket,
    #[cfg(has_drtio)]
    DrtioError(drtio::Error),
}

type Result<T> = core::result::Result<T, Error>;

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            &Error::NetworkError(error) => write!(f, "network error: {}", error),
            &Error::OvertakeError => write!(f, "connection overtaken"),
            &Error::UnknownLogLevel(lvl) => write!(f, "unknown log level {}", lvl),
            &Error::UnexpectedPattern => write!(f, "unexpected pattern"),
            &Error::UnrecognizedPacket => write!(f, "unrecognized packet"),
            #[cfg(has_drtio)]
            &Error::DrtioError(error) => write!(f, "drtio error: {}", error),
        }
    }
}

impl From<smoltcp::Error> for Error {
    fn from(error: smoltcp::Error) -> Self {
        Error::NetworkError(error)
    }
}

#[cfg(has_drtio)]
impl From<drtio::Error> for Error {
    fn from(error: drtio::Error) -> Self {
        Error::DrtioError(error)
    }
}

#[derive(Debug, FromPrimitive)]
pub enum Request {
    GetLog = 1,
    ClearLog = 2,
    PullLog = 7,
    SetLogFilter = 3,
    Reboot = 5,
    SetUartLogFilter = 6,

    ConfigRead = 12,
    ConfigWrite = 13,
    ConfigRemove = 14,
    ConfigErase = 15,

    DebugAllocator = 8,

    Flash = 9,
}

#[repr(i8)]
pub enum Reply {
    Success = 1,
    LogContent = 2,
    RebootImminent = 3,
    Error = 6,
    ConfigData = 7,
}

async fn read_log_level_filter(stream: &mut TcpStream) -> Result<log::LevelFilter> {
    Ok(match read_i8(stream).await? {
        0 => log::LevelFilter::Off,
        1 => log::LevelFilter::Error,
        2 => log::LevelFilter::Warn,
        3 => log::LevelFilter::Info,
        4 => log::LevelFilter::Debug,
        5 => log::LevelFilter::Trace,
        lv => return Err(Error::UnknownLogLevel(lv as u8)),
    })
}

async fn get_logger_buffer_pred<F>(f: F) -> LogBufferRef<'static>
where F: Fn(&LogBufferRef) -> bool {
    poll_fn(|ctx| {
        let logger = BufferLogger::get_logger();
        match logger.buffer() {
            Some(buffer) if f(&buffer) => Poll::Ready(buffer),
            _ => {
                ctx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    })
    .await
}

async fn get_logger_buffer() -> LogBufferRef<'static> {
    get_logger_buffer_pred(|_| true).await
}

async fn read_key(stream: &mut TcpStream) -> Result<String> {
    let len = read_i32(stream).await?;
    if len <= 0 {
        write_i8(stream, Reply::Error as i8).await?;
        return Err(Error::UnexpectedPattern);
    }
    let mut buffer = Vec::with_capacity(len as usize);
    for _ in 0..len {
        buffer.push(0);
    }
    read_chunk(stream, &mut buffer).await?;
    if !buffer.is_ascii() {
        write_i8(stream, Reply::Error as i8).await?;
        return Err(Error::UnexpectedPattern);
    }
    Ok(String::from_utf8(buffer).unwrap())
}

#[cfg(has_drtio)]
mod remote_coremgmt {
    use core_io::Read;
    use io::ProtoWrite;
    use libboard_artiq::{drtioaux_async,
                         drtioaux_proto::{MASTER_PAYLOAD_MAX_SIZE, Packet}};

    use super::*;

    pub async fn get_log(stream: &mut TcpStream, linkno: u8, destination: u8) -> Result<()> {
        let mut buffer = Vec::new();
        loop {
            let reply = drtio::aux_transact(
                linkno,
                &Packet::CoreMgmtGetLogRequest {
                    destination,
                    clear: false,
                },
            )
            .await;

            match reply {
                Ok(Packet::CoreMgmtGetLogReply { last, length, data }) => {
                    buffer.extend(&data[..length as usize]);
                    if last {
                        write_i8(stream, Reply::LogContent as i8).await?;
                        write_chunk(stream, &buffer).await?;
                        return Ok(());
                    }
                }
                Ok(packet) => {
                    error!("received unexpected aux packet: {:?}", packet);
                    write_i8(stream, Reply::Error as i8).await?;
                    return Err(drtio::Error::UnexpectedReply.into());
                }
                Err(e) => {
                    error!("aux packet error ({})", e);
                    write_i8(stream, Reply::Error as i8).await?;
                    return Err(e.into());
                }
            }
        }
    }

    pub async fn clear_log(stream: &mut TcpStream, linkno: u8, destination: u8) -> Result<()> {
        let reply = drtio::aux_transact(linkno, &Packet::CoreMgmtClearLogRequest { destination }).await;

        match reply {
            Ok(Packet::CoreMgmtReply { succeeded: true }) => {
                write_i8(stream, Reply::Success as i8).await?;
                Ok(())
            }
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                write_i8(stream, Reply::Error as i8).await?;
                Err(drtio::Error::UnexpectedReply.into())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                write_i8(stream, Reply::Error as i8).await?;
                Err(e.into())
            }
        }
    }

    pub async fn pull_log(stream: &mut TcpStream, linkno: u8, destination: u8, pull_id: &RefCell<u32>) -> Result<()> {
        let id = {
            let mut guard = pull_id.borrow_mut();
            *guard += 1;
            *guard
        };
        let mut buffer = Vec::new();

        loop {
            if id != *pull_id.borrow() {
                // another connection attempts to pull the log...
                // abort this connection...
                return Err(Error::OvertakeError);
            }

            let reply = drtio::aux_transact(
                linkno,
                &Packet::CoreMgmtGetLogRequest {
                    destination,
                    clear: true,
                },
            )
            .await;

            match reply {
                Ok(Packet::CoreMgmtGetLogReply { last, length, data }) => {
                    buffer.extend(&data[..length as usize]);
                    if last {
                        write_chunk(stream, &buffer).await?;
                        buffer.clear();
                        task::r#yield().await;
                    }
                }
                Ok(packet) => {
                    error!("received unexpected aux packet: {:?}", packet);
                    return Err(drtio::Error::UnexpectedReply.into());
                }
                Err(e) => {
                    error!("aux packet error ({})", e);
                    return Err(e.into());
                }
            }
        }
    }

    pub async fn set_log_filter(
        stream: &mut TcpStream,
        linkno: u8,
        destination: u8,
        level: log::LevelFilter,
    ) -> Result<()> {
        let reply = drtio::aux_transact(
            linkno,
            &Packet::CoreMgmtSetLogLevelRequest {
                destination,
                log_level: level as u8,
            },
        )
        .await;

        match reply {
            Ok(Packet::CoreMgmtReply { succeeded: true }) => {
                write_i8(stream, Reply::Success as i8).await?;
                Ok(())
            }
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                write_i8(stream, Reply::Error as i8).await?;
                Err(drtio::Error::UnexpectedReply.into())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                write_i8(stream, Reply::Error as i8).await?;
                Err(e.into())
            }
        }
    }

    pub async fn set_uart_log_filter(
        stream: &mut TcpStream,
        linkno: u8,
        destination: u8,
        level: log::LevelFilter,
    ) -> Result<()> {
        let reply = drtio::aux_transact(
            linkno,
            &Packet::CoreMgmtSetUartLogLevelRequest {
                destination,
                log_level: level as u8,
            },
        )
        .await;

        match reply {
            Ok(Packet::CoreMgmtReply { succeeded: true }) => {
                write_i8(stream, Reply::Success as i8).await?;
                Ok(())
            }
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                write_i8(stream, Reply::Error as i8).await?;
                Err(drtio::Error::UnexpectedReply.into())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                write_i8(stream, Reply::Error as i8).await?;
                Err(e.into())
            }
        }
    }

    pub async fn config_read(stream: &mut TcpStream, linkno: u8, destination: u8, key: &String) -> Result<()> {
        let mut config_key: [u8; MASTER_PAYLOAD_MAX_SIZE] = [0; MASTER_PAYLOAD_MAX_SIZE];
        let len = key.len();
        config_key[..len].clone_from_slice(key.as_bytes());

        let mut reply = drtio::aux_transact(
            linkno,
            &Packet::CoreMgmtConfigReadRequest {
                destination: destination,
                length: len as u16,
                key: config_key,
            },
        )
        .await;

        let mut buffer = Vec::<u8>::new();
        loop {
            match reply {
                Ok(Packet::CoreMgmtConfigReadReply { last, length, value }) => {
                    buffer.extend(&value[..length as usize]);

                    if last {
                        write_i8(stream, Reply::ConfigData as i8).await?;
                        write_chunk(stream, &buffer).await?;
                        return Ok(());
                    }

                    reply = drtio::aux_transact(
                        linkno,
                        &Packet::CoreMgmtConfigReadContinue {
                            destination: destination,
                        },
                    )
                    .await;
                }
                Ok(packet) => {
                    error!("received unexpected aux packet: {:?}", packet);
                    write_i8(stream, Reply::Error as i8).await?;
                    return Err(drtio::Error::UnexpectedReply.into());
                }
                Err(e) => {
                    error!("aux packet error ({})", e);
                    write_i8(stream, Reply::Error as i8).await?;
                    return Err(e.into());
                }
            }
        }
    }

    pub async fn config_write(
        stream: &mut TcpStream,
        linkno: u8,
        destination: u8,
        key: &String,
        value: Vec<u8>,
    ) -> Result<()> {
        let mut message = Vec::with_capacity(key.len() + value.len() + 4 * 2);
        message.write_string::<NativeEndian>(key).unwrap();
        message.write_bytes::<NativeEndian>(&value).unwrap();

        match drtio::partition_data(
            linkno,
            &message,
            |slice, status, len: usize| Packet::CoreMgmtConfigWriteRequest {
                destination: destination,
                last: status.is_last(),
                length: len as u16,
                data: *slice,
            },
            |reply| match reply {
                Packet::CoreMgmtReply { succeeded: true } => Ok(()),
                packet => {
                    error!("received unexpected aux packet: {:?}", packet);
                    Err(drtio::Error::UnexpectedReply)
                }
            },
        )
        .await
        {
            Ok(()) => {
                write_i8(stream, Reply::Success as i8).await?;
                Ok(())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                write_i8(stream, Reply::Error as i8).await?;
                Err(e.into())
            }
        }
    }

    pub async fn config_remove(stream: &mut TcpStream, linkno: u8, destination: u8, key: &String) -> Result<()> {
        let mut config_key: [u8; MASTER_PAYLOAD_MAX_SIZE] = [0; MASTER_PAYLOAD_MAX_SIZE];
        let len = key.len();
        config_key[..len].clone_from_slice(key.as_bytes());

        let reply = drtio::aux_transact(
            linkno,
            &Packet::CoreMgmtConfigRemoveRequest {
                destination: destination,
                length: len as u16,
                key: config_key,
            },
        )
        .await;

        match reply {
            Ok(Packet::CoreMgmtReply { succeeded: true }) => {
                write_i8(stream, Reply::Success as i8).await?;
                Ok(())
            }
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                write_i8(stream, Reply::Error as i8).await?;
                Err(drtio::Error::UnexpectedReply.into())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                write_i8(stream, Reply::Error as i8).await?;
                Err(e.into())
            }
        }
    }

    pub async fn config_erase(stream: &mut TcpStream, linkno: u8, destination: u8) -> Result<()> {
        let reply = drtio::aux_transact(
            linkno,
            &Packet::CoreMgmtConfigEraseRequest {
                destination: destination,
            },
        )
        .await;

        match reply {
            Ok(Packet::CoreMgmtReply { succeeded: true }) => {
                write_i8(stream, Reply::Success as i8).await?;
                Ok(())
            }
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                write_i8(stream, Reply::Error as i8).await?;
                Err(drtio::Error::UnexpectedReply.into())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                write_i8(stream, Reply::Error as i8).await?;
                Err(e.into())
            }
        }
    }

    pub async fn reboot(stream: &mut TcpStream, linkno: u8, destination: u8) -> Result<()> {
        let reply = drtio::aux_transact(
            linkno,
            &Packet::CoreMgmtRebootRequest {
                destination: destination,
            },
        )
        .await;

        match reply {
            Ok(Packet::CoreMgmtReply { succeeded: true }) => {
                write_i8(stream, Reply::RebootImminent as i8).await?;
                Ok(())
            }
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                write_i8(stream, Reply::Error as i8).await?;
                Err(drtio::Error::UnexpectedReply.into())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                write_i8(stream, Reply::Error as i8).await?;
                Err(e.into())
            }
        }
    }

    pub async fn debug_allocator(stream: &mut TcpStream, linkno: u8, destination: u8) -> Result<()> {
        let reply = drtio::aux_transact(
            linkno,
            &Packet::CoreMgmtAllocatorDebugRequest {
                destination: destination,
            },
        )
        .await;

        match reply {
            Ok(Packet::CoreMgmtReply { succeeded: true }) => {
                write_i8(stream, Reply::Success as i8).await?;
                Ok(())
            }
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                Err(drtio::Error::UnexpectedReply.into())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                Err(e.into())
            }
        }
    }

    pub async fn image_write(stream: &mut TcpStream, linkno: u8, destination: u8, image: Vec<u8>) -> Result<()> {
        let mut image = &image[..];

        let alloc_reply = drtio::aux_transact(
            linkno,
            &Packet::CoreMgmtFlashRequest {
                destination: destination,
                payload_length: image.len() as u32,
            },
        )
        .await;

        match alloc_reply {
            Ok(Packet::CoreMgmtReply { succeeded: true }) => Ok(()),
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                write_i8(stream, Reply::Error as i8).await?;
                Err(drtio::Error::UnexpectedReply)
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                write_i8(stream, Reply::Error as i8).await?;
                Err(drtio::Error::AuxError)
            }
        }?;

        while !image.is_empty() {
            let mut data = [0; MASTER_PAYLOAD_MAX_SIZE];
            let len = image.read(&mut data).unwrap();
            let last = image.is_empty();

            let reply = drtio::aux_transact(
                linkno,
                &Packet::CoreMgmtFlashAddDataRequest {
                    destination: destination,
                    last: last,
                    length: len as u16,
                    data: data,
                },
            )
            .await;

            match reply {
                Ok(Packet::CoreMgmtReply { succeeded: true }) if !last => Ok(()),
                Ok(Packet::CoreMgmtDropLink) if last => drtioaux_async::send(
                    linkno,
                    &Packet::CoreMgmtDropLinkAck {
                        destination: destination,
                    },
                )
                .await
                .map_err(|_| drtio::Error::AuxError),
                Ok(packet) => {
                    error!("received unexpected aux packet: {:?}", packet);
                    write_i8(stream, Reply::Error as i8).await?;
                    Err(drtio::Error::UnexpectedReply)
                }
                Err(e) => {
                    error!("aux packet error ({})", e);
                    write_i8(stream, Reply::Error as i8).await?;
                    Err(drtio::Error::AuxError)
                }
            }?;
        }

        write_i8(stream, Reply::RebootImminent as i8).await?;
        Ok(())
    }
}

mod local_coremgmt {
    use libboard_zynq::slcr;

    use super::*;

    pub async fn get_log(stream: &mut TcpStream) -> Result<()> {
        let buffer = get_logger_buffer().await.extract().as_bytes().to_vec();
        write_i8(stream, Reply::LogContent as i8).await?;
        write_chunk(stream, &buffer).await?;
        Ok(())
    }

    pub async fn clear_log(stream: &mut TcpStream) -> Result<()> {
        let mut buffer = get_logger_buffer().await;
        buffer.clear();
        write_i8(stream, Reply::Success as i8).await?;
        Ok(())
    }

    pub async fn pull_log(stream: &mut TcpStream, pull_id: &RefCell<u32>) -> Result<()> {
        let id = {
            let mut guard = pull_id.borrow_mut();
            *guard += 1;
            *guard
        };
        loop {
            let mut buffer = get_logger_buffer_pred(|b| !b.is_empty()).await;
            if id != *pull_id.borrow() {
                // another connection attempts to pull the log...
                // abort this connection...
                return Err(Error::OvertakeError);
            }
            let bytes = buffer.extract().as_bytes().to_vec();
            buffer.clear();
            core::mem::drop(buffer);
            write_chunk(stream, &bytes).await?;
            if BufferLogger::get_logger().buffer_log_level() == log::LevelFilter::Trace{
                let logger = BufferLogger::get_logger();
                logger.set_buffer_log_level(log::LevelFilter::Debug);
                stream.flush().await?;
                logger.set_buffer_log_level(log::LevelFilter::Trace);
            }
        }
    }

    pub async fn set_log_filter(stream: &mut TcpStream, lvl: log::LevelFilter) -> Result<()> {
        info!("Changing log level to {}", lvl);
        BufferLogger::get_logger().set_buffer_log_level(lvl);
        write_i8(stream, Reply::Success as i8).await?;
        Ok(())
    }

    pub async fn set_uart_log_filter(stream: &mut TcpStream, lvl: log::LevelFilter) -> Result<()> {
        info!("Changing UART log level to {}", lvl);
        BufferLogger::get_logger().set_uart_log_level(lvl);
        write_i8(stream, Reply::Success as i8).await?;
        Ok(())
    }

    pub async fn config_read(stream: &mut TcpStream, key: &String) -> Result<()> {
        let value = libconfig::read(&key);
        if let Ok(value) = value {
            debug!("got value");
            write_i8(stream, Reply::ConfigData as i8).await?;
            write_chunk(stream, &value).await?;
        } else {
            warn!("read error: no such key");
            write_i8(stream, Reply::Error as i8).await?;
        }
        Ok(())
    }

    pub async fn config_write(stream: &mut TcpStream, key: &String, value: Vec<u8>) -> Result<()> {
        let value = libconfig::write(&key, value);
        if value.is_ok() {
            debug!("write success");
            if key == "idle_kernel" {
                RESTART_IDLE.signal();
            }
            write_i8(stream, Reply::Success as i8).await?;
        } else {
            // this is an error because we do not expect write to fail
            error!("failed to write: {:?}", value);
            write_i8(stream, Reply::Error as i8).await?;
        }
        Ok(())
    }

    pub async fn config_remove(stream: &mut TcpStream, key: &String) -> Result<()> {
        debug!("erase key: {}", key);
        let value = libconfig::remove(&key);
        if value.is_ok() {
            debug!("erase success");
            if key == "idle_kernel" {
                RESTART_IDLE.signal();
            }
            write_i8(stream, Reply::Success as i8).await?;
        } else {
            warn!("erase failed");
            write_i8(stream, Reply::Error as i8).await?;
        }
        Ok(())
    }

    pub async fn config_erase(stream: &mut TcpStream) -> Result<()> {
        error!("zynq device does not support config erase");
        write_i8(stream, Reply::Error as i8).await?;
        Ok(())
    }

    pub async fn reboot(stream: &mut TcpStream) -> Result<()> {
        info!("rebooting");
        log::logger().flush();
        write_i8(stream, Reply::RebootImminent as i8).await?;
        stream.flush().await?;
        slcr::reboot();

        unreachable!()
    }

    pub async fn debug_allocator(_stream: &mut TcpStream) -> Result<()> {
        error!("zynq device does not support allocator debug print");
        Ok(())
    }

    pub async fn image_write(stream: &mut TcpStream, image: Vec<u8>) -> Result<()> {
        let mut image = image.clone();
        let image_ref = &image[..];
        let bin_len = image.len() - 4;

        let (image_ref, expected_crc) = {
            let (image_ref, crc_slice) = image_ref.split_at(bin_len);
            (image_ref, NativeEndian::read_u32(crc_slice))
        };

        let actual_crc = crc32::checksum_ieee(image_ref);

        if actual_crc == expected_crc {
            info!("CRC passed. Writing boot image to SD card...");
            image.truncate(bin_len);
            libconfig::write("boot", image).expect("failed to write boot image");
            reboot(stream).await?;
        } else {
            error!(
                "CRC failed, images have not been written to flash.\n(actual {:08x}, expected {:08x})",
                actual_crc, expected_crc
            );
            write_i8(stream, Reply::Error as i8).await?;
        }
        Ok(())
    }
}

#[cfg(has_drtio)]
macro_rules! process {
    ($stream: ident, $destination:expr, $func:ident $(, $param:expr)*) => {{
        let hop = ROUTING_TABLE.get().unwrap().0[$destination as usize][0];
        let linkno = hop - 1 as u8;
        if hop == 0 {
            local_coremgmt::$func($stream, $($param, )*).await
        } else {
            remote_coremgmt::$func($stream, linkno, $destination, $($param, )*).await
        }
    }}
}

#[cfg(not(has_drtio))]
macro_rules! process {
    ($stream: ident, $destination:expr, $func:ident $(, $param:expr)*) => {{
        local_coremgmt::$func($stream, $($param, )*).await
    }}
}

async fn handle_connection(stream: &mut TcpStream, pull_ids: Rc<[RefCell<u32>]>) -> Result<()> {
    if !expect(&stream, b"ARTIQ management\n").await? {
        return Err(Error::UnexpectedPattern);
    }

    let _destination: u8 = read_i8(stream).await? as u8;
    stream.send_slice("e".as_bytes()).await?;

    let pull_id = &pull_ids[_destination as usize];

    loop {
        let msg = read_i8(stream).await;
        if let Err(smoltcp::Error::Finished) = msg {
            return Ok(());
        }
        let msg: Request = FromPrimitive::from_i8(msg?).ok_or(Error::UnrecognizedPacket)?;
        match msg {
            Request::GetLog => process!(stream, _destination, get_log),
            Request::ClearLog => process!(stream, _destination, clear_log),
            Request::PullLog => process!(stream, _destination, pull_log, pull_id),
            Request::SetLogFilter => {
                let lvl = read_log_level_filter(stream).await?;
                process!(stream, _destination, set_log_filter, lvl)
            }
            Request::SetUartLogFilter => {
                let lvl = read_log_level_filter(stream).await?;
                process!(stream, _destination, set_uart_log_filter, lvl)
            }
            Request::ConfigRead => {
                let key = read_key(stream).await?;
                process!(stream, _destination, config_read, &key)
            }
            Request::ConfigWrite => {
                let key = read_key(stream).await?;
                let len = read_i32(stream).await?;
                let len = if len <= 0 { 0 } else { len as usize };
                let mut buffer = Vec::with_capacity(len);
                unsafe {
                    buffer.set_len(len);
                }
                read_chunk(stream, &mut buffer).await?;
                process!(stream, _destination, config_write, &key, buffer)
            }
            Request::ConfigRemove => {
                let key = read_key(stream).await?;
                process!(stream, _destination, config_remove, &key)
            }
            Request::Reboot => {
                process!(stream, _destination, reboot)
            }
            Request::ConfigErase => {
                process!(stream, _destination, config_erase)
            }
            Request::DebugAllocator => {
                process!(stream, _destination, debug_allocator)
            }
            Request::Flash => {
                let len = read_i32(stream).await?;
                if len <= 0 {
                    write_i8(stream, Reply::Error as i8).await?;
                    return Err(Error::UnexpectedPattern);
                }
                let mut buffer = Vec::with_capacity(len as usize);
                unsafe {
                    buffer.set_len(len as usize);
                }
                read_chunk(stream, &mut buffer).await?;
                process!(stream, _destination, image_write, buffer)
            }
        }?;
    }
}

pub fn start() {
    task::spawn(async move {
        #[cfg(has_drtio)]
        let pull_ids = Rc::new([const { RefCell::new(0u32) }; drtio_routing::DEST_COUNT]);
        #[cfg(not(has_drtio))]
        let pull_ids = Rc::new([RefCell::new(0u32); 1]);
        loop {
            let mut stream = TcpStream::accept(1380, 2048, 2048).await.unwrap();
            let pull_ids = pull_ids.clone();
            task::spawn(async move {
                info!("received connection");
                let _ = handle_connection(&mut stream, pull_ids)
                    .await
                    .map_err(|e| warn!("connection terminated: {:?}", e));
                let _ = stream.flush().await;
                let _ = stream.abort().await;
            });
        }
    });
}
