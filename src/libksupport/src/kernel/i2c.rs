use super::{KERNEL_CHANNEL_0TO1, KERNEL_CHANNEL_1TO0, Message};
use crate::artiq_raise;

pub extern "C" fn start(busno: i32) {
    let reply = unsafe {
        KERNEL_CHANNEL_1TO0
            .as_mut()
            .unwrap()
            .send(Message::I2cStartRequest(busno as u32));
        KERNEL_CHANNEL_0TO1.as_mut().unwrap().recv()
    };
    match reply {
        Message::I2cBasicReply(true) => (),
        Message::I2cBasicReply(false) => artiq_raise!("I2CError", "I2C start fail"),
        msg => panic!("Expected I2cBasicReply for I2cStartRequest, got: {:?}", msg),
    }
}

pub extern "C" fn restart(busno: i32) {
    let reply = unsafe {
        KERNEL_CHANNEL_1TO0
            .as_mut()
            .unwrap()
            .send(Message::I2cRestartRequest(busno as u32));
        KERNEL_CHANNEL_0TO1.as_mut().unwrap().recv()
    };
    match reply {
        Message::I2cBasicReply(true) => (),
        Message::I2cBasicReply(false) => artiq_raise!("I2CError", "I2C restart fail"),
        msg => panic!("Expected I2cBasicReply for I2cRestartRequest, got: {:?}", msg),
    }
}

pub extern "C" fn stop(busno: i32) {
    let reply = unsafe {
        KERNEL_CHANNEL_1TO0
            .as_mut()
            .unwrap()
            .send(Message::I2cStopRequest(busno as u32));
        KERNEL_CHANNEL_0TO1.as_mut().unwrap().recv()
    };
    match reply {
        Message::I2cBasicReply(true) => (),
        Message::I2cBasicReply(false) => artiq_raise!("I2CError", "I2C stop fail"),
        msg => panic!("Expected I2cBasicReply for I2cStopRequest, got: {:?}", msg),
    }
}

pub extern "C" fn write(busno: i32, data: i32) -> bool {
    let reply = unsafe {
        KERNEL_CHANNEL_1TO0.as_mut().unwrap().send(Message::I2cWriteRequest {
            busno: busno as u32,
            data: data as u8,
        });
        KERNEL_CHANNEL_0TO1.as_mut().unwrap().recv()
    };
    match reply {
        Message::I2cWriteReply { succeeded: true, ack } => ack,
        Message::I2cWriteReply { succeeded: false, .. } => artiq_raise!("I2CError", "I2C write fail"),
        msg => panic!("Expected I2cWriteReply for I2cWriteRequest, got: {:?}", msg),
    }
}

pub extern "C" fn read(busno: i32, ack: bool) -> i32 {
    let reply = unsafe {
        KERNEL_CHANNEL_1TO0.as_mut().unwrap().send(Message::I2cReadRequest {
            busno: busno as u32,
            ack,
        });
        KERNEL_CHANNEL_0TO1.as_mut().unwrap().recv()
    };
    match reply {
        Message::I2cReadReply { succeeded: true, data } => data as i32,
        Message::I2cReadReply { succeeded: false, .. } => artiq_raise!("I2CError", "I2C read fail"),
        msg => panic!("Expected I2cReadReply for I2cReadRequest, got: {:?}", msg),
    }
}

pub extern "C" fn switch_select(busno: i32, address: i32, mask: i32) {
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
        Message::I2cBasicReply(true) => (),
        Message::I2cBasicReply(false) => artiq_raise!("I2CError", "I2C switch select fail"),
        msg => panic!("Expected I2cBasicReply for I2cSwitchSelectRequest, got: {:?}", msg),
    }
}
