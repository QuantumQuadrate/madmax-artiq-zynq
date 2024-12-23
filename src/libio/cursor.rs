#[cfg(feature = "alloc")]
use alloc::vec::Vec;
use core::arch::asm;

use core_io::{Error as IoError, Read, Write};

#[derive(Debug, Clone)]
pub struct Cursor<T> {
    inner: T,
    pos: usize,
}

impl<T> Cursor<T> {
    #[inline]
    pub fn new(inner: T) -> Cursor<T> {
        Cursor { inner, pos: 0 }
    }

    #[inline]
    pub fn into_inner(self) -> T {
        self.inner
    }

    #[inline]
    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    #[inline]
    pub fn position(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn set_position(&mut self, pos: usize) {
        self.pos = pos
    }
}

impl<T: AsRef<[u8]>> Read for Cursor<T> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        let data = &self.inner.as_ref()[self.pos..];
        let len = buf.len().min(data.len());
        // ``copy_from_slice`` generates AXI bursts, use a regular loop instead
        for i in 0..len {
            unsafe {
                asm!("", options(preserves_flags, nostack, readonly));
            }
            buf[i] = data[i];
        }
        self.pos += len;
        Ok(len)
    }
}

impl Write for Cursor<&mut [u8]> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
        let data = &mut self.inner[self.pos..];
        let len = buf.len().min(data.len());
        for i in 0..len {
            unsafe {
                asm!("", options(preserves_flags, nostack, readonly));
            }
            data[i] = buf[i];
        }
        self.pos += len;
        Ok(len)
    }

    #[inline]
    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

#[cfg(feature = "alloc")]
impl Write for Cursor<Vec<u8>> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
        self.inner.extend_from_slice(buf);
        Ok(buf.len())
    }

    #[inline]
    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}
