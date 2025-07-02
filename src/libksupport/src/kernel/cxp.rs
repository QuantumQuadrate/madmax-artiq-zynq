use alloc::{string::{String, ToString},
            vec::Vec};
use core::fmt;

use byteorder::{ByteOrder, NetworkEndian};
use cslice::CMutSlice;
use libboard_artiq::{cxp_ctrl::{DATA_MAXSIZE, Error as CtrlErr},
                     cxp_grabber::{camera_connected, roi_viewer_setup, with_tag},
                     cxp_packet::{read_bytes, read_u32, write_u32}};
use log::info;

use crate::{artiq_raise, pl::csr::cxp_grabber};

const ROI_MAX_SIZE: usize = 4096;

#[repr(C)]
pub struct ROIViewerFrame {
    width: i32,
    height: i32,
    pixel_width: i32,
}

enum Error {
    BufferSizeTooSmall(usize, usize),
    ROISizeTooBig(usize, usize),
    InvalidLocalUrl(String),
    CtrlPacketError(CtrlErr),
}

impl From<CtrlErr> for Error {
    fn from(value: CtrlErr) -> Error {
        Error::CtrlPacketError(value)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Error::BufferSizeTooSmall(required_size, buffer_size) => {
                write!(
                    f,
                    "BufferSizeTooSmall - The required size is {} bytes but the buffer size is {} bytes",
                    required_size, buffer_size
                )
            }
            &Error::ROISizeTooBig(width, height) => {
                write!(
                    f,
                    "ROISizeTooBig - The maximum ROIViewer height and total size are {} and {} pixels respectively \
                     but the ROI is set to {} ({}x{}) pixels",
                    ROI_MAX_SIZE / 4,
                    ROI_MAX_SIZE,
                    width * height,
                    width,
                    height
                )
            }
            &Error::InvalidLocalUrl(ref s) => {
                write!(f, "InvalidLocalUrl - Cannot download xml file locally from {}", s)
            }
            &Error::CtrlPacketError(ref err) => write!(f, "{}", err),
        }
    }
}

fn read_xml_url(with_tag: bool) -> Result<String, Error> {
    let mut addr = read_u32(0x0018, with_tag)?;
    let mut buffer = Vec::new();

    // Strings stored in the bootstrap and manufacturer-specific registers space shall be NULL-terminated, encoded ASCII - Section 12.3.1 (CXP-001-2021)
    // String length is not known during runtime, grabber must read 4 bytes at a time until NULL-terminated
    loop {
        let mut bytes: [u8; 4] = [0; 4];
        read_bytes(addr, &mut bytes, with_tag)?;
        addr += 4;

        for b in bytes {
            if b == 0 {
                // UTF-8 is compatible with ASCII encoding
                // use U+FFFD REPLACEMENT_CHARACTER to represent decoding error
                return Ok(String::from_utf8_lossy(&buffer).to_string());
            } else {
                buffer.push(b);
            }
        }
    }
}

fn read_xml_location(with_tag: bool) -> Result<(String, u32, u32), Error> {
    let url = read_xml_url(with_tag)?;

    // url example - Section 13.2.3 (CXP-001-2021)
    // Available on camera - "Local:MyFilename.zip;B8000;33A?SchemaVersion=1.0.0"
    // => ZIP file starting at address 0xB8000 in the Device with a length of 0x33A bytes
    //
    // Available online - "Web:http://www.example.com/xml/MyFilename.xml"
    // => xml is available at http://www.example.com/xml/MyFilename.xml
    let mut splitter = url.split(|c| c == ':' || c == ';' || c == '?');
    let scheme = splitter.next().unwrap();
    if scheme.eq_ignore_ascii_case("local") {
        if let (Some(file_name), Some(addr_str), Some(size_str)) = (splitter.next(), splitter.next(), splitter.next()) {
            let addr = u32::from_str_radix(addr_str, 16).map_err(|_| Error::InvalidLocalUrl(url.to_string()))?;
            let size = u32::from_str_radix(size_str, 16).map_err(|_| Error::InvalidLocalUrl(url.to_string()))?;
            return Ok((file_name.to_string(), addr, size));
        }
    }
    Err(Error::InvalidLocalUrl(url.to_string()))
}

