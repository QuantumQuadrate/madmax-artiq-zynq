[package]
authors = ["M-Labs"]
name = "dyld"
version = "0.1.0"
edition = "2018"

[lib]
name = "dyld"

[dependencies]
log = "0.4"
libcortex_a9 = { path = "@@ZYNQ_RS@@/libcortex_a9" }
