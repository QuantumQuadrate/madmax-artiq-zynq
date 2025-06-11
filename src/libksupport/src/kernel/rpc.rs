//! Kernel-side RPC API

use alloc::vec::Vec;

use cslice::CSlice;

use super::{KERNEL_CHANNEL_0TO1, KERNEL_CHANNEL_1TO0, Message};
use crate::{eh_artiq, rpc::send_args};

fn rpc_send_common(is_async: bool, service: u32, tag: &CSlice<u8>, data: *const *const ()) {
    let core1_tx = unsafe { KERNEL_CHANNEL_1TO0.as_mut().unwrap() };
    let mut buffer = Vec::<u8>::new();
    send_args(&mut buffer, service, tag.as_ref(), data, true).expect("RPC encoding failed");
    core1_tx.send(Message::RpcSend { is_async, data: buffer });
}

pub extern "C" fn rpc_send(service: u32, tag: &CSlice<u8>, data: *const *const ()) {
    rpc_send_common(false, service, tag, data);
}

pub extern "C" fn rpc_send_async(service: u32, tag: &CSlice<u8>, data: *const *const ()) {
    rpc_send_common(true, service, tag, data);
}

pub extern "C" fn rpc_recv(slot: *mut ()) -> usize {
    let reply = unsafe {
        let core1_rx = KERNEL_CHANNEL_0TO1.as_mut().unwrap();
        let core1_tx = KERNEL_CHANNEL_1TO0.as_mut().unwrap();
        core1_tx.send(Message::RpcRecvRequest(slot));
        core1_rx.recv()
    };
    match reply {
        Message::RpcRecvReply(Ok(alloc_size)) => alloc_size,
        Message::RpcRecvReply(Err(exception)) => unsafe {
            eh_artiq::raise(&eh_artiq::Exception {
                id: exception.id,
                file: CSlice::new(exception.file as *const u8, usize::MAX),
                line: exception.line as u32,
                column: exception.column as u32,
                function: CSlice::new(exception.function as *const u8, usize::MAX),
                message: CSlice::new(exception.message as *const u8, usize::MAX),
                param: exception.param,
            })
        },
        _ => panic!("received unexpected reply to RpcRecvRequest: {:?}", reply),
    }
}
