use crate::{
    pl::csr,
    artiq_raise,
    rtio,
};
use alloc::{vec::Vec, string::String, boxed::Box};
use cslice::CSlice;
use super::{KERNEL_IMAGE, KERNEL_CHANNEL_0TO1, KERNEL_CHANNEL_1TO0, Message};
use core::mem;

use libcortex_a9::cache::dcci_slice;

const ALIGNMENT: usize = 16 * 8;

#[repr(C)]
pub struct DmaTrace {
    duration: i64,
    address:  i32,
}

#[derive(Clone, Debug)]
pub struct DmaRecorder {
    pub name: String,
    pub buffer: Vec<u8>,
    pub duration: i64,
}

static mut RECORDER: Option<DmaRecorder> = None;

pub unsafe fn init_dma_recorder() {
    // as static would remain after restart, we have to reset it,
    // without running its destructor.
    mem::forget(mem::replace(&mut RECORDER, None));
}

pub extern fn dma_record_start(name: CSlice<u8>) {
    let name = String::from_utf8(name.as_ref().to_vec()).unwrap();
    unsafe {
        KERNEL_CHANNEL_1TO0.as_mut().unwrap().send(Message::DmaEraseRequest(name.clone()));
    }
    unsafe {
        if RECORDER.is_some() {
            artiq_raise!("DMAError", "DMA is already recording")
        }

        let library = KERNEL_IMAGE.as_ref().unwrap();
        library.rebind(b"rtio_output",
                       dma_record_output as *const ()).unwrap();
        library.rebind(b"rtio_output_wide",
                       dma_record_output_wide as *const ()).unwrap();

        RECORDER = Some(DmaRecorder {
            name,
            buffer: Vec::new(),
            duration: 0,
        });
    }
}

pub extern fn dma_record_stop(duration: i64) {
    unsafe {
        if RECORDER.is_none() {
            artiq_raise!("DMAError", "DMA is not recording")
        }

        let library = KERNEL_IMAGE.as_ref().unwrap();
        library.rebind(b"rtio_output",
                       rtio::output as *const ()).unwrap();
        library.rebind(b"rtio_output_wide",
                       rtio::output_wide as *const ()).unwrap();

        let mut recorder = RECORDER.take().unwrap();
        recorder.duration = duration;
        KERNEL_CHANNEL_1TO0.as_mut().unwrap().send(
            Message::DmaPutRequest(recorder)
        );
    }
}

#[inline(always)]
unsafe fn dma_record_output_prepare(timestamp: i64, target: i32,
                                    words: usize) {
    // See gateware/rtio/dma.py.
    const HEADER_LENGTH: usize = /*length*/1 + /*channel*/3 + /*timestamp*/8 + /*address*/1;
    let length = HEADER_LENGTH + /*data*/words * 4;

    let buffer = &mut RECORDER.as_mut().unwrap().buffer;
    buffer.reserve(length);
    buffer.extend_from_slice(&[
        (length    >>  0) as u8,
        (target    >>  8) as u8,
        (target    >>  16) as u8,
        (target    >>  24) as u8,
        (timestamp >>  0) as u8,
        (timestamp >>  8) as u8,
        (timestamp >> 16) as u8,
        (timestamp >> 24) as u8,
        (timestamp >> 32) as u8,
        (timestamp >> 40) as u8,
        (timestamp >> 48) as u8,
        (timestamp >> 56) as u8,
        (target    >>  0) as u8,
    ]);
}

pub extern fn dma_record_output(target: i32, word: i32) {
    unsafe {
        let timestamp = rtio::now_mu();
        dma_record_output_prepare(timestamp, target, 1);
        RECORDER.as_mut().unwrap().buffer.extend_from_slice(&[
            (word >>  0) as u8,
            (word >>  8) as u8,
            (word >> 16) as u8,
            (word >> 24) as u8,
        ]);
    }
}

pub extern fn dma_record_output_wide(target: i32, words: &CSlice<i32>) {
    assert!(words.len() <= 16); // enforce the hardware limit

    unsafe {
        let timestamp = rtio::now_mu();
        dma_record_output_prepare(timestamp, target, words.len());
        let buffer = &mut RECORDER.as_mut().unwrap().buffer;
        for word in words.as_ref().iter() {
            buffer.extend_from_slice(&[
                (word >>  0) as u8,
                (word >>  8) as u8,
                (word >> 16) as u8,
                (word >> 24) as u8,
            ]);
        }
    }
}

pub extern fn dma_erase(name: CSlice<u8>) {
    let name = String::from_utf8(name.as_ref().to_vec()).unwrap();
    unsafe {
        KERNEL_CHANNEL_1TO0.as_mut().unwrap().send(Message::DmaEraseRequest(name));
    }
}

pub extern fn dma_retrieve(name: CSlice<u8>) -> DmaTrace {
    let name = String::from_utf8(name.as_ref().to_vec()).unwrap();
    unsafe {
        KERNEL_CHANNEL_1TO0.as_mut().unwrap().send(Message::DmaGetRequest(name));
    }
    match unsafe {KERNEL_CHANNEL_0TO1.as_mut().unwrap()}.recv() {
        Message::DmaGetReply(None) => (),
        Message::DmaGetReply(Some((mut v, duration))) => {
            v.reserve(ALIGNMENT - 1);
            let original_length = v.len();
            let padding = ALIGNMENT - v.as_ptr() as usize % ALIGNMENT;
            let padding = if padding == ALIGNMENT { 0 } else { padding };
            for _ in 0..padding {
                v.push(0);
            }
            // trailing zero to indicate end of buffer
            v.push(0);
            v.copy_within(0..original_length, padding);
            dcci_slice(&v);
            let v = Box::new(v);
            let address = Box::into_raw(v) as *mut Vec<u8> as i32;
            return DmaTrace {
                address,
                duration,
            };
        },
        _ => panic!("Expected DmaGetReply after DmaGetRequest!"),
    }
    // we have to defer raising error as we have to drop the message first...
    artiq_raise!("DMAError", "DMA trace not found");
}

pub extern fn dma_playback(timestamp: i64, ptr: i32) {
    unsafe {
        let v = Box::from_raw(ptr as *mut Vec<u8>);
        let padding = ALIGNMENT - v.as_ptr() as usize % ALIGNMENT;
        let padding = if padding == ALIGNMENT { 0 } else { padding };
        let ptr = v.as_ptr().add(padding) as i32;

        csr::rtio_dma::base_address_write(ptr as u32);
        csr::rtio_dma::time_offset_write(timestamp as u64);

        csr::cri_con::selected_write(1);
        csr::rtio_dma::enable_write(1);
        while csr::rtio_dma::enable_read() != 0 {}
        csr::cri_con::selected_write(0);

        // leave the handle as we may try to do playback for another time.
        mem::forget(v);

        let error = csr::rtio_dma::error_read();
        if error != 0 {
            let timestamp = csr::rtio_dma::error_timestamp_read();
            let channel = csr::rtio_dma::error_channel_read();
            csr::rtio_dma::error_write(1);
            if error & 1 != 0 {
                artiq_raise!("RTIOUnderflow",
                    "RTIO underflow at {0} mu, channel {1}",
                    timestamp as i64, channel as i64, 0);
            }
            if error & 2 != 0 {
                artiq_raise!("RTIODestinationUnreachable",
                    "RTIO destination unreachable, output, at {0} mu, channel {1}",
                    timestamp as i64, channel as i64, 0);
            }
        }
    }
}

