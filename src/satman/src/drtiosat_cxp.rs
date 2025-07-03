use alloc::format;

use libasync::task;
use libboard_artiq::{cxp_ctrl::DATA_MAXSIZE,
                     cxp_grabber, cxp_packet, drtioaux,
                     drtioaux::Packet,
                     drtioaux_async,
                     drtioaux_proto::{CXP_PAYLOAD_MAX_SIZE, CXP_PAYLOAD_MAX_SIZE_U64},
                     pl::csr};

static mut IDLE: bool = true;
static mut CXP_PACKET: Option<Packet> = None;

fn get_cxp_error_packet(s: &str) -> Packet {
    let err_msg = s.as_bytes();
    let length = err_msg.len();
    let mut message: [u8; CXP_PAYLOAD_MAX_SIZE] = [0; CXP_PAYLOAD_MAX_SIZE];
    message[..length].copy_from_slice(&err_msg);
    drtioaux::Packet::CXPError {
        length: length as u16,
        message,
    }
}

#[allow(static_mut_refs)]
pub async fn process_read_request(addr: u32, length: u16) -> Result<(), drtioaux::Error> {
    if !cxp_grabber::async_camera_connected().await {
        return drtioaux_async::send(0, &get_cxp_error_packet("Camera is not connected")).await;
    };
    unsafe {
        if CXP_PACKET.is_some() {
            let packet = CXP_PACKET.take().unwrap();
            return drtioaux_async::send(0, &packet).await;
        }
    }

    if unsafe { IDLE } {
        unsafe { IDLE = false };
        // CoaXPress CTRL packet allow a maximum of 10 seconds timeout - Section 9.6.3 (CXP-001-2021)
        // Spawn an async task to prevent blocking the whole main loop for 10 seconds and reply CXPWaitReply when the packet is not ready
        task::spawn(async move {
            let mut data: [u8; CXP_PAYLOAD_MAX_SIZE] = [0; CXP_PAYLOAD_MAX_SIZE];
            let mut address = addr;
            let mut bytesleft = length as usize;
            while bytesleft > 0 {
                let read_len = DATA_MAXSIZE.min(bytesleft);
                let offset = length as usize - bytesleft;

                if let Err(e) = cxp_packet::async_read_bytes(
                    address,
                    &mut data[offset..(offset + read_len)],
                    cxp_grabber::async_with_tag().await,
                )
                .await
                {
                    unsafe { CXP_PACKET = Some(get_cxp_error_packet(&format!("{}", e))) };
                    return;
                };

                address += read_len as u32;
                bytesleft -= read_len;
            }
            unsafe {
                CXP_PACKET = Some(Packet::CXPReadReply { length, data });
                IDLE = true;
            };
        });
    }
    drtioaux_async::send(0, &drtioaux::Packet::CXPWaitReply).await
}

#[allow(static_mut_refs)]
pub async fn process_write32_request(addr: u32, val: u32) -> Result<(), drtioaux::Error> {
    if !cxp_grabber::async_camera_connected().await {
        return drtioaux_async::send(0, &get_cxp_error_packet("Camera is not connected")).await;
    };
    unsafe {
        if CXP_PACKET.is_some() {
            let packet = CXP_PACKET.take().unwrap();
            return drtioaux_async::send(0, &packet).await;
        }

        if IDLE {
            IDLE = false;
            task::spawn(async move {
                match cxp_packet::async_write_u32(addr, val, cxp_grabber::async_with_tag().await).await {
                    Err(e) => CXP_PACKET = Some(get_cxp_error_packet(&format!("{}", e))),
                    Ok(()) => CXP_PACKET = Some(drtioaux::Packet::CXPWrite32Reply),
                }
                IDLE = true;
            });
        }
    }
    drtioaux_async::send(0, &drtioaux::Packet::CXPWaitReply).await
}

pub async fn process_roi_viewer_setup_request(x0: u16, y0: u16, x1: u16, y1: u16) -> Result<(), drtioaux::Error> {
    cxp_grabber::roi_viewer_setup(x0, y0, x1, y1);
    drtioaux_async::send(0, &drtioaux::Packet::CXPROIViewerSetupReply).await
}

pub async fn process_roi_viewer_data_request() -> Result<(), drtioaux::Error> {
    unsafe {
        if csr::cxp_grabber::roi_viewer_ready_read() == 0 {
            return drtioaux_async::send(0, &drtioaux::Packet::CXPWaitReply).await;
        }

        if csr::cxp_grabber::roi_viewer_fifo_stb_read() == 0 {
            // clear ready CSR when FIFO is empty
            csr::cxp_grabber::roi_viewer_ready_write(1);
            let width = csr::cxp_grabber::roi_viewer_x1_read() - csr::cxp_grabber::roi_viewer_x0_read();
            let height = csr::cxp_grabber::roi_viewer_y1_read() - csr::cxp_grabber::roi_viewer_y0_read();
            let pixel_code = csr::cxp_grabber::stream_decoder_pixel_format_code_read();
            return drtioaux_async::send(
                0,
                &drtioaux::Packet::CXPROIViewerFrameDataReply {
                    width,
                    height,
                    pixel_code,
                },
            )
            .await;
        }

        let mut data: [u64; CXP_PAYLOAD_MAX_SIZE_U64] = [0; CXP_PAYLOAD_MAX_SIZE_U64];
        let mut i = 0;
        while i != CXP_PAYLOAD_MAX_SIZE_U64 && csr::cxp_grabber::roi_viewer_fifo_stb_read() == 1 {
            data[i] = csr::cxp_grabber::roi_viewer_fifo_data_read();
            i += 1;
            csr::cxp_grabber::roi_viewer_fifo_ack_write(1);
        }

        drtioaux_async::send(
            0,
            &drtioaux::Packet::CXPROIViewerPixelDataReply { length: i as u16, data },
        )
        .await
    }
}
