use alloc::vec::Vec;

use cslice::CSlice;

use super::{rtio::now_mu, Message, SubkernelStatus, KERNEL_CHANNEL_0TO1, KERNEL_CHANNEL_1TO0};
use crate::{artiq_raise, eh_artiq, rpc::send_args};

pub extern "C" fn load_run(id: u32, destination: u8, run: bool) {
    unsafe {
        KERNEL_CHANNEL_1TO0
            .as_mut()
            .unwrap()
            .send(Message::SubkernelLoadRunRequest {
                id: id,
                destination: destination,
                run: run,
                timestamp: now_mu() as u64,
            });
    }
    match unsafe { KERNEL_CHANNEL_0TO1.as_mut().unwrap() }.recv() {
        Message::SubkernelLoadRunReply { succeeded: true } => (),
        Message::SubkernelLoadRunReply { succeeded: false } => {
            artiq_raise!("SubkernelError", "Error loading or running the subkernel")
        }
        _ => panic!("Expected SubkernelLoadRunReply after SubkernelLoadRunRequest!"),
    }
}

pub extern "C" fn await_finish(id: u32, timeout: i64) {
    unsafe {
        KERNEL_CHANNEL_1TO0
            .as_mut()
            .unwrap()
            .send(Message::SubkernelAwaitFinishRequest {
                id: id,
                timeout: timeout,
            });
    }
    match unsafe { KERNEL_CHANNEL_0TO1.as_mut().unwrap() }.recv() {
        Message::SubkernelAwaitFinishReply => (),
        Message::SubkernelError(SubkernelStatus::IncorrectState) => {
            artiq_raise!("SubkernelError", "Subkernel not running")
        }
        Message::SubkernelError(SubkernelStatus::Timeout) => artiq_raise!("SubkernelError", "Subkernel timed out"),
        Message::SubkernelError(SubkernelStatus::CommLost) => {
            artiq_raise!("SubkernelError", "Lost communication with satellite")
        }
        Message::SubkernelError(SubkernelStatus::OtherError) => {
            artiq_raise!("SubkernelError", "An error occurred during subkernel operation")
        }
        Message::SubkernelError(SubkernelStatus::Exception(raw_exception)) => eh_artiq::raise_raw(&raw_exception),
        _ => panic!("expected SubkernelAwaitFinishReply after SubkernelAwaitFinishRequest"),
    }
}

pub extern "C" fn send_message(
    id: u32,
    is_return: bool,
    destination: u8,
    count: u8,
    tag: &CSlice<u8>,
    data: *const *const (),
) {
    let mut buffer = Vec::<u8>::new();
    send_args(&mut buffer, 0, tag.as_ref(), data, false).expect("RPC encoding failed");
    // overwrite service tag, include how many tags are in the message
    buffer[3] = count;
    unsafe {
        KERNEL_CHANNEL_1TO0.as_mut().unwrap().send(Message::SubkernelMsgSend {
            id: id,
            destination: if is_return { None } else { Some(destination) },
            data: buffer[3..].to_vec(),
        });
    }
    match unsafe { KERNEL_CHANNEL_0TO1.as_mut().unwrap() }.recv() {
        Message::SubkernelMsgSent => (),
        _ => panic!("expected SubkernelMsgSent after SubkernelMsgSend"),
    }
}

pub extern "C" fn await_message(id: i32, timeout: i64, tags: &CSlice<u8>, min: u8, max: u8) {
    unsafe {
        KERNEL_CHANNEL_1TO0
            .as_mut()
            .unwrap()
            .send(Message::SubkernelMsgRecvRequest {
                id: id,
                timeout: timeout,
                tags: tags.as_ref().to_vec(),
            });
    }
    match unsafe { KERNEL_CHANNEL_0TO1.as_mut().unwrap() }.recv() {
        Message::SubkernelMsgRecvReply { count } => {
            if min > count || count > max {
                artiq_raise!("SubkernelError", "Received more or less arguments than required")
            }
        }
        Message::SubkernelError(SubkernelStatus::IncorrectState) => {
            artiq_raise!("SubkernelError", "Subkernel not running")
        }
        Message::SubkernelError(SubkernelStatus::Timeout) => artiq_raise!("SubkernelError", "Subkernel timed out"),
        Message::SubkernelError(SubkernelStatus::CommLost) => {
            artiq_raise!("SubkernelError", "Lost communication with satellite")
        }
        Message::SubkernelError(SubkernelStatus::OtherError) => {
            artiq_raise!("SubkernelError", "An error occurred during subkernel operation")
        }
        Message::SubkernelError(SubkernelStatus::Exception(raw_exception)) => eh_artiq::raise_raw(&raw_exception),
        _ => panic!("expected SubkernelMsgRecvReply after SubkernelMsgRecvRequest"),
    }
    // RpcRecvRequest should be called after this to receive message data
}
