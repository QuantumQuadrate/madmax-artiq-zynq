use core::{cell::Cell, fmt::Write};

use libboard_zynq::{println, stdio, timer};
use libcortex_a9::{mutex::{Mutex, MutexGuard},
                   once_lock::OnceLock};
use log::{LevelFilter, Log};
use log_buffer::LogBuffer;

pub struct LogBufferRef<'a> {
    buffer: MutexGuard<'a, LogBuffer<&'static mut [u8]>>,
    old_log_level: LevelFilter,
}

impl<'a> LogBufferRef<'a> {
    fn new(buffer: MutexGuard<'a, LogBuffer<&'static mut [u8]>>) -> LogBufferRef<'a> {
        let old_log_level = BufferLogger::get_logger().buffer_log_level();
        BufferLogger::get_logger().set_buffer_log_level(LevelFilter::Off);
        LogBufferRef { buffer, old_log_level }
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn clear(&mut self) {
        self.buffer.clear()
    }

    pub fn extract(&mut self) -> &str {
        self.buffer.extract()
    }
}

impl<'a> Drop for LogBufferRef<'a> {
    fn drop(&mut self) {
        BufferLogger::get_logger().set_buffer_log_level(self.old_log_level);
    }
}

pub struct BufferLogger {
    buffer: Mutex<LogBuffer<&'static mut [u8]>>,
    uart_filter: Cell<LevelFilter>,
    buffer_filter: Cell<LevelFilter>,
}

static LOGGER: OnceLock<BufferLogger> = OnceLock::new();

impl BufferLogger {
    pub fn new(buffer: &'static mut [u8]) -> BufferLogger {
        BufferLogger {
            buffer: Mutex::new(LogBuffer::new(buffer)),
            uart_filter: Cell::new(LevelFilter::Info),
            buffer_filter: Cell::new(LevelFilter::Info),
        }
    }

    pub fn register(self) {
        LOGGER.set(self).expect("LOGGER can only be initialized once");
        log::set_logger(LOGGER.get().unwrap()).expect("global logger can only be initialized once");
    }

    pub fn get_logger() -> &'static BufferLogger {
        LOGGER.get().expect("cannot get logger before it is initialized")
    }

    pub fn buffer<'a>(&'a self) -> Option<LogBufferRef<'a>> {
        self.buffer.try_lock().map(LogBufferRef::new)
    }

    pub fn uart_log_level(&self) -> LevelFilter {
        self.uart_filter.get()
    }

    pub fn set_uart_log_level(&self, max_level: LevelFilter) {
        self.uart_filter.set(max_level);
        self.update_global_log_level()
    }

    pub fn buffer_log_level(&self) -> LevelFilter {
        self.buffer_filter.get()
    }

    /// this should be reserved for mgmt module
    pub fn set_buffer_log_level(&self, max_level: LevelFilter) {
        self.buffer_filter.set(max_level);
        self.update_global_log_level()
    }

    pub fn update_global_log_level(&self) {
        let uart_level = self.uart_filter.get();
        let buffer_level = self.buffer_filter.get();
        let global_level = core::cmp::max(uart_level, buffer_level);

        log::set_max_level(global_level);
    }
}

// required for impl Log
unsafe impl Sync for BufferLogger {}

impl Log for BufferLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let timestamp = timer::get_us();
            let seconds = timestamp / 1_000_000;
            let micros = timestamp % 1_000_000;

            if record.level() <= self.buffer_log_level() {
                let mut buffer = self.buffer.lock();
                writeln!(
                    buffer,
                    "[{:6}.{:06}s] {:>5}({}): {}",
                    seconds,
                    micros,
                    record.level(),
                    record.target(),
                    record.args()
                )
                .unwrap();
            }

            if record.level() <= self.uart_log_level() {
                println!(
                    "[{:6}.{:06}s] {:>5}({}): {}",
                    seconds,
                    micros,
                    record.level(),
                    record.target(),
                    record.args()
                );
            }
        }
    }

    fn flush(&self) {
        let uart = stdio::get_uart();
        while !uart.tx_idle() {}
    }
}
