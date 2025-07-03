use libboard_zynq::{i2c, timer};
use libcortex_a9::mutex::Mutex;
use log::{error, info};

#[cfg(has_cxp_led)]
use crate::cxp_led::{LEDState, update_led};
use crate::{cxp_camera_setup::{camera_setup, discover_camera, master_channel_ready},
            pl::csr};

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Connected,
    Detected,
    Disconnected,
}

// Mutex as they are needed by core1 cxp api calls
static STATE: Mutex<State> = Mutex::new(State::Disconnected);
static WITH_TAG: Mutex<bool> = Mutex::new(false);

pub fn camera_connected() -> bool {
    *STATE.lock() == State::Connected
}

pub fn with_tag() -> bool {
    *WITH_TAG.lock()
}

pub async fn async_camera_connected() -> bool {
    *STATE.async_lock().await == State::Connected
}

pub async fn async_with_tag() -> bool {
    *WITH_TAG.async_lock().await
}

pub async fn thread(i2c: &mut i2c::I2c) {
    loop {
        tick(i2c).await;
        timer::async_delay_ms(200).await;
    }
}

async fn tick(_i2c: &mut i2c::I2c) {
    // Get the value and drop the mutexguard to prevent blocking other async task that need to use it
    let current_state = { *STATE.async_lock().await };
    let next_state = match current_state {
        State::Disconnected => {
            #[cfg(has_cxp_led)]
            update_led(_i2c, LEDState::RedFlash1Hz);
            match discover_camera().await {
                Ok(_) => {
                    info!("camera detected, setting up camera...");
                    State::Detected
                }
                Err(_) => State::Disconnected,
            }
        }
        State::Detected => {
            #[cfg(has_cxp_led)]
            update_led(_i2c, LEDState::OrangeFlash12Hz5);
            match camera_setup().await {
                Ok(with_tag) => {
                    info!("camera setup complete");
                    *WITH_TAG.async_lock().await = with_tag;
                    State::Connected
                }
                Err(e) => {
                    error!("camera setup failure: {}", e);
                    *WITH_TAG.async_lock().await = false;
                    State::Disconnected
                }
            }
        }
        State::Connected => {
            #[cfg(has_cxp_led)]
            update_led(_i2c, LEDState::GreenSolid);
            if master_channel_ready() {
                unsafe {
                    if csr::cxp_grabber::stream_decoder_crc_error_read() == 1 {
                        error!("frame packet has CRC error");
                        csr::cxp_grabber::stream_decoder_crc_error_write(1);
                    };

                    if csr::cxp_grabber::stream_decoder_stream_type_error_read() == 1 {
                        error!("Non CoaXPress stream type detected, the CXP grabber doesn't support GenDC stream type");
                        csr::cxp_grabber::stream_decoder_stream_type_error_write(1);
                    };

                    if csr::cxp_grabber::core_rx_trigger_ack_read() == 1 {
                        info!("received CXP linktrigger ack");
                        csr::cxp_grabber::core_rx_trigger_ack_write(1);
                    };

                    if csr::cxp_grabber::stream_decoder_new_frame_read() == 1 {
                        let width = csr::cxp_grabber::stream_decoder_x_size_read();
                        let height = csr::cxp_grabber::stream_decoder_y_size_read();
                        match csr::cxp_grabber::stream_decoder_pixel_format_code_read() {
                            0x0101 => info!("received frame: {}x{} with MONO8 format", width, height),
                            0x0102 => info!("received frame: {}x{} with MONO10 format", width, height),
                            0x0103 => info!("received frame: {}x{} with MONO12 format", width, height),
                            0x0104 => info!("received frame: {}x{} with MONO14 format", width, height),
                            0x0105 => info!("received frame: {}x{} with MONO16 format", width, height),
                            _ => info!("received frame: {}x{} with Unsupported pixel format", width, height),
                        };
                        csr::cxp_grabber::stream_decoder_new_frame_write(1);
                    };
                }
                State::Connected
            } else {
                *WITH_TAG.async_lock().await = false;
                info!("camera disconnected");
                State::Disconnected
            }
        }
    };
    {
        *STATE.async_lock().await = next_state
    };
}

pub fn roi_viewer_setup(x0: u16, y0: u16, x1: u16, y1: u16) {
    unsafe {
        // flush the fifo before arming
        while csr::cxp_grabber::roi_viewer_fifo_stb_read() == 1 {
            csr::cxp_grabber::roi_viewer_fifo_ack_write(1);
        }
        csr::cxp_grabber::roi_viewer_x0_write(x0);
        csr::cxp_grabber::roi_viewer_x1_write(x1);
        csr::cxp_grabber::roi_viewer_y0_write(y0);
        csr::cxp_grabber::roi_viewer_y1_write(y1);
        csr::cxp_grabber::roi_viewer_arm_write(1);
    }
}
