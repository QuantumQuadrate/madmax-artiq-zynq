[package]
authors = ["M-Labs"]
name = "io"
version = "0.0.0"

[lib]
name = "io"
path = "lib.rs"

[dependencies]
core_io = { git = "https://git.m-labs.hk/M-Labs/rs-core_io.git", rev = "e9d3edf027", features = ["collections"] }
byteorder = { version = "1.0", default-features = false, optional = true }

libsupport_zynq = { path = "@@ZYNQ_RS@@/libsupport_zynq", default-features = false, features = ["alloc_core"] }

[features]
alloc = []
