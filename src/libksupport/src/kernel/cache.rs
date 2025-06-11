use alloc::{boxed::Box, string::String};
use core::mem::{forget, transmute};

use cslice::{AsCSlice, CSlice};

use super::{KERNEL_CHANNEL_0TO1, KERNEL_CHANNEL_1TO0, Message};

pub extern "C" fn get(key: CSlice<u8>) -> &CSlice<'static, i32> {
    let key = String::from_utf8(key.as_ref().to_vec()).unwrap();
    unsafe {
        KERNEL_CHANNEL_1TO0
            .as_mut()
            .unwrap()
            .send(Message::CacheGetRequest(key));
        let msg = KERNEL_CHANNEL_0TO1.as_mut().unwrap().recv();
        if let Message::CacheGetReply(v) = msg {
            let leaked = Box::new(v.as_c_slice());
            let reference = transmute(leaked.as_ref());
            forget(leaked);
            forget(v);
            reference
        } else {
            panic!("Expected CacheGetReply for CacheGetRequest");
        }
    }
}

pub extern "C" fn put(key: CSlice<u8>, list: &CSlice<i32>) {
    let key = String::from_utf8(key.as_ref().to_vec()).unwrap();
    let value = list.as_ref().to_vec();
    unsafe {
        KERNEL_CHANNEL_1TO0
            .as_mut()
            .unwrap()
            .send(Message::CachePutRequest(key, value));
    }
}
