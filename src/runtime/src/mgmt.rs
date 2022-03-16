use futures::{future::poll_fn, task::Poll};
use libasync::{smoltcp::TcpStream, task};
use libboard_zynq::smoltcp;
use libconfig::Config;
use core::cell::RefCell;
use alloc::{rc::Rc, vec::Vec, string::String};
use log::{self, info, debug, warn, error, LevelFilter};

use crate::logger::{BufferLogger, LogBufferRef};
use crate::proto_async::*;
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NetworkError(smoltcp::Error),
    UnknownLogLevel(u8),
    UnexpectedPattern,
    UnrecognizedPacket,
}

type Result<T> = core::result::Result<T, Error>;

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            &Error::NetworkError(error)  => write!(f, "network error: {}", error),
            &Error::UnknownLogLevel(lvl) => write!(f, "unknown log level {}", lvl),
            &Error::UnexpectedPattern    => write!(f, "unexpected pattern"),
            &Error::UnrecognizedPacket   => write!(f, "unrecognized packet"),
        }
    }
}

impl From<smoltcp::Error> for Error {
    fn from(error: smoltcp::Error) -> Self {
        Error::NetworkError(error)
    }
}

#[derive(Debug, FromPrimitive)]
pub enum Request {
    GetLog = 1,
    ClearLog = 2,
    PullLog = 7,
    SetLogFilter = 3,
    SetUartLogFilter = 6,

    ConfigRead = 12,
    ConfigWrite = 13,
    ConfigRemove = 14,
}

#[repr(i8)]
pub enum Reply {
    Success = 1,
    LogContent = 2,
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
where
    F: Fn(&LogBufferRef) -> bool,
{
    poll_fn(|ctx| {
        let logger = unsafe { BufferLogger::get_logger().as_mut().unwrap() };
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

async fn handle_connection(
    stream: &mut TcpStream,
    pull_id: Rc<RefCell<u32>>,
    cfg: Rc<Config>) -> Result<()> {
    if !expect(&stream, b"ARTIQ management\n").await? {
        return Err(Error::UnexpectedPattern);
    }
    stream.send_slice("e".as_bytes()).await?;

    loop {
        let msg = read_i8(stream).await;
        if let Err(smoltcp::Error::Illegal) = msg {
            return Ok(());
        }
        let msg: Request = FromPrimitive::from_i8(msg?).ok_or(Error::UnrecognizedPacket)?;
        match msg {
            Request::GetLog => {
                let buffer = get_logger_buffer().await.extract().as_bytes().to_vec();
                write_i8(stream, Reply::LogContent as i8).await?;
                write_chunk(stream, &buffer).await?;
            }
            Request::ClearLog => {
                let mut buffer = get_logger_buffer().await;
                buffer.clear();
                write_i8(stream, Reply::Success as i8).await?;
            }
            Request::PullLog => {
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
                        break;
                    }
                    let bytes = buffer.extract().as_bytes().to_vec();
                    buffer.clear();
                    core::mem::drop(buffer);
                    write_chunk(stream, &bytes).await?;
                    if log::max_level() == LevelFilter::Trace {
                        // temporarily discard all trace level log
                        let logger = unsafe { BufferLogger::get_logger().as_mut().unwrap() };
                        logger.set_buffer_log_level(LevelFilter::Debug);
                        stream.flush().await?;
                        logger.set_buffer_log_level(LevelFilter::Trace);
                    }
                }
            },
            Request::SetLogFilter => {
                let lvl = read_log_level_filter(stream).await?;
                info!("Changing log level to {}", lvl);
                log::set_max_level(lvl);
                write_i8(stream, Reply::Success as i8).await?;
            }
            Request::SetUartLogFilter => {
                let lvl = read_log_level_filter(stream).await?;
                info!("Changing UART log level to {}", lvl);
                unsafe {
                    BufferLogger::get_logger()
                        .as_ref()
                        .unwrap()
                        .set_uart_log_level(lvl);
                }
                write_i8(stream, Reply::Success as i8).await?;
            }
            Request::ConfigRead => {
                let key = read_key(stream).await?;
                debug!("read key: {}", key);
                let value = cfg.read(&key);
                if let Ok(value) = value {
                    debug!("got value");
                    write_i8(stream, Reply::ConfigData as i8).await?;
                    write_chunk(stream, &value).await?;
                } else {
                    warn!("read error: no such key");
                    write_i8(stream, Reply::Error as i8).await?;
                }
            },
            Request::ConfigWrite => {
                let key = read_key(stream).await?;
                debug!("write key: {}", key);
                let len = read_i32(stream).await?;
                let len = if len <= 0 {
                    0
                } else {
                    len as usize
                };
                let mut buffer = Vec::with_capacity(len);
                unsafe {
                    buffer.set_len(len);
                }
                read_chunk(stream, &mut buffer).await?;
                let value = cfg.write(&key, buffer);
                if value.is_ok() {
                    debug!("write success");
                    write_i8(stream, Reply::Success as i8).await?;
                } else {
                    // this is an error because we do not expect write to fail
                    error!("failed to write: {:?}", value);
                    write_i8(stream, Reply::Error as i8).await?;
                }
            },
            Request::ConfigRemove => {
                let key = read_key(stream).await?;
                debug!("erase key: {}", key);
                let value = cfg.remove(&key);
                if value.is_ok() {
                    debug!("erase success");
                    write_i8(stream, Reply::Success as i8).await?;
                } else {
                    warn!("erase failed");
                    write_i8(stream, Reply::Error as i8).await?;
                }
            }
        }
    }
}

pub fn start(cfg: Config) {
    task::spawn(async move {
        let pull_id = Rc::new(RefCell::new(0u32));
        let cfg = Rc::new(cfg);
        loop {
            let mut stream = TcpStream::accept(1380, 2048, 2048).await.unwrap();
            let pull_id = pull_id.clone();
            let cfg = cfg.clone();
            task::spawn(async move {
                info!("received connection");
                let _ = handle_connection(&mut stream, pull_id, cfg)
                    .await
                    .map_err(|e| warn!("connection terminated: {:?}", e));
                let _ = stream.flush().await;
                let _ = stream.abort().await;
            });
        }
    });
}
