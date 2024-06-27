use alloc::{string::String, vec::Vec};
use core::ptr;

use libcortex_a9::{mutex::Mutex, semaphore::Semaphore, sync_channel};

use crate::{eh_artiq, RPCException};

mod control;
pub use control::Control;
mod api;
pub mod core1;
mod dma;
mod rpc;
pub use dma::DmaRecorder;
mod cache;
#[cfg(has_drtio)]
mod subkernel;

#[cfg(has_drtio)]
#[derive(Debug, Clone)]
pub enum SubkernelStatus {
    NoError,
    Timeout,
    IncorrectState,
    CommLost,
    OtherError,
    Exception(Vec<u8>),
}

#[derive(Debug, Clone)]
pub enum Message {
    LoadRequest(Vec<u8>),
    LoadCompleted,
    LoadFailed,
    StartRequest,
    KernelFinished(u8),
    KernelException(
        &'static [Option<eh_artiq::Exception<'static>>],
        &'static [eh_artiq::StackPointerBacktrace],
        &'static [(usize, usize)],
        u8,
    ),
    RpcSend {
        is_async: bool,
        data: Vec<u8>,
    },
    RpcRecvRequest(*mut ()),
    RpcRecvReply(Result<usize, RPCException>),

    CacheGetRequest(String),
    CacheGetReply(Vec<i32>),
    CachePutRequest(String, Vec<i32>),

    DmaPutRequest(DmaRecorder),
    DmaEraseRequest(String),
    DmaGetRequest(String),
    DmaGetReply(Option<(i32, i64, bool)>),
    #[cfg(has_drtio)]
    DmaStartRemoteRequest {
        id: i32,
        timestamp: i64,
    },
    #[cfg(has_drtio)]
    DmaAwaitRemoteRequest(i32),
    #[cfg(has_drtio)]
    DmaAwaitRemoteReply {
        timeout: bool,
        error: u8,
        channel: u32,
        timestamp: u64,
    },

    #[cfg(has_drtio)]
    UpDestinationsRequest(i32),
    #[cfg(has_drtio)]
    UpDestinationsReply(bool),

    #[cfg(has_drtio)]
    SubkernelLoadRunRequest {
        id: u32,
        destination: u8,
        run: bool,
    },
    #[cfg(has_drtio)]
    SubkernelLoadRunReply {
        succeeded: bool,
    },
    #[cfg(has_drtio)]
    SubkernelAwaitFinishRequest {
        id: u32,
        timeout: i64,
    },
    #[cfg(has_drtio)]
    SubkernelAwaitFinishReply {
        status: SubkernelStatus,
    },
    #[cfg(has_drtio)]
    SubkernelMsgSend {
        id: u32,
        destination: Option<u8>,
        data: Vec<u8>,
    },
    #[cfg(has_drtio)]
    SubkernelMsgSent,
    #[cfg(has_drtio)]
    SubkernelMsgRecvRequest {
        id: i32,
        timeout: i64,
        tags: Vec<u8>,
    },
    #[cfg(has_drtio)]
    SubkernelMsgRecvReply {
        status: SubkernelStatus,
        count: u8,
    },
}

static CHANNEL_0TO1: Mutex<Option<sync_channel::Sender<'static, Message>>> = Mutex::new(None);
static CHANNEL_1TO0: Mutex<Option<sync_channel::Receiver<'static, Message>>> = Mutex::new(None);
static CHANNEL_SEM: Semaphore = Semaphore::new(0, 1);

static mut KERNEL_CHANNEL_0TO1: Option<sync_channel::Receiver<'static, Message>> = None;
static mut KERNEL_CHANNEL_1TO0: Option<sync_channel::Sender<'static, Message>> = None;

pub static mut KERNEL_IMAGE: *const core1::KernelImage = ptr::null();

static INIT_LOCK: Mutex<()> = Mutex::new(());
