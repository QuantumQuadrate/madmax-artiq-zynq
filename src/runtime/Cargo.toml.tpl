[package]
name = "runtime"
description = "ARTIQ runtime on Zynq"
version = "0.1.0"
authors = ["M-Labs"]
edition = "2018"

[features]
target_zc706 = ["libboard_zynq/target_zc706", "libsupport_zynq/target_zc706", "libconfig/target_zc706", "libboard_artiq/target_zc706"]
target_kasli_soc = ["libboard_zynq/target_kasli_soc", "libsupport_zynq/target_kasli_soc", "libconfig/target_kasli_soc", "libboard_artiq/target_kasli_soc"]
target_ebaz4205 = ["libboard_zynq/target_ebaz4205", "libsupport_zynq/target_ebaz4205", "libconfig/target_ebaz4205", "libboard_artiq/target_ebaz4205"]
default = ["target_zc706"]

[build-dependencies]
build_zynq = { path = "../libbuild_zynq" }

[dependencies]
num-traits = { version = "0.2", default-features = false }
num-derive = "0.4"
cslice = "0.3"
log = "0.4"
embedded-hal = "0.2"
core_io = { git = "https://git.m-labs.hk/M-Labs/rs-core_io.git", rev = "e9d3edf027", features = ["collections"] }
crc = { version = "1.7", default-features = false }
byteorder = { version = "1.3", default-features = false }
void = { version = "1", default-features = false }
futures = { version = "0.3", default-features = false, features = ["async-await"] }
async-recursion = "1.1"
log_buffer = { version = "1.2" }
vcell = "0.1"

libboard_zynq = { path = "@@ZYNQ_RS@@/libboard_zynq", features = ["ipv6", "async"]}
libsupport_zynq = { path = "@@ZYNQ_RS@@/libsupport_zynq", default-features = false, features = ["alloc_core"] }
libcortex_a9 = { path = "@@ZYNQ_RS@@/libcortex_a9" }
libasync = { path = "@@ZYNQ_RS@@/libasync" }
libregister = { path = "@@ZYNQ_RS@@/libregister" }
libconfig = { path = "@@ZYNQ_RS@@/libconfig", features = ["fat_lfn", "ipv6"] }

dyld = { path = "../libdyld" }
dwarf = { path = "../libdwarf" }
unwind = { path = "../libunwind" }
libc = { path = "../libc" }
io = { path = "../libio", features = ["alloc"] }
ksupport = { path = "../libksupport" }
libboard_artiq = { path = "../libboard_artiq" }

[dependencies.tar-no-std]
git = "https://git.m-labs.hk/M-Labs/tar-no-std"
rev = "2ab6dc5"