fn read_xml_file(buffer: &mut [i32], with_tag: bool) -> Result<u32, Error> {
    let (file_name, base_addr, size) = read_xml_location(with_tag)?;

    if buffer.len() * 4 < size as usize {
        return Err(Error::BufferSizeTooSmall(size as usize, buffer.len() * 4));
    };

    info!("downloading xml file {} with {} bytes...", file_name, size);
    let mut v: Vec<u8> = Vec::new();
    let mut addr = base_addr;
    let mut bytesleft = size;
    let mut bytes: [u8; DATA_MAXSIZE] = [0; DATA_MAXSIZE];

    while bytesleft > 0 {
        let read_len = DATA_MAXSIZE.min(bytesleft as usize);
        read_bytes(addr, &mut bytes[..read_len], with_tag)?;
        v.extend(&bytes[..read_len]);
        addr += read_len as u32;
        bytesleft -= read_len as u32;
    }
    info!("download successful");

    // pad to 32 bit boundary
    let padding = (4 - (size % 4)) % 4;
    for _ in 0..padding {
        v.push(0);
    }

    NetworkEndian::read_i32_into(&v, &mut buffer[..((size + padding) / 4) as usize]);
    Ok((size + padding) / 4)
}

pub extern "C" fn download_xml_file(buffer: &mut CMutSlice<i32>) -> i32 {
    if camera_connected() {
        match read_xml_file(buffer.as_mut_slice(), with_tag()) {
            Ok(size_read) => size_read as i32,
            Err(e) => artiq_raise!("CXPError", format!("{}", e)),
        }
    } else {
        artiq_raise!("CXPError", "Camera is not connected");
    }
}

pub extern "C" fn read32(addr: i32) -> i32 {
    if camera_connected() {
        match read_u32(addr as u32, with_tag()) {
            Ok(result) => result as i32,
            Err(e) => artiq_raise!("CXPError", format!("{}", e)),
        }
    } else {
        artiq_raise!("CXPError", "Camera is not connected");
    }
}

pub extern "C" fn write32(addr: i32, val: i32) {
    if camera_connected() {
        match write_u32(addr as u32, val as u32, with_tag()) {
            Ok(_) => {}
            Err(e) => artiq_raise!("CXPError", format!("{}", e)),
        }
    } else {
        artiq_raise!("CXPError", "Camera is not connected");
    }
}

pub extern "C" fn start_roi_viewer(x0: i32, y0: i32, x1: i32, y1: i32) {
    let (width, height) = ((x1 - x0) as usize, (y1 - y0) as usize);
    if width * height > ROI_MAX_SIZE || height > ROI_MAX_SIZE / 4 {
        artiq_raise!("CXPError", format!("{}", Error::ROISizeTooBig(width, height)));
    } else {
        roi_viewer_setup(x0 as u16, y0 as u16, x1 as u16, y1 as u16);
    }
}

pub extern "C" fn download_roi_viewer_frame(buffer: &mut CMutSlice<i64>) -> ROIViewerFrame {
    if buffer.len() * 4 < ROI_MAX_SIZE {
        // each pixel is 16 bits
        artiq_raise!(
            "CXPError",
            format!("{}", Error::BufferSizeTooSmall(ROI_MAX_SIZE * 2, buffer.len() * 8))
        );
    };

    let buf = buffer.as_mut_slice();
    unsafe {
        while cxp_grabber::roi_viewer_ready_read() == 0 {}
        cxp_grabber::roi_viewer_ready_write(1);

        let mut i = 0;
        while cxp_grabber::roi_viewer_fifo_stb_read() == 1 {
            buf[i] = cxp_grabber::roi_viewer_fifo_data_read() as i64;
            i += 1;
            cxp_grabber::roi_viewer_fifo_ack_write(1);
        }
        let width = (cxp_grabber::roi_viewer_x1_read() - cxp_grabber::roi_viewer_x0_read()) as i32;
        let height = (cxp_grabber::roi_viewer_y1_read() - cxp_grabber::roi_viewer_y0_read()) as i32;
        let pixel_width = match cxp_grabber::stream_decoder_pixel_format_code_read() {
            0x0101 => 8,
            0x0102 => 10,
            0x0103 => 12,
            0x0104 => 14,
            0x0105 => 16,
            _ => artiq_raise!("CXPError", "UnsupportedPixelFormat"),
        };
        ROIViewerFrame {
            width,
            height,
            pixel_width,
        }
    }
}
