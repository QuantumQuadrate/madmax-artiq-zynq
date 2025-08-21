#[cfg(has_drtio)]
use alloc::string::ToString;
use alloc::{collections::BTreeMap, rc::Rc, string::String, vec::Vec};
use core::{cell::RefCell, fmt, slice, str};

use core_io::Error as IoError;
use cslice::CSlice;
use dyld::elf;
use futures::{future::FutureExt, select_biased};
#[cfg(has_drtio)]
use io::Cursor;
use ksupport::kernel;
#[cfg(has_drtio)]
use ksupport::rpc;
use libasync::{block_async,
               smoltcp::{Sockets, TcpStream},
               task};
#[cfg(has_drtio)]
use libboard_artiq::drtioaux::Packet;
use libboard_artiq::{drtio_routing::{self, RoutingTable},
                     resolve_channel_name};
#[cfg(feature = "target_kasli_soc")]
use libboard_zynq::error_led::ErrorLED;
use libboard_zynq::{self as zynq,
                    i2c::Error as I2cError,
                    smoltcp::{self,
                              iface::{EthernetInterfaceBuilder, NeighborCache},
                              time::{Duration, Instant},
                              wire::IpCidr},
                    timer};
use libconfig::{self, net_settings};
use libcortex_a9::{mutex::Mutex,
                   once_lock::OnceLock,
                   semaphore::Semaphore,
                   sync_channel::{Receiver, Sender}};
use log::{error, info, warn};
use num_derive::{FromPrimitive, ToPrimitive};
use num_traits::{FromPrimitive, ToPrimitive};
#[cfg(has_drtiosat)]
use pl::csr::drtiosat as rtio_core;
#[cfg(has_rtio_core)]
use pl::csr::rtio_core;
#[cfg(has_drtio)]
use tar_no_std::TarArchiveRef;
use void::Void;

#[cfg(any(has_rtio_core, has_drtiosat, has_drtio))]
use crate::pl;
use crate::{analyzer, mgmt, moninj, proto_async::*, rpc_async, rtio_dma, rtio_mgt};
#[cfg(has_drtio)]
use crate::{subkernel, subkernel::Error as SubkernelError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NetworkError(smoltcp::Error),
    IoError,
    UnexpectedPattern,
    UnrecognizedPacket,
    BufferExhausted,
    #[cfg(has_drtio)]
    SubkernelError(subkernel::Error),
    #[cfg(has_drtio)]
    DestinationDown,
}

pub type Result<T> = core::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::NetworkError(error) => write!(f, "network error: {}", error),
            Error::IoError => write!(f, "io error"),
            Error::UnexpectedPattern => write!(f, "unexpected pattern"),
            Error::UnrecognizedPacket => write!(f, "unrecognized packet"),
            Error::BufferExhausted => write!(f, "buffer exhausted"),
            #[cfg(has_drtio)]
            Error::SubkernelError(error) => write!(f, "subkernel error: {:?}", error),
            #[cfg(has_drtio)]
            Error::DestinationDown => write!(f, "subkernel destination down"),
        }
    }
}

impl From<smoltcp::Error> for Error {
    fn from(error: smoltcp::Error) -> Self {
        Error::NetworkError(error)
    }
}

impl From<IoError> for Error {
    fn from(_error: IoError) -> Self {
        Error::IoError
    }
}

#[cfg(has_drtio)]
impl From<subkernel::Error> for Error {
    fn from(error: subkernel::Error) -> Self {
        Error::SubkernelError(error)
    }
}

#[derive(Debug, FromPrimitive, ToPrimitive)]
enum Request {
    SystemInfo = 3,
    LoadKernel = 5,
    RunKernel = 6,
    RPCReply = 7,
    RPCException = 8,
    UploadSubkernel = 9,
}

#[derive(Debug, FromPrimitive, ToPrimitive)]
enum Reply {
    SystemInfo = 2,
    LoadCompleted = 5,
    LoadFailed = 6,
    KernelFinished = 7,
    KernelStartupFailed = 8,
    KernelException = 9,
    RPCRequest = 10,
    WatchdogExpired = 14,
    ClockFailure = 15,
}

pub static mut SEEN_ASYNC_ERRORS: u8 = 0;

pub const ASYNC_ERROR_COLLISION: u8 = 1 << 0;
pub const ASYNC_ERROR_BUSY: u8 = 1 << 1;
pub const ASYNC_ERROR_SEQUENCE_ERROR: u8 = 1 << 2;

pub unsafe fn get_async_errors() -> u8 {
    let errors = SEEN_ASYNC_ERRORS;
    SEEN_ASYNC_ERRORS = 0;
    errors
}

fn wait_for_async_rtio_error() -> nb::Result<(), Void> {
    unsafe {
        #[cfg(has_rtio_core)]
        let errors = rtio_core::async_error_read();
        #[cfg(has_drtiosat)]
        let errors = rtio_core::protocol_error_read();
        if errors != 0 {
            Ok(())
        } else {
            Err(nb::Error::WouldBlock)
        }
    }
}

