use core::fmt;
use core::cell::RefCell;
use core::str::Utf8Error;
use alloc::{vec, vec::Vec, string::String, collections::BTreeMap, rc::Rc};
use log::{info, warn, error};

use num_derive::{FromPrimitive, ToPrimitive};
use num_traits::{FromPrimitive, ToPrimitive};

use libboard_zynq::{
    self as zynq,
    smoltcp::{
        self,
        wire::IpCidr,
        iface::{NeighborCache, EthernetInterfaceBuilder},
        time::Instant,
    },
    timer::GlobalTimer,
};
use libcortex_a9::{semaphore::Semaphore, mutex::Mutex, sync_channel::{Sender, Receiver}};
use futures::{select_biased, future::FutureExt};
use libasync::{smoltcp::{Sockets, TcpStream}, task};
use libconfig::{Config, net_settings};

use crate::proto_async::*;
use crate::kernel;
use crate::rpc;
use crate::moninj;
use crate::mgmt;
use crate::analyzer;


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NetworkError(smoltcp::Error),
    UnexpectedPattern,
    UnrecognizedPacket,
    BufferExhausted,
    Utf8Error(Utf8Error),
}

pub type Result<T> = core::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::NetworkError(error) => write!(f, "network error: {}", error),
            Error::UnexpectedPattern   => write!(f, "unexpected pattern"),
            Error::UnrecognizedPacket  => write!(f, "unrecognized packet"),
            Error::BufferExhausted     => write!(f, "buffer exhausted"),
            Error::Utf8Error(error)    => write!(f, "UTF-8 error: {}", error),
        }
    }
}

impl From<smoltcp::Error> for Error {
    fn from(error: smoltcp::Error) -> Self {
        Error::NetworkError(error)
    }
}

