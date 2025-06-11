use core::mem::MaybeUninit;

use libboard_zynq::i2c::{Error, I2c};

#[cfg(has_drtio)]
use super::{KERNEL_CHANNEL_0TO1, KERNEL_CHANNEL_1TO0, Message};
use crate::artiq_raise;

static mut I2C_BUS: MaybeUninit<I2c> = MaybeUninit::uninit();

pub extern "C" fn start(busno: i32) {
    let _destination = (busno >> 16) as u8;
    #[cfg(has_drtio)]
    if _destination != 0 {
        let reply = unsafe {
            KERNEL_CHANNEL_1TO0
                .as_mut()
                .unwrap()
                .send(Message::I2cStartRequest(busno as u32));
            KERNEL_CHANNEL_0TO1.as_mut().unwrap().recv()
        };
        match reply {
            Message::I2cBasicReply(true) => return,
            Message::I2cBasicReply(false) => artiq_raise!("I2CError", "I2C remote start fail"),
            msg => panic!("Expected I2cBasicReply for I2cStartRequest, got: {:?}", msg),
        }
    }
    if busno > 0 {
        artiq_raise!("I2CError", "I2C bus could not be accessed");
    }
    if get_bus().start().is_err() {
        artiq_raise!("I2CError", "I2C start failed");
    }
}

pub extern "C" fn restart(busno: i32) {
    let _destination = (busno >> 16) as u8;
    #[cfg(has_drtio)]
    if _destination != 0 {
        let reply = unsafe {
            KERNEL_CHANNEL_1TO0
                .as_mut()
                .unwrap()
                .send(Message::I2cRestartRequest(busno as u32));
            KERNEL_CHANNEL_0TO1.as_mut().unwrap().recv()
        };
        match reply {
            Message::I2cBasicReply(true) => return,
            Message::I2cBasicReply(false) => artiq_raise!("I2CError", "I2C remote restart fail"),
            msg => panic!("Expected I2cBasicReply for I2cRetartRequest, got: {:?}", msg),
        }
    }
    if busno > 0 {
        artiq_raise!("I2CError", "I2C bus could not be accessed");
    }
    if get_bus().restart().is_err() {
        artiq_raise!("I2CError", "I2C restart failed");
    }
}

pub extern "C" fn stop(busno: i32) {
    let _destination = (busno >> 16) as u8;
    #[cfg(has_drtio)]
    if _destination != 0 {
        // remote
        let reply = unsafe {
            KERNEL_CHANNEL_1TO0
                .as_mut()
                .unwrap()
                .send(Message::I2cStopRequest(busno as u32));
            KERNEL_CHANNEL_0TO1.as_mut().unwrap().recv()
        };
        match reply {
            Message::I2cBasicReply(true) => return,
            Message::I2cBasicReply(false) => artiq_raise!("I2CError", "I2C remote stop fail"),
            msg => panic!("Expected I2cBasicReply for I2cStopRequest, got: {:?}", msg),
        }
    }
    if busno > 0 {
        artiq_raise!("I2CError", "I2C bus could not be accessed");
    }
    if get_bus().stop().is_err() {
        artiq_raise!("I2CError", "I2C stop failed");
    }
}

pub extern "C" fn write(busno: i32, data: i32) -> bool {
    let _destination = (busno >> 16) as u8;
    #[cfg(has_drtio)]
    if _destination != 0 {
        // remote
        let reply = unsafe {
            KERNEL_CHANNEL_1TO0.as_mut().unwrap().send(Message::I2cWriteRequest {
                busno: busno as u32,
                data: data as u8,
            });
            KERNEL_CHANNEL_0TO1.as_mut().unwrap().recv()
        };
        match reply {
            Message::I2cWriteReply { succeeded: true, ack } => return ack,
            Message::I2cWriteReply { succeeded: false, .. } => artiq_raise!("I2CError", "I2C remote write fail"),
            msg => panic!("Expected I2cWriteReply for I2cWriteRequest, got: {:?}", msg),
        }
    }
    if busno > 0 {
        artiq_raise!("I2CError", "I2C bus could not be accessed");
    }
    match get_bus().write(data as u8) {
        Ok(()) => true,
        Err(Error::Nack) => false,
        Err(_) => artiq_raise!("I2CError", "I2C write failed"),
    }
}

pub extern "C" fn read(busno: i32, ack: bool) -> i32 {
    let _destination = (busno >> 16) as u8;
    #[cfg(has_drtio)]
    if _destination != 0 {
        let reply = unsafe {
            KERNEL_CHANNEL_1TO0.as_mut().unwrap().send(Message::I2cReadRequest {
                busno: busno as u32,
                ack,
            });
            KERNEL_CHANNEL_0TO1.as_mut().unwrap().recv()
        };
        match reply {
            Message::I2cReadReply { succeeded: true, data } => return data as i32,
            Message::I2cReadReply { succeeded: false, .. } => artiq_raise!("I2CError", "I2C remote read fail"),
            msg => panic!("Expected I2cReadReply for I2cReadRequest, got: {:?}", msg),
        }
    }
    if busno > 0 {
        artiq_raise!("I2CError", "I2C bus could not be accessed");
    }
    match get_bus().read(ack) {
        Ok(r) => r as i32,
        Err(_) => artiq_raise!("I2CError", "I2C read failed"),
    }
}

pub extern "C" fn switch_select(busno: i32, address: i32, mask: i32) {
    let _destination = (busno >> 16) as u8;
    #[cfg(has_drtio)]
    if _destination != 0 {
        // remote
        let reply = unsafe {
            KERNEL_CHANNEL_1TO0
                .as_mut()
                .unwrap()
                .send(Message::I2cSwitchSelectRequest {
                    busno: busno as u32,
                    address: address as u8,
                    mask: mask as u8,
                });
            KERNEL_CHANNEL_0TO1.as_mut().unwrap().recv()
        };
        match reply {
            Message::I2cBasicReply(true) => return,
            Message::I2cBasicReply(false) => artiq_raise!("I2CError", "I2C remote start fail"),
            msg => panic!("Expected I2cBasicReply for I2cSwitchSelectRequest, got: {:?}", msg),
        }
    }
    if busno > 0 {
        artiq_raise!("I2CError", "I2C bus could not be accessed");
    }
    let ch = match mask {
        //decode from mainline, PCA9548-centric API
        0x00 => None,
        0x01 => Some(0),
        0x02 => Some(1),
        0x04 => Some(2),
        0x08 => Some(3),
        0x10 => Some(4),
        0x20 => Some(5),
        0x40 => Some(6),
        0x80 => Some(7),
        _ => artiq_raise!("I2CError", "switch select supports only one channel"),
    };
    if get_bus().pca954x_select(address as u8, ch).is_err() {
        artiq_raise!("I2CError", "switch select failed");
    }
}

pub fn init() {
    let mut i2c = I2c::i2c0();
    i2c.init().expect("I2C bus initialization failed");
    unsafe { I2C_BUS.write(i2c) };
}

pub fn get_bus() -> &'static mut I2c {
    unsafe { I2C_BUS.assume_init_mut() }
}