pub async fn report_async_rtio_errors() {
    loop {
        let _ = block_async!(wait_for_async_rtio_error()).await;
        unsafe {
            #[cfg(has_rtio_core)]
            let errors = rtio_core::async_error_read();
            #[cfg(has_drtiosat)]
            let errors = rtio_core::protocol_error_read();
            if errors & ASYNC_ERROR_COLLISION != 0 {
                let channel = rtio_core::collision_channel_read();
                error!(
                    "RTIO collision involving channel 0x{:04x}:{}",
                    channel,
                    resolve_channel_name(channel as u32)
                );
            }
            if errors & ASYNC_ERROR_BUSY != 0 {
                let channel = rtio_core::busy_channel_read();
                error!(
                    "RTIO busy error involving channel 0x{:04x}:{}",
                    channel,
                    resolve_channel_name(channel as u32)
                );
            }
            if errors & ASYNC_ERROR_SEQUENCE_ERROR != 0 {
                let channel = rtio_core::sequence_error_channel_read();
                error!(
                    "RTIO sequence error involving channel 0x{:04x}:{}",
                    channel,
                    resolve_channel_name(channel as u32)
                );
            }
            SEEN_ASYNC_ERRORS = errors;
            #[cfg(has_rtio_core)]
            rtio_core::async_error_write(errors);
            #[cfg(has_drtiosat)]
            rtio_core::protocol_error_write(errors);
        }
    }
}

static CACHE_STORE: Mutex<BTreeMap<String, Vec<i32>>> = Mutex::new(BTreeMap::new());

pub static RESTART_IDLE: Semaphore = Semaphore::new(1, 1);

pub static ROUTING_TABLE: OnceLock<RoutingTable> = OnceLock::new();

async fn write_header(stream: &TcpStream, reply: Reply) -> Result<()> {
    stream
        .send_slice(&[0x5a, 0x5a, 0x5a, 0x5a, reply.to_u8().unwrap()])
        .await?;
    Ok(())
}

async fn read_request(stream: &TcpStream, allow_close: bool) -> Result<Option<Request>> {
    match expect(stream, &[0x5a, 0x5a, 0x5a, 0x5a]).await {
        Ok(true) => {}
        Ok(false) => return Err(Error::UnexpectedPattern),
        Err(smoltcp::Error::Finished) => {
            if allow_close {
                info!("peer closed connection");
                return Ok(None);
            } else {
                error!("peer unexpectedly closed connection");
                return Err(smoltcp::Error::Finished)?;
            }
        }
        Err(e) => return Err(e)?,
    }
    Ok(Some(
        FromPrimitive::from_i8(read_i8(&stream).await?).ok_or(Error::UnrecognizedPacket)?,
    ))
}

async fn read_bytes(stream: &TcpStream, max_length: usize) -> Result<Vec<u8>> {
    let length = read_i32(&stream).await? as usize;
    if length > max_length {
        return Err(Error::BufferExhausted);
    }
    let mut buffer = vec![0; length];
    read_chunk(&stream, &mut buffer).await?;
    Ok(buffer)
}

const RETRY_LIMIT: usize = 100;

async fn fast_send(sender: &mut Sender<'_, kernel::Message>, content: kernel::Message) {
    let mut content = content;
    for _ in 0..RETRY_LIMIT {
        match sender.try_send(content) {
            Ok(()) => return,
            Err(v) => {
                content = v;
            }
        }
    }
    sender.async_send(content).await;
}

async fn fast_recv(receiver: &mut Receiver<'_, kernel::Message>) -> kernel::Message {
    for _ in 0..RETRY_LIMIT {
        match receiver.try_recv() {
            Ok(v) => return v,
            Err(()) => (),
        }
    }
    receiver.async_recv().await
}

async fn write_exception_string(stream: &TcpStream, s: CSlice<'static, u8>) -> Result<()> {
    if s.len() == usize::MAX {
        write_i32(stream, -1).await?;
        write_i32(stream, s.as_ptr() as i32).await?
    } else {
        write_chunk(stream, s.as_ref()).await?;
    };
    Ok(())
}