#[derive(Debug, FromPrimitive, ToPrimitive)]
enum Request {
    SystemInfo = 3,
    LoadKernel = 5,
    RunKernel = 6,
    RPCReply = 7,
    RPCException = 8,
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

static CACHE_STORE: Mutex<BTreeMap<String, Vec<i32>>> = Mutex::new(BTreeMap::new());
static DMA_RECORD_STORE: Mutex<BTreeMap<String, (Vec<u8>, i64)>> = Mutex::new(BTreeMap::new());

async fn write_header(stream: &TcpStream, reply: Reply) -> Result<()> {
    stream.send_slice(&[0x5a, 0x5a, 0x5a, 0x5a, reply.to_u8().unwrap()]).await?;
    Ok(())
}

async fn read_request(stream: &TcpStream, allow_close: bool) -> Result<Option<Request>> {
    match expect(stream, &[0x5a, 0x5a, 0x5a, 0x5a]).await {
        Ok(true) => {}
        Ok(false) =>
            return Err(Error::UnexpectedPattern),
        Err(smoltcp::Error::Illegal) => {
            if allow_close {
                info!("peer closed connection");
                return Ok(None);
            } else {
                error!("peer unexpectedly closed connection");
                return Err(smoltcp::Error::Illegal)?;
            }
        },
        Err(e) =>
            return Err(e)?,
    }
    Ok(Some(FromPrimitive::from_i8(read_i8(&stream).await?).ok_or(Error::UnrecognizedPacket)?))
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

async fn read_string(stream: &TcpStream, max_length: usize) -> Result<String> {
    let bytes = read_bytes(stream, max_length).await?;
    Ok(String::from_utf8(bytes).map_err(|err| Error::Utf8Error(err.utf8_error()))?)
}

const RETRY_LIMIT: usize = 100;

async fn fast_send(sender: &mut Sender<'_, kernel::Message>, content: kernel::Message) {
    let mut content = content;
    for _ in 0..RETRY_LIMIT {
        match sender.try_send(content) {
            Ok(()) => {
                return
            },
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
            Ok(v) => {
                return v;
            },
            Err(()) => ()
        }
    }
    receiver.async_recv().await
}

async fn handle_run_kernel(stream: Option<&TcpStream>, control: &Rc<RefCell<kernel::Control>>) -> Result<()> {
    control.borrow_mut().tx.async_send(kernel::Message::StartRequest).await;
    loop {
        let reply = control.borrow_mut().rx.async_recv().await;
        match reply {
            kernel::Message::RpcSend { is_async, data } => {
                if stream.is_none() {
                    error!("Unexpected RPC from startup/idle kernel!");
                    break
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
                            rpc::recv_return(stream, &tag, slot, &|size| {
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
                                            other => panic!("expected nested value slot from kernel CPU, not {:?}", other),
                                        }
                                    }
                                }
                            }).await?;
                            control.borrow_mut().tx.async_send(kernel::Message::RpcRecvReply(Ok(0))).await;
                        },
                        Request::RPCException => {
                            let mut control = control.borrow_mut();
                            match control.rx.async_recv().await {
                                kernel::Message::RpcRecvRequest(_) => (),
                                other => panic!("expected (ignored) root value slot from kernel CPU, not {:?}", other),
                            }
                            let name =     read_string(stream, 16384).await?;
                            let message =  read_string(stream, 16384).await?;
                            let param =    [read_i64(stream).await?,
                                            read_i64(stream).await?,
                                            read_i64(stream).await?];
                            let file =     read_string(stream, 16384).await?;
                            let line =     read_i32(stream).await?;
                            let column =   read_i32(stream).await?;
                            let function = read_string(stream, 16384).await?;
                            control.tx.async_send(kernel::Message::RpcRecvReply(Err(kernel::RPCException {
                                name, message, param, file, line, column, function
                            }))).await;
                        },
                        _ => {
                            error!("unexpected RPC request from host: {:?}", host_request);
                            return Err(Error::UnrecognizedPacket)
                        }
                    }
                }
            },
            kernel::Message::KernelFinished => {
                if let Some(stream) = stream {
                    write_header(stream, Reply::KernelFinished).await?;
                }
                break;
            },
            kernel::Message::KernelException(exception, backtrace) => {
                match stream {
                    Some(stream) => {
                        // only send the exception data to host if there is host,
                        // i.e. not idle/startup kernel.
                        write_header(stream, Reply::KernelException).await?;
                        write_chunk(stream, exception.name.as_ref()).await?;
                        write_chunk(stream, exception.message.as_ref()).await?;
                        write_i64(stream, exception.param[0] as i64).await?;
                        write_i64(stream, exception.param[1] as i64).await?;
                        write_i64(stream, exception.param[2] as i64).await?;
                        write_chunk(stream, exception.file.as_ref()).await?;
                        write_i32(stream, exception.line as i32).await?;
                        write_i32(stream, exception.column as i32).await?;
                        write_chunk(stream, exception.function.as_ref()).await?;
                        write_i32(stream, backtrace.len() as i32).await?;
                        for &addr in backtrace {
                            write_i32(stream, addr as i32).await?;
                        }
                    },
                    None => {
                        error!("Uncaught kernel exception: {:?}", exception);
                    }
                }
                break;
            }
            kernel::Message::CachePutRequest(key, value) => {
                CACHE_STORE.lock().insert(key, value);
            },
            kernel::Message::CacheGetRequest(key) => {
                const DEFAULT: Vec<i32> = Vec::new();
                let value = CACHE_STORE.lock().get(&key).unwrap_or(&DEFAULT).clone();
                control.borrow_mut().tx.async_send(kernel::Message::CacheGetReply(value)).await;
            },
            kernel::Message::DmaPutRequest(recorder) => {
                DMA_RECORD_STORE.lock().insert(recorder.name, (recorder.buffer, recorder.duration));
            },
            kernel::Message::DmaEraseRequest(name) => {
                // prevent possible OOM when we have large DMA record replacement.
                DMA_RECORD_STORE.lock().remove(&name);
            },
            kernel::Message::DmaGetRequest(name) => {
                let result = DMA_RECORD_STORE.lock().get(&name).map(|v| v.clone());
                control.borrow_mut().tx.async_send(kernel::Message::DmaGetReply(result)).await;
            },
            _ => {
                panic!("unexpected message from core1 while kernel was running: {:?}", reply);
            }
        }
    }
    Ok(())
}


