#![no_std]
#![feature(never_type)]

#[cfg(feature = "alloc")]
extern crate alloc;
extern crate core_io;

#[cfg(feature = "byteorder")]
extern crate byteorder;

pub mod cursor;
#[cfg(feature = "byteorder")]
pub mod proto;

pub use cursor::Cursor;
#[cfg(all(feature = "byteorder", feature = "alloc"))]
pub use proto::ReadStringError;
#[cfg(feature = "byteorder")]
pub use proto::{ProtoRead, ProtoWrite};