async fn handle_run_kernel(
    stream: Option<&TcpStream>,
    control: &Rc<RefCell<kernel::Control>>,
    _up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
) -> Result<()> {
    let i2c_bus = libboard_artiq::i2c::get_bus();
    control.borrow_mut().tx.async_send(kernel::Message::StartRequest).await;
    loop {
        let reply = control.borrow_mut().rx.async_recv().await;
        match reply {
            kernel::Message::RpcSend { is_async, data } => {
                if stream.is_none() {
                    error!("Unexpected RPC from startup/idle kernel!");
                    break;
                }
                let stream = stream.unwrap();
                write_header(stream, Reply::RPCRequest).await?;
                write_bool(stream, is_async).await?;
                stream.send_slice(&data).await?;
                if !is_async {
                    let host_request = read_request(stream, false).await?.unwrap();
                    match host_request {
                        Request::RPCReply => {
                            let tag = read_bytes(stream, 512).await?;
                            let slot = match fast_recv(&mut control.borrow_mut().rx).await {
                                kernel::Message::RpcRecvRequest(slot) => slot,
                                other => panic!("expected root value slot from core1, not {:?}", other),
                            };
                            rpc_async::recv_return(stream, &tag, slot, &|size| {
                                let control = control.clone();
                                async move {
                                    if size == 0 {
                                        // Don't try to allocate zero-length values, as RpcRecvReply(0) is
                                        // used to terminate the kernel-side receive loop.
                                        0 as *mut ()
                                    } else {
                                        let mut control = control.borrow_mut();
                                        fast_send(&mut control.tx, kernel::Message::RpcRecvReply(Ok(size))).await;
                                        match fast_recv(&mut control.rx).await {
                                            kernel::Message::RpcRecvRequest(slot) => slot,
                                            other => {
                                                panic!("expected nested value slot from kernel CPU, not {:?}", other)
                                            }
                                        }
                                    }
                                }
                            })
                            .await?;
                            control
                                .borrow_mut()
                                .tx
                                .async_send(kernel::Message::RpcRecvReply(Ok(0)))
                                .await;
                        }
                        Request::RPCException => {
                            let mut control = control.borrow_mut();
                            match control.rx.async_recv().await {
                                kernel::Message::RpcRecvRequest(_) => (),
                                other => panic!("expected (ignored) root value slot from kernel CPU, not {:?}", other),
                            }
                            let id = read_i32(stream).await? as u32;
                            let message = read_i32(stream).await? as u32;
                            let param = [
                                read_i64(stream).await?,
                                read_i64(stream).await?,
                                read_i64(stream).await?,
                            ];
                            let file = read_i32(stream).await? as u32;
                            let line = read_i32(stream).await?;
                            let column = read_i32(stream).await?;
                            let function = read_i32(stream).await? as u32;
                            control
                                .tx
                                .async_send(kernel::Message::RpcRecvReply(Err(ksupport::RPCException {
                                    id,
                                    message,
                                    param,
                                    file,
                                    line,
                                    column,
                                    function,
                                })))
                                .await;
                        }
                        _ => {
                            error!("unexpected RPC request from host: {:?}", host_request);
                            return Err(Error::UnrecognizedPacket);
                        }
                    }
                }
            }
            kernel::Message::KernelFinished => {
                let async_errors = unsafe { get_async_errors() };
                if let Some(stream) = stream {
                    write_header(stream, Reply::KernelFinished).await?;
                    write_i8(stream, async_errors as i8).await?;
                }
                break;
            }
            kernel::Message::KernelException(exceptions, stack_pointers, backtrace) => {
                let async_errors = unsafe { get_async_errors() };
                match stream {
                    Some(stream) => {
                        // only send the exception data to host if there is host,
                        // i.e. not idle/startup kernel.
                        write_header(stream, Reply::KernelException).await?;
                        write_i32(stream, exceptions.len() as i32).await?;
                        for exception in exceptions.iter() {
                            let exception = exception.as_ref().unwrap();
                            write_i32(stream, exception.id as i32).await?;

                            if exception.message.len() == usize::MAX {
                                // exception with host string
                                write_exception_string(stream, exception.message).await?;
                            } else {
                                let msg = str::from_utf8(unsafe {
                                    slice::from_raw_parts(exception.message.as_ptr(), exception.message.len())
                                })
                                .unwrap()
                                .replace(
                                    "{rtio_channel_info:0}",
                                    &format!(
                                        "0x{:04x}:{}",
                                        exception.param[0],
                                        resolve_channel_name(exception.param[0] as u32)
                                    ),
                                );
                                write_exception_string(stream, unsafe { CSlice::new(msg.as_ptr(), msg.len()) }).await?;
                            }

                            write_i64(stream, exception.param[0] as i64).await?;
                            write_i64(stream, exception.param[1] as i64).await?;
                            write_i64(stream, exception.param[2] as i64).await?;
                            write_exception_string(stream, exception.file).await?;
                            write_i32(stream, exception.line as i32).await?;
                            write_i32(stream, exception.column as i32).await?;
                            write_exception_string(stream, exception.function).await?;
                        }
                        for sp in stack_pointers.iter() {
                            write_i32(stream, sp.stack_pointer as i32).await?;
                            write_i32(stream, sp.initial_backtrace_size as i32).await?;
                            write_i32(stream, sp.current_backtrace_size as i32).await?;
                        }
                        write_i32(stream, backtrace.len() as i32).await?;
                        for &(addr, sp) in backtrace {
                            write_i32(stream, addr as i32).await?;
                            write_i32(stream, sp as i32).await?;
                        }
                        write_i8(stream, async_errors as i8).await?;
                    }
                    None => {
                        error!("Uncaught kernel exceptions: {:?}", exceptions);
                    }
                }
                break;
            }
            kernel::Message::CachePutRequest(key, value) => {
                CACHE_STORE.lock().insert(key, value);
            }
            kernel::Message::CacheGetRequest(key) => {
                const DEFAULT: Vec<i32> = Vec::new();
                let value = CACHE_STORE.lock().get(&key).unwrap_or(&DEFAULT).clone();
                control
                    .borrow_mut()
                    .tx
                    .async_send(kernel::Message::CacheGetReply(value))
                    .await;
            }
            kernel::Message::DmaPutRequest(recorder) => {
                let _id = rtio_dma::put_record(recorder).await;
                #[cfg(has_drtio)]
                rtio_dma::remote_dma::upload_traces(_id).await;
            }
            kernel::Message::DmaEraseRequest(name) => {
                // prevent possible OOM when we have large DMA record replacement.
                rtio_dma::erase(name).await;
            }
            kernel::Message::DmaGetRequest(name) => {
                let result = rtio_dma::retrieve(name).await;
                control
                    .borrow_mut()
                    .tx
                    .async_send(kernel::Message::DmaGetReply(result))
                    .await;
            }
            #[cfg(has_drtio)]
            kernel::Message::DmaStartRemoteRequest { id, timestamp } => {
                rtio_dma::remote_dma::playback(id as u32, timestamp as u64).await;
            }
            #[cfg(has_drtio)]
            kernel::Message::DmaAwaitRemoteRequest(id) => {
                let result = rtio_dma::remote_dma::await_done(id as u32, Some(10_000)).await;
                let reply = match result {
                    Ok(rtio_dma::remote_dma::RemoteState::PlaybackEnded {
                        error,
                        channel,
                        timestamp,
                    }) => kernel::Message::DmaAwaitRemoteReply {
                        timeout: false,
                        error: error,
                        channel: channel,
                        timestamp: timestamp,
                    },
                    _ => kernel::Message::DmaAwaitRemoteReply {
                        timeout: true,
                        error: 0,
                        channel: 0,
                        timestamp: 0,
                    },
                };
                control.borrow_mut().tx.async_send(reply).await;
            }
            kernel::Message::I2cStartRequest(busno)
            | kernel::Message::I2cRestartRequest(busno)
            | kernel::Message::I2cStopRequest(busno)
            | kernel::Message::I2cSwitchSelectRequest { busno, .. } => {
                let _destination = (busno >> 16) as u8;
                #[cfg(has_drtio)]
                if _destination != 0 {
                    let result = rtio_mgt::drtio::i2c_send_basic(&reply, busno).await;
                    let reply = match result {
                        Ok(succeeded) => kernel::Message::I2cBasicReply(succeeded),
                        Err(_) => kernel::Message::I2cBasicReply(false),
                    };
                    control.borrow_mut().tx.async_send(reply).await;
                    continue;
                }
                let mut succeeded = busno == 0;
                if succeeded {
                    succeeded = match &reply {
                        kernel::Message::I2cStartRequest(_) => i2c_bus.start().is_ok(),
                        kernel::Message::I2cRestartRequest(_) => i2c_bus.restart().is_ok(),
                        kernel::Message::I2cStopRequest(_) => i2c_bus.stop().is_ok(),
                        kernel::Message::I2cSwitchSelectRequest { address, mask, .. } => {
                            let ch = match mask {
                                //decode from mainline, PCA9548-centric API
                                0x00 => Some(None),
                                0x01 => Some(Some(0)),
                                0x02 => Some(Some(1)),
                                0x04 => Some(Some(2)),
                                0x08 => Some(Some(3)),
                                0x10 => Some(Some(4)),
                                0x20 => Some(Some(5)),
                                0x40 => Some(Some(6)),
                                0x80 => Some(Some(7)),
                                _ => None,
                            };
                            ch.is_some_and(|c| i2c_bus.pca954x_select(*address as u8, c).is_ok())
                        }
                        _ => unreachable!(),
                    }
                }
                control
                    .borrow_mut()
                    .tx
                    .async_send(kernel::Message::I2cBasicReply(succeeded))
                    .await;
            }
            kernel::Message::I2cWriteRequest { busno, data } => {
                let _destination = (busno >> 16) as u8;
                #[cfg(has_drtio)]
                if _destination != 0 {
                    let result = rtio_mgt::drtio::i2c_send_write(busno, data).await;
                    let reply = match result {
                        Ok((succeeded, ack)) => kernel::Message::I2cWriteReply { succeeded, ack },
                        Err(_) => kernel::Message::I2cWriteReply {
                            succeeded: false,
                            ack: false,
                        },
                    };
                    control.borrow_mut().tx.async_send(reply).await;
                    continue;
                }
                let mut succeeded = busno == 0;
                let mut ack = false;
                if succeeded {
                    (succeeded, ack) = match i2c_bus.write(data as u8) {
                        Ok(()) => (true, true),
                        Err(I2cError::Nack) => (true, false),
                        Err(_) => (false, false),
                    }
                }
                control
                    .borrow_mut()
                    .tx
                    .async_send(kernel::Message::I2cWriteReply { succeeded, ack })
                    .await;
            }
            kernel::Message::I2cReadRequest { busno, ack } => {
                let _destination = (busno >> 16) as u8;
                #[cfg(has_drtio)]
                if _destination != 0 {
                    let result = rtio_mgt::drtio::i2c_send_read(busno, ack).await;
                    let reply = match result {
                        Ok((succeeded, data)) => kernel::Message::I2cReadReply { succeeded, data },
                        Err(_) => kernel::Message::I2cReadReply {
                            succeeded: false,
                            data: 0xFF,
                        },
                    };
                    control.borrow_mut().tx.async_send(reply).await;
                    continue;
                }
                let mut succeeded = busno == 0;
                let mut data = 0xFF;
                if succeeded {
                    (succeeded, data) = match i2c_bus.read(ack) {
                        Ok(r) => (true, r),
                        Err(_) => (false, 0xFF),
                    }
                }
                control
                    .borrow_mut()
                    .tx
                    .async_send(kernel::Message::I2cReadReply { succeeded, data })
                    .await;
            }
            #[cfg(has_drtio)]
            kernel::Message::SubkernelLoadRunRequest {
                id,
                destination: _,
                run,
                timestamp,
            } => {
                let succeeded = match subkernel::load(id, run, timestamp).await {
                    Ok(()) => true,
                    Err(e) => {
                        error!("Error loading subkernel: {:?}", e);
                        false
                    }
                };
                control
                    .borrow_mut()
                    .tx
                    .async_send(kernel::Message::SubkernelLoadRunReply { succeeded: succeeded })
                    .await;
            }
            #[cfg(has_drtio)]
            kernel::Message::SubkernelAwaitFinishRequest { id, timeout } => {
                let res = subkernel::await_finish(id, timeout).await;
                let response = match res {
                    Ok(res) => {
                        if res.status == subkernel::FinishStatus::CommLost {
                            kernel::Message::SubkernelError(kernel::SubkernelStatus::CommLost)
                        } else if let Some(exception) = res.exception {
                            kernel::Message::SubkernelError(kernel::SubkernelStatus::Exception(exception))
                        } else {
                            kernel::Message::SubkernelAwaitFinishReply
                        }
                    }
                    Err(SubkernelError::Timeout) => kernel::Message::SubkernelError(kernel::SubkernelStatus::Timeout),
                    Err(SubkernelError::IncorrectState) => {
                        kernel::Message::SubkernelError(kernel::SubkernelStatus::IncorrectState)
                    }
                    Err(_) => kernel::Message::SubkernelError(kernel::SubkernelStatus::OtherError),
                };
                control.borrow_mut().tx.async_send(response).await;
            }
            #[cfg(has_drtio)]
            kernel::Message::SubkernelMsgSend { id, destination, data } => {
                let res = subkernel::message_send(id, destination.unwrap(), data).await;
                match res {
                    Ok(_) => (),
                    Err(e) => {
                        error!("error sending subkernel message: {:?}", e)
                    }
                };
                control
                    .borrow_mut()
                    .tx
                    .async_send(kernel::Message::SubkernelMsgSent)
                    .await;
            }
            #[cfg(has_drtio)]
            kernel::Message::SubkernelMsgRecvRequest { id, timeout, tags } => {
                let message_received = subkernel::message_await(id as u32, timeout).await;
                let response = match message_received {
                    Ok(ref message) => kernel::Message::SubkernelMsgRecvReply { count: message.count },
                    Err(SubkernelError::Timeout) => kernel::Message::SubkernelError(kernel::SubkernelStatus::Timeout),
                    Err(SubkernelError::IncorrectState) => {
                        kernel::Message::SubkernelError(kernel::SubkernelStatus::IncorrectState)
                    }
                    Err(SubkernelError::CommLost) => kernel::Message::SubkernelError(kernel::SubkernelStatus::CommLost),
                    Err(SubkernelError::SubkernelException) => {
                        // just retrieve the exception
                        let status = subkernel::await_finish(id as u32, timeout).await.unwrap();
                        kernel::Message::SubkernelError(kernel::SubkernelStatus::Exception(status.exception.unwrap()))
                    }
                    Err(_) => kernel::Message::SubkernelError(kernel::SubkernelStatus::OtherError),
                };
                control.borrow_mut().tx.async_send(response).await;
                if let Ok(message) = message_received {
                    // receive code almost identical to RPC recv, except we are not reading from a stream
                    let mut reader = Cursor::new(message.data);
                    let mut current_tags: &[u8] = &tags;
                    let mut i = 0;
                    loop {
                        // kernel has to consume all arguments in the whole message
                        let slot = match fast_recv(&mut control.borrow_mut().rx).await {
                            kernel::Message::RpcRecvRequest(slot) => slot,
                            other => panic!("expected root value slot from core1, not {:?}", other),
                        };
                        let remaining_tags = rpc::recv_return(&mut reader, &current_tags, slot, &mut |size| {
                            if size == 0 {
                                0 as *mut ()
                            } else {
                                let mut control = control.borrow_mut();
                                control.tx.send(kernel::Message::RpcRecvReply(Ok(size)));
                                match control.rx.recv() {
                                    kernel::Message::RpcRecvRequest(slot) => slot,
                                    other => {
                                        panic!("expected nested value slot from kernel CPU, not {:?}", other)
                                    }
                                }
                            }
                        })?;
                        control
                            .borrow_mut()
                            .tx
                            .async_send(kernel::Message::RpcRecvReply(Ok(0)))
                            .await;
                        i += 1;
                        if i < message.count {
                            current_tags = remaining_tags;
                        } else {
                            break;
                        }
                    }
                }
            }
            #[cfg(has_drtio)]
            kernel::Message::UpDestinationsRequest(destination) => {
                let result = _up_destinations.borrow()[destination as usize];
                control
                    .borrow_mut()
                    .tx
                    .async_send(kernel::Message::UpDestinationsReply(result))
                    .await;
            }
            #[cfg(has_drtio)]
            kernel::Message::RtioInitRequest => {
                rtio_mgt::drtio::reset().await;
                control.borrow_mut().tx.async_send(kernel::Message::RtioInitReply).await;
            }
            #[cfg(has_drtio)]
            kernel::Message::CXPReadRequest {
                destination,
                address,
                length,
            } => {
                let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
                let reply = loop {
                    let result = rtio_mgt::drtio::aux_transact(
                        linkno,
                        &Packet::CXPReadRequest {
                            destination,
                            address,
                            length,
                        },
                    )
                    .await;

                    match result {
                        Ok(Packet::CXPWaitReply) => {}
                        Ok(Packet::CXPReadReply { length, data }) => {
                            break kernel::Message::CXPReadReply { length, data };
                        }
                        Ok(Packet::CXPError { length, message }) => {
                            break kernel::Message::CXPError(
                                String::from_utf8_lossy(&message[..length as usize]).to_string(),
                            );
                        }
                        Ok(packet) => {
                            error!("received unexpected aux packet {:?}", packet);
                            break kernel::Message::CXPError("recevied unexpected drtio aux reply".to_string());
                        }
                        Err(e) => {
                            error!("aux packet error ({})", e);
                            break kernel::Message::CXPError("drtio aux error".to_string());
                        }
                    };
                };
                control.borrow_mut().tx.async_send(reply).await;
            }
            #[cfg(has_drtio)]
            kernel::Message::CXPWrite32Request {
                destination,
                address,
                value,
            } => {
                let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
                let reply = loop {
                    let drtioaux_packet = rtio_mgt::drtio::aux_transact(
                        linkno,
                        &Packet::CXPWrite32Request {
                            destination,
                            address,
                            value,
                        },
                    )
                    .await;

                    match drtioaux_packet {
                        Ok(Packet::CXPWaitReply) => {}
                        Ok(Packet::CXPWrite32Reply) => break kernel::Message::CXPWrite32Reply,
                        Ok(Packet::CXPError { length, message }) => {
                            break kernel::Message::CXPError(
                                String::from_utf8_lossy(&message[..length as usize]).to_string(),
                            );
                        }
                        Ok(packet) => {
                            error!("received unexpected aux packet {:?}", packet);
                            break kernel::Message::CXPError("recevied unexpected drtio aux reply".to_string());
                        }
                        Err(e) => {
                            error!("aux packet error ({})", e);
                            break kernel::Message::CXPError("drtio aux error".to_string());
                        }
                    };
                };
                control.borrow_mut().tx.async_send(reply).await;
            }
            #[cfg(has_drtio)]
            kernel::Message::CXPROIViewerSetupRequest {
                destination,
                x0,
                y0,
                x1,
                y1,
            } => {
                let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
                let drtioaux_packet = rtio_mgt::drtio::aux_transact(
                    linkno,
                    &Packet::CXPROIViewerSetupRequest {
                        destination,
                        x0,
                        y0,
                        x1,
                        y1,
                    },
                )
                .await;

                let reply = match drtioaux_packet {
                    Ok(Packet::CXPROIViewerSetupReply) => kernel::Message::CXPROIViewerSetupReply,
                    Ok(packet) => {
                        error!("received unexpected aux packet {:?}", packet);
                        kernel::Message::CXPError("recevied unexpected drtio aux reply".to_string())
                    }
                    Err(e) => {
                        error!("aux packet error ({})", e);
                        kernel::Message::CXPError("drtio aux error".to_string())
                    }
                };
                control.borrow_mut().tx.async_send(reply).await;
            }
            #[cfg(has_drtio)]
            kernel::Message::CXPROIViewerDataRequest { destination } => {
                let linkno = ROUTING_TABLE.get().unwrap().0[destination as usize][0] - 1;
                let reply = loop {
                    let drtioaux_packet =
                        rtio_mgt::drtio::aux_transact(linkno, &Packet::CXPROIViewerDataRequest { destination }).await;

                    match drtioaux_packet {
                        Ok(Packet::CXPWaitReply) => {}
                        Ok(Packet::CXPROIViewerPixelDataReply { length, data }) => {
                            break kernel::Message::CXPROIVIewerPixelDataReply { length, data };
                        }
                        Ok(Packet::CXPROIViewerFrameDataReply {
                            width,
                            height,
                            pixel_code,
                        }) => {
                            break kernel::Message::CXPROIVIewerFrameDataReply {
                                width,
                                height,
                                pixel_code,
                            };
                        }
                        Ok(packet) => {
                            error!("received unexpected aux packet {:?}", packet);
                            break kernel::Message::CXPError("recevied unexpected drtio aux reply".to_string());
                        }
                        Err(e) => {
                            error!("aux packet error ({})", e);
                            break kernel::Message::CXPError("drtio aux error".to_string());
                        }
                    };
                };
                control.borrow_mut().tx.async_send(reply).await;
            }
            _ => {
                panic!("unexpected message from core1 while kernel was running: {:?}", reply);
            }
        }
    }
    Ok(())
}

