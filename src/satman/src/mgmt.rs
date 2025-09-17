use alloc::vec::Vec;

use byteorder::{ByteOrder, NativeEndian};
use core_io::Write;
use crc::crc32;
use io::ProtoRead;
use libboard_artiq::{drtioaux_proto::SAT_PAYLOAD_MAX_SIZE,
                     logger::{BufferLogger, LogBufferRef}};
use log::{LevelFilter, debug, error, info, warn};

use crate::routing::{SliceMeta, Sliceable};

type Result<T> = core::result::Result<T, ()>;

fn get_logger_buffer() -> LogBufferRef<'static> {
    let logger = BufferLogger::get_logger();
    loop {
        if let Some(buffer_ref) = logger.buffer() {
            return buffer_ref;
        }
    }
}

pub fn clear_log() {
    let mut buffer = get_logger_buffer();
    buffer.clear();
}

pub struct Manager {
    last_log: Sliceable,
    config_payload: Vec<u8>,
    last_value: Sliceable,
    image_payload: Vec<u8>,
}

impl Manager {
    pub fn new() -> Manager {
        Manager {
            last_log: Sliceable::new(0, Vec::new()),
            config_payload: Vec::new(),
            last_value: Sliceable::new(0, Vec::new()),
            image_payload: Vec::new(),
        }
    }

    pub fn log_get_slice(&mut self, data_slice: &mut [u8; SAT_PAYLOAD_MAX_SIZE], consume: bool) -> SliceMeta {
        // Populate buffer if depleted
        if self.last_log.at_end() {
            let mut buffer = get_logger_buffer();
            self.last_log.extend(buffer.extract().as_bytes());
            if consume {
                buffer.clear();
            }
        }

        self.last_log.get_slice_satellite(data_slice)
    }

    pub fn fetch_config_value(&mut self, key: &str) -> Result<()> {
        libconfig::read(&key)
            .map(|value| {
                debug!("got value");
                self.last_value = Sliceable::new(0, value)
            })
            .map_err(|_| warn!("read error: no such key"))
    }

    pub fn get_config_value_slice(&mut self, data_slice: &mut [u8; SAT_PAYLOAD_MAX_SIZE]) -> SliceMeta {
        self.last_value.get_slice_satellite(data_slice)
    }

    pub fn add_config_data(&mut self, data: &[u8], data_len: usize) {
        self.config_payload.write_all(&data[..data_len]).unwrap();
    }

    pub fn clear_config_data(&mut self) {
        self.config_payload.clear();
    }

    pub fn write_config(&mut self) -> Result<()> {
        let mut payload = &self.config_payload[..];
        let key = payload
            .read_string::<NativeEndian>()
            .map_err(|_err| error!("error on reading key"))?;
        debug!("write key: {}", key);
        let value = payload.read_bytes::<NativeEndian>().unwrap();

        let mut delay_set_flag = false;
        if key == "log_level" || key == "uart_log_level" {
            let value_str = core::str::from_utf8(&value).map_err(|err| error!("invalid UTF_8: {:?}", err))?;
            let max_level = value_str
                .parse::<LevelFilter>()
                .map_err(|err| error!("unknown log level: {:?}", err))?;

            if key == "log_level" {
                info!("Changing log level to {}", max_level);
                BufferLogger::get_logger().set_buffer_log_level(max_level);
            } else {
                if max_level == LevelFilter::Trace {
                    delay_set_flag = true;
                    BufferLogger::get_logger().set_uart_log_level(LevelFilter::Debug);
                } else {
                    info!("Changing UART log level to {}", max_level);
                    BufferLogger::get_logger().set_uart_log_level(max_level);
                }
            }
        };

        libconfig::write(&key, value)
            .map(|()| debug!("write success"))
            .map_err(|err| error!("failed to write: {:?}", err))?;

        if delay_set_flag {
            info!("Changing UART log level to {}", LevelFilter::Trace);
            BufferLogger::get_logger().set_uart_log_level(LevelFilter::Trace);
        }
        Ok(())
    }

    pub fn remove_config(&mut self, key: &str) -> Result<()> {
        debug!("erase key: {}", key);
        libconfig::remove(&key)
            .map(|()| debug!("erase success"))
            .map_err(|err| warn!("failed to erase: {:?}", err))
    }

    pub fn allocate_image_buffer(&mut self, image_size: usize) {
        self.image_payload = Vec::with_capacity(image_size);
    }

    pub fn add_image_data(&mut self, data: &[u8], data_len: usize) {
        self.image_payload.extend(&data[..data_len]);
    }

    pub fn write_image(&self) {
        let mut image = self.image_payload.clone();
        let image_ref = &image[..];
        let bin_len = image.len() - 4;

        let (image_ref, expected_crc) = {
            let (image_ref, crc_slice) = image_ref.split_at(bin_len);
            (image_ref, NativeEndian::read_u32(crc_slice))
        };

        let actual_crc = crc32::checksum_ieee(image_ref);

        if actual_crc == expected_crc {
            info!("CRC passed. Writing boot image to SD card...");
            image.truncate(bin_len);
            libconfig::write("boot", image).expect("failed to write boot image");
        } else {
            panic!(
                "CRC failed, images have not been written to flash.\n(actual {:08x}, expected {:08x})",
                actual_crc, expected_crc
            );
        }
    }
}
