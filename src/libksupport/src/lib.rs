#![no_std]
#![feature(c_variadic)]
#![feature(const_btree_len)]
#![feature(naked_functions)]
#![allow(unexpected_cfgs)]
#![allow(static_mut_refs)]

#[macro_use]
extern crate alloc;

#[cfg(has_drtiosat)]
pub use pl::csr::drtiosat as rtio_core;
#[cfg(has_rtio_core)]
pub use pl::csr::rtio_core;

pub mod eh_artiq;
pub mod irq;
pub mod kernel;
pub mod rpc;
#[rustfmt::skip]
#[path = "../../../build/pl.rs"]
pub mod pl;

#[derive(Debug, Clone)]
pub struct RPCException {
    pub id: u32,
    pub message: u32,
    pub param: [i64; 3],
    pub file: u32,
    pub line: i32,
    pub column: i32,
    pub function: u32,
}