async fn handle_flash_kernel(
    buffer: &Vec<u8>,
    control: &Rc<RefCell<kernel::Control>>,
    _up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
) -> Result<()> {
    if buffer[0] == elf::ELFMAG0 && buffer[1] == elf::ELFMAG1 && buffer[2] == elf::ELFMAG2 && buffer[3] == elf::ELFMAG3
    {
        // assume ELF file, proceed as before
        load_kernel(buffer, control, None).await
    } else {
        #[cfg(has_drtio)]
        {
            let archive = TarArchiveRef::new(buffer.as_ref());
            let entries = archive.entries();
            let mut main_lib: Vec<u8> = Vec::new();
            for entry in entries {
                if entry.filename().as_str() == "main.elf" {
                    main_lib = entry.data().to_vec();
                } else {
                    // subkernel filename must be in format:
                    // "<subkernel id> <destination>.elf"
                    let filename = entry.filename();
                    let mut iter = filename.as_str().split_whitespace();
                    let sid: u32 = iter.next().unwrap().parse().unwrap();
                    let dest: u8 = iter.next().unwrap().strip_suffix(".elf").unwrap().parse().unwrap();
                    let up = _up_destinations.borrow()[dest as usize];
                    if up {
                        let subkernel_lib = entry.data().to_vec();
                        subkernel::add_subkernel(sid, dest, subkernel_lib).await;
                        match subkernel::upload(sid).await {
                            Ok(_) => (),
                            Err(_) => return Err(Error::UnexpectedPattern),
                        }
                    } else {
                        return Err(Error::DestinationDown);
                    }
                }
            }
            load_kernel(&main_lib, control, None).await
        }
        #[cfg(not(has_drtio))]
        {
            panic!("multi-kernel libraries are not supported in standalone systems");
        }
    }
}

