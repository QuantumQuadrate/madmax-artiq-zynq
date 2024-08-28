use alloc::vec::Vec;

use io::{Cursor, ProtoRead, ProtoWrite};
use libboard_artiq::{drtioaux_proto::SAT_PAYLOAD_MAX_SIZE,
                     logger::{BufferLogger, LogBufferRef}};
use libconfig::Config;
use log::{self, debug, error, info, warn, LevelFilter};

use crate::routing::{SliceMeta, Sliceable};

type Result<T> = core::result::Result<T, ()>;

pub fn byte_to_level_filter(level_byte: u8) -> Result<log::LevelFilter> {
    Ok(match level_byte {
        0 => log::LevelFilter::Off,
        1 => log::LevelFilter::Error,
        2 => log::LevelFilter::Warn,
        3 => log::LevelFilter::Info,
        4 => log::LevelFilter::Debug,
        5 => log::LevelFilter::Trace,
        lv => {
            error!("unknown log level: {}", lv);
            return Err(());
        }
    })
}

fn get_logger_buffer_pred() -> LogBufferRef<'static> {
    let logger = unsafe { BufferLogger::get_logger().as_mut().unwrap() };
    loop {
        if let Some(buffer_ref) = logger.buffer() {
            return buffer_ref;
        }
    }
}

fn get_logger_buffer() -> LogBufferRef<'static> {
    get_logger_buffer_pred()
}

pub fn clear_log() {
    let mut buffer = get_logger_buffer();
    buffer.clear();
}

pub struct Manager<'a> {
    cfg: &'a mut Config,
    last_log: Sliceable,
    current_payload: Cursor<Vec<u8>>,
    last_value: Sliceable,
}

impl<'a> Manager<'_> {
    pub fn new(cfg: &mut Config) -> Manager {
        Manager {
            cfg: cfg,
            last_log: Sliceable::new(0, Vec::new()),
            current_payload: Cursor::new(Vec::new()),
            last_value: Sliceable::new(0, Vec::new()),
        }
    }

    pub fn log_get_slice(&mut self, data_slice: &mut [u8; SAT_PAYLOAD_MAX_SIZE]) -> SliceMeta {
        // Populate buffer if depleted
        if self.last_log.at_end() {
            self.last_log.extend(get_logger_buffer().extract().as_bytes());
        }

        self.last_log.get_slice_satellite(data_slice)
    }

    pub fn fetch_config_value(&mut self, key: &str) -> Result<()> {
        self.cfg
            .read(&key)
            .map(|value| {
                debug!("got value");
                self.last_value = Sliceable::new(0, value)
            })
            .map_err(|_| warn!("read error: no such key"))
    }

    pub fn get_config_value_slice(&mut self, data_slice: &mut [u8; SAT_PAYLOAD_MAX_SIZE]) -> SliceMeta {
        self.last_value.get_slice_satellite(data_slice)
    }

    pub fn add_data(&mut self, data: &[u8], data_len: usize) {
        self.current_payload.write_all(&data[..data_len]).unwrap();
    }

    pub fn clear_data(&mut self) {
        self.current_payload.get_mut().clear();
        self.current_payload.set_position(0);
    }

    pub fn write_config(&mut self) -> Result<()> {
        let key = self
            .current_payload
            .read_string()
            .map_err(|_err| error!("error on reading key"))?;
        debug!("write key: {}", key);
        let value = self.current_payload.read_bytes().unwrap();

        self.cfg
            .write(&key, value)
            .map(|()| debug!("write success"))
            .map_err(|err| error!("failed to write: {:?}", err))
    }

    pub fn remove_config(&mut self, key: &str) -> Result<()> {
        debug!("erase key: {}", key);
        self.cfg
            .remove(&key)
            .map(|()| debug!("erase success"))
            .map_err(|err| warn!("failed to erase: {:?}", err))
    }
}
