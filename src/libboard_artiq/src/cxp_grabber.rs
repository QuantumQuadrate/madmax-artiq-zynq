use libboard_zynq::i2c;
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

pub async fn tick(_i2c: &mut i2c::I2c) {
    let mut state_guard = STATE.lock();
    let mut with_tag_guard = WITH_TAG.lock();
    *state_guard = match *state_guard {
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
                    *with_tag_guard = with_tag;
                    State::Connected
                }
                Err(e) => {
                    error!("camera setup failure: {}", e);
                    *with_tag_guard = false;
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
                *with_tag_guard = false;
                info!("camera disconnected");
                State::Disconnected
            }
        }
    };
}