async fn load_kernel(
    buffer: &Vec<u8>,
    control: &Rc<RefCell<kernel::Control>>,
    stream: Option<&TcpStream>,
) -> Result<()> {
    let mut control = control.borrow_mut();
    control.restart();
    control
        .tx
        .async_send(kernel::Message::LoadRequest(buffer.to_vec()))
        .await;
    let reply = control.rx.async_recv().await;
    match reply {
        kernel::Message::LoadCompleted => {
            if let Some(stream) = stream {
                write_header(stream, Reply::LoadCompleted).await?;
            }
            Ok(())
        }
        kernel::Message::LoadFailed => {
            if let Some(stream) = stream {
                write_header(stream, Reply::LoadFailed).await?;
                write_chunk(stream, b"core1 failed to process data").await?;
            } else {
                error!("Kernel load failed");
            }
            Err(Error::UnexpectedPattern)
        }
        _ => {
            error!("unexpected message from core1: {:?}", reply);
            if let Some(stream) = stream {
                write_header(stream, Reply::LoadFailed).await?;
                write_chunk(stream, b"core1 sent unexpected reply").await?;
            }
            Err(Error::UnrecognizedPacket)
        }
    }
}

async fn handle_connection(
    stream: &mut TcpStream,
    control: Rc<RefCell<kernel::Control>>,
    up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
) -> Result<()> {
    stream.set_ack_delay(None);

    if !expect(stream, b"ARTIQ coredev\n").await? {
        return Err(Error::UnexpectedPattern);
    }
    stream.send_slice("e".as_bytes()).await?;
    #[cfg(has_drtio)]
    subkernel::clear_subkernels().await;
    loop {
        let request = read_request(stream, true).await?;
        if request.is_none() {
            #[cfg(has_drtio)]
            subkernel::clear_subkernels().await;
            return Ok(());
        }
        let request = request.unwrap();
        match request {
            Request::SystemInfo => {
                write_header(stream, Reply::SystemInfo).await?;
                stream.send_slice("ARZQ".as_bytes()).await?;
            }
            Request::LoadKernel => {
                let buffer = read_bytes(stream, 1024 * 1024).await?;
                load_kernel(&buffer, &control, Some(stream)).await?;
            }
            Request::RunKernel => {
                handle_run_kernel(Some(stream), &control, &up_destinations).await?;
            }
            Request::UploadSubkernel => {
                #[cfg(has_drtio)]
                {
                    let id = read_i32(stream).await? as u32;
                    let destination = read_i8(stream).await? as u8;
                    let buffer = read_bytes(stream, 1024 * 1024).await?;
                    subkernel::add_subkernel(id, destination, buffer).await;
                    match subkernel::upload(id).await {
                        Ok(_) => write_header(stream, Reply::LoadCompleted).await?,
                        Err(_) => {
                            write_header(stream, Reply::LoadFailed).await?;
                            write_chunk(stream, b"subkernel failed to load").await?;
                            return Err(Error::UnexpectedPattern);
                        }
                    }
                }
                #[cfg(not(has_drtio))]
                {
                    write_header(stream, Reply::LoadFailed).await?;
                    write_chunk(stream, b"No DRTIO on this system, subkernels are not supported").await?;
                    return Err(Error::UnexpectedPattern);
                }
            }
            _ => {
                error!("unexpected request from host: {:?}", request);
                return Err(Error::UnrecognizedPacket);
            }
        }
    }
}

