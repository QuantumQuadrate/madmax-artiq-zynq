#[cfg(feature = "alloc")]
use alloc::{string::String, vec};
use core::str::Utf8Error;

use byteorder::ByteOrder;
use core_io::{Error, Read, Write};

#[cfg(feature = "alloc")]
#[derive(Debug, Clone, PartialEq)]
pub enum ReadStringError<T> {
    Utf8(Utf8Error),
    Other(T),
}

pub trait ProtoRead: Read {
    #[inline]
    fn read_u8(&mut self) -> Result<u8, Error> {
        let mut bytes = [0; 1];
        self.read_exact(&mut bytes)?;
        Ok(bytes[0])
    }

    #[inline]
    fn read_u16<T: ByteOrder>(&mut self) -> Result<u16, Error> {
        let mut bytes = [0; 2];
        self.read_exact(&mut bytes)?;
        Ok(T::read_u16(&bytes))
    }

    #[inline]
    fn read_u32<T: ByteOrder>(&mut self) -> Result<u32, Error> {
        let mut bytes = [0; 4];
        self.read_exact(&mut bytes)?;
        Ok(T::read_u32(&bytes))
    }

    #[inline]
    fn read_u64<T: ByteOrder>(&mut self) -> Result<u64, Error> {
        let mut bytes = [0; 8];
        self.read_exact(&mut bytes)?;
        Ok(T::read_u64(&bytes))
    }

    #[inline]
    fn read_bool(&mut self) -> Result<bool, Error> {
        Ok(self.read_u8()? != 0)
    }

    #[inline]
    #[cfg(feature = "alloc")]
    fn read_bytes<T: ByteOrder>(&mut self) -> Result<vec::Vec<u8>, Error> {
        let length = self.read_u32::<T>()?;
        let mut value = vec![0; length as usize];
        self.read_exact(&mut value)?;
        Ok(value)
    }

    #[inline]
    #[cfg(feature = "alloc")]
    fn read_string<T: ByteOrder>(&mut self) -> Result<String, ReadStringError<Error>> {
        let bytes = self.read_bytes::<T>().map_err(ReadStringError::Other)?;
        String::from_utf8(bytes).map_err(|err| ReadStringError::Utf8(err.utf8_error()))
    }
}

pub trait ProtoWrite: Write {
    #[inline]
    fn write_u8(&mut self, value: u8) -> Result<(), Error> {
        let bytes = [value; 1];
        self.write_all(&bytes)
    }

    #[inline]
    fn write_i8(&mut self, value: i8) -> Result<(), Error> {
        let bytes = [value as u8; 1];
        self.write_all(&bytes)
    }

    #[inline]
    fn write_u16<T: ByteOrder>(&mut self, value: u16) -> Result<(), Error> {
        let mut bytes = [0; 2];
        T::write_u16(&mut bytes, value);
        self.write_all(&bytes)
    }

    #[inline]
    fn write_i16<T: ByteOrder>(&mut self, value: i16) -> Result<(), Error> {
        let mut bytes = [0; 2];
        T::write_i16(&mut bytes, value);
        self.write_all(&bytes)
    }

    #[inline]
    fn write_u32<T: ByteOrder>(&mut self, value: u32) -> Result<(), Error> {
        let mut bytes = [0; 4];
        T::write_u32(&mut bytes, value);
        self.write_all(&bytes)
    }

    #[inline]
    fn write_i32<T: ByteOrder>(&mut self, value: i32) -> Result<(), Error> {
        let mut bytes = [0; 4];
        T::write_i32(&mut bytes, value);
        self.write_all(&bytes)
    }

    #[inline]
    fn write_u64<T: ByteOrder>(&mut self, value: u64) -> Result<(), Error> {
        let mut bytes = [0; 8];
        T::write_u64(&mut bytes, value);
        self.write_all(&bytes)
    }

    #[inline]
    fn write_i64<T: ByteOrder>(&mut self, value: i64) -> Result<(), Error> {
        let mut bytes = [0; 8];
        T::write_i64(&mut bytes, value);
        self.write_all(&bytes)
    }

    #[inline]
    fn write_bool(&mut self, value: bool) -> Result<(), Error> {
        self.write_u8(value as u8)
    }

    #[inline]
    fn write_bytes<T: ByteOrder>(&mut self, value: &[u8]) -> Result<(), Error> {
        self.write_u32::<T>(value.len() as u32)?;
        self.write_all(value)
    }

    #[inline]
    #[cfg(feature = "alloc")]
    fn write_string<T: ByteOrder>(&mut self, value: &str) -> Result<(), Error> {
        self.write_bytes::<T>(value.as_bytes())
    }
}

impl<T: Read> ProtoRead for T {}

impl<T: Write> ProtoWrite for T {}