async fn load_kernel(buffer: &Vec<u8>, control: &Rc<RefCell<kernel::Control>>, stream: Option<&TcpStream>) -> Result<()> {
    let mut control = control.borrow_mut();
    control.restart();
    control.tx.async_send(kernel::Message::LoadRequest(buffer.to_vec())).await;
    let reply = control.rx.async_recv().await;
    match reply {
        kernel::Message::LoadCompleted => {
            if let Some(stream) = stream {
                write_header(stream, Reply::LoadCompleted).await?;
            }
            Ok(())
        },
        kernel::Message::LoadFailed => {
            if let Some(stream) = stream {
                write_header(stream, Reply::LoadFailed).await?;
                write_chunk(stream, b"core1 failed to process data").await?;
            } else {
                error!("Kernel load failed");
            }
            Err(Error::UnexpectedPattern)
        },
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

async fn handle_connection(stream: &TcpStream, control: Rc<RefCell<kernel::Control>>) -> Result<()> {
    if !expect(stream, b"ARTIQ coredev\n").await? {
        return Err(Error::UnexpectedPattern);
    }
    stream.send_slice("e".as_bytes()).await?;
    loop {
        let request = read_request(stream, true).await?;
        if request.is_none() {
            return Ok(());
        }
        let request = request.unwrap();
        match request {
            Request::SystemInfo => {
                write_header(stream, Reply::SystemInfo).await?;
                stream.send_slice("ARZQ".as_bytes()).await?;
            },
            Request::LoadKernel => {
                let buffer = read_bytes(stream, 1024*1024).await?;
                load_kernel(&buffer, &control, Some(stream)).await?;
            },
            Request::RunKernel => {
                handle_run_kernel(Some(stream), &control).await?;
            },
            _ => {
                error!("unexpected request from host: {:?}", request);
                return Err(Error::UnrecognizedPacket)
            }
        }
    }
}

pub fn main(timer: GlobalTimer, cfg: Config) {
    let net_addresses = net_settings::get_adresses(&cfg);
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
                IpCidr::new(addr, 0)
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
                IpCidr::new(net_addresses.ipv6_ll_addr, 0)
            ];
            EthernetInterfaceBuilder::new(&mut eth)
                       .ethernet_addr(net_addresses.hardware_addr)
                       .ip_addrs(ip_addrs)
                       .neighbor_cache(neighbor_cache)
                       .finalize()
        }
    };

    Sockets::init(32);

    analyzer::start();
    moninj::start(timer);

    let control: Rc<RefCell<kernel::Control>> = Rc::new(RefCell::new(kernel::Control::start()));
    let idle_kernel = Rc::new(cfg.read("idle_kernel").ok());
    if let Ok(buffer) = cfg.read("startup_kernel") {
        info!("Loading startup kernel...");
        if let Ok(()) = task::block_on(load_kernel(&buffer, &control, None)) {
            info!("Starting startup kernel...");
            let _ = task::block_on(handle_run_kernel(None, &control));
            info!("Startup kernel finished!");
        } else {
            error!("Error loading startup kernel!");
        }
    }

    mgmt::start(cfg);

    task::spawn(async move {
        let connection = Rc::new(Semaphore::new(1, 1));
        let terminate = Rc::new(Semaphore::new(0, 1));
        loop {
            let stream = TcpStream::accept(1381, 0x10_000, 0x10_000).await.unwrap();

            if connection.try_wait().is_none() {
                // there is an existing connection
                terminate.signal();
                connection.async_wait().await;
            }

            let control = control.clone();
            let idle_kernel = idle_kernel.clone();
            let connection = connection.clone();
            let terminate = terminate.clone();

            // we make sure the value of terminate is 0 before we start
            let _ = terminate.try_wait();
            task::spawn(async move {
                select_biased! {
                    _ = (async {
                        let _ = handle_connection(&stream, control.clone())
                            .await
                            .map_err(|e| warn!("connection terminated: {}", e));
                        if let Some(buffer) = &*idle_kernel {
                            info!("Loading idle kernel");
                            let _ = load_kernel(&buffer, &control, None)
                                .await.map_err(|_| warn!("error loading idle kernel"));
                            info!("Running idle kernel");
                            let _ = handle_run_kernel(None, &control)
                                .await.map_err(|_| warn!("error running idle kernel"));
                            info!("Idle kernel terminated");
                        }
                    }).fuse() => (),
                    _ = terminate.async_wait().fuse() => ()
                }
                connection.signal();
                let _ = stream.flush().await;
                let _ = stream.abort().await;
            });
        }
    });

    Sockets::run(&mut iface, || {
        Instant::from_millis(timer.get_time().0 as i32)
    });
}