pub fn main() {
    let net_addresses = net_settings::get_addresses();
    info!("network addresses: {}", net_addresses);

    let eth = zynq::eth::Eth::eth0(net_addresses.hardware_addr.0.clone());
    const RX_LEN: usize = 64;
    // Number of transmission buffers (minimum is two because with
    // one, duplicate packet transmission occurs)
    const TX_LEN: usize = 64;
    let eth = eth.start_rx(RX_LEN);
    let mut eth = eth.start_tx(TX_LEN);

    let neighbor_cache = NeighborCache::new(alloc::collections::BTreeMap::new());
    let mut iface = match net_addresses.ipv6_addr {
        Some(addr) => {
            let ip_addrs = [
                IpCidr::new(net_addresses.ipv4_addr, 0),
                IpCidr::new(net_addresses.ipv6_ll_addr, 0),
                IpCidr::new(addr, 0),
            ];
            EthernetInterfaceBuilder::new(&mut eth)
                .ethernet_addr(net_addresses.hardware_addr)
                .ip_addrs(ip_addrs)
                .neighbor_cache(neighbor_cache)
                .finalize()
        }
        None => {
            let ip_addrs = [
                IpCidr::new(net_addresses.ipv4_addr, 0),
                IpCidr::new(net_addresses.ipv6_ll_addr, 0),
            ];
            EthernetInterfaceBuilder::new(&mut eth)
                .ethernet_addr(net_addresses.hardware_addr)
                .ip_addrs(ip_addrs)
                .neighbor_cache(neighbor_cache)
                .finalize()
        }
    };

    Sockets::init(32);

    #[cfg(has_drtio)]
    let res = ROUTING_TABLE.set(drtio_routing::config_routing_table(pl::csr::DRTIO.len()));
    #[cfg(not(has_drtio))]
    let res = ROUTING_TABLE.set(drtio_routing::RoutingTable::default_empty());
    res.expect("routing_table can only be initialized once");

    let up_destinations = Rc::new(RefCell::new([false; drtio_routing::DEST_COUNT]));
    #[cfg(has_drtio_routing)]
    drtio_routing::interconnect_disable_all();

    task::spawn(report_async_rtio_errors());
    rtio_mgt::startup(&up_destinations);
    libboard_artiq::setup_device_map();

    analyzer::start(&up_destinations);
    moninj::start();

    let control: Rc<RefCell<kernel::Control>> = Rc::new(RefCell::new(kernel::Control::start()));
    if let Ok(buffer) = libconfig::read("startup_kernel") {
        info!("Loading startup kernel...");
        if let Ok(()) = task::block_on(handle_flash_kernel(&buffer, &control, &up_destinations)) {
            info!("Starting startup kernel...");
            let _ = task::block_on(handle_run_kernel(None, &control, &up_destinations));
            info!("Startup kernel finished!");
        } else {
            error!("Error loading startup kernel!");
        }
    }

    mgmt::start();

    task::spawn(async move {
        let connection = Rc::new(Semaphore::new(1, 1));
        let terminate = Rc::new(Semaphore::new(0, 1));
        let can_restart_idle = Rc::new(Semaphore::new(1, 1));
        loop {
            let control = control.clone();
            let mut maybe_stream = select_biased! {
                s = (async {
                        TcpStream::accept(1381, 0x10_000, 0x10_000).await.unwrap()
                    }).fuse() => Some(s),
                _ = (async {
                        RESTART_IDLE.async_wait().await;
                        can_restart_idle.async_wait().await;
                    }).fuse() => None
            };

            if connection.try_wait().is_none() {
                // there is an existing connection
                terminate.signal();
                connection.async_wait().await;
            }

            let maybe_idle_kernel = libconfig::read("idle_kernel").ok();
            if maybe_idle_kernel.is_none() && maybe_stream.is_none() {
                control.borrow_mut().restart(); // terminate idle kernel if running
            }

            let control = control.clone();
            let connection = connection.clone();
            let terminate = terminate.clone();
            let can_restart_idle = can_restart_idle.clone();
            let up_destinations = up_destinations.clone();

            // we make sure the value of terminate is 0 before we start
            let _ = terminate.try_wait();
            let _ = can_restart_idle.try_wait();
            task::spawn(async move {
                select_biased! {
                    _ = (async {
                        if let Some(stream) = &mut maybe_stream {
                            let _ = handle_connection(stream, control.clone(), &up_destinations)
                                .await
                                .map_err(|e| warn!("connection terminated: {}", e));
                        }
                        can_restart_idle.signal();
                        match maybe_idle_kernel {
                            Some(buffer) => {
                                loop {
                                    info!("loading idle kernel");
                                    match handle_flash_kernel(&buffer, &control, &up_destinations).await {
                                        Ok(_) => {
                                            info!("running idle kernel");
                                            match handle_run_kernel(None, &control, &up_destinations).await {
                                                Ok(_) => info!("idle kernel finished"),
                                                Err(_) => warn!("idle kernel running error")
                                            }
                                        },
                                        Err(_) => warn!("idle kernel loading error")
                                    }
                                }
                            },
                            None => info!("no idle kernel found")
                        }
                    }).fuse() => (),
                    _ = terminate.async_wait().fuse() => ()
                }
                connection.signal();
                if let Some(stream) = maybe_stream {
                    let _ = stream.flush().await;
                    let _ = stream.abort().await;
                }
            });
        }
    });

    task::block_on(async {
        let mut last_link_check = Instant::from_millis(0);
        const LINK_CHECK_INTERVAL: u64 = 500;

        loop {
            let instant = Instant::from_millis(timer::get_ms() as i32);
            Sockets::instance().poll(&mut iface, instant);

            let dev = iface.device_mut();
            if dev.is_idle() && instant >= last_link_check + Duration::from_millis(LINK_CHECK_INTERVAL) {
                dev.check_link_change();
                last_link_check = instant;
            }

            task::r#yield().await;
        }
    })
}

pub fn soft_panic_main() -> ! {
    let net_addresses = net_settings::get_addresses();
    info!("network addresses: {}", net_addresses);

    let eth = zynq::eth::Eth::eth0(net_addresses.hardware_addr.0.clone());
    const RX_LEN: usize = 64;
    // Number of transmission buffers (minimum is two because with
    // one, duplicate packet transmission occurs)
    const TX_LEN: usize = 64;
    let eth = eth.start_rx(RX_LEN);
    let mut eth = eth.start_tx(TX_LEN);

    let neighbor_cache = NeighborCache::new(alloc::collections::BTreeMap::new());
    let mut iface = match net_addresses.ipv6_addr {
        Some(addr) => {
            let ip_addrs = [
                IpCidr::new(net_addresses.ipv4_addr, 0),
                IpCidr::new(net_addresses.ipv6_ll_addr, 0),
                IpCidr::new(addr, 0),
            ];
            EthernetInterfaceBuilder::new(&mut eth)
                .ethernet_addr(net_addresses.hardware_addr)
                .ip_addrs(ip_addrs)
                .neighbor_cache(neighbor_cache)
                .finalize()
        }
        None => {
            let ip_addrs = [
                IpCidr::new(net_addresses.ipv4_addr, 0),
                IpCidr::new(net_addresses.ipv6_ll_addr, 0),
            ];
            EthernetInterfaceBuilder::new(&mut eth)
                .ethernet_addr(net_addresses.hardware_addr)
                .ip_addrs(ip_addrs)
                .neighbor_cache(neighbor_cache)
                .finalize()
        }
    };

    Sockets::init(32);

    mgmt::start();

    // getting eth settings disables the LED as it resets GPIO
    // need to re-enable it here
    #[cfg(feature = "target_kasli_soc")]
    {
        let mut err_led = ErrorLED::error_led();
        err_led.toggle(true);
    }

    task::block_on(async {
        let mut last_link_check = Instant::from_millis(0);
        const LINK_CHECK_INTERVAL: u64 = 500;

        loop {
            let instant = Instant::from_millis(timer::get_ms() as i32);
            Sockets::instance().poll(&mut iface, instant);

            let dev = iface.device_mut();
            if dev.is_idle() && instant >= last_link_check + Duration::from_millis(LINK_CHECK_INTERVAL) {
                dev.check_link_change();
                last_link_check = instant;
            }

            task::r#yield().await;
        }
    })
}
