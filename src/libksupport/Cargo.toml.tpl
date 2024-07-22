[package]
name = "ksupport"
description = "Kernel support for Zynq-based platforms"
version = "0.1.0"
authors = ["M-Labs"]
edition = "2018"

[build-dependencies]
build_zynq = { path = "../libbuild_zynq" }

[dependencies]
cslice = "0.3"
log = "0.4"
nb = "0.1"
core_io = { version = "0.1", features = ["collections"] }
byteorder = { version = "1.3", default-features = false }
void = { version = "1", default-features = false }
log_buffer = { version = "1.2" }
libm = { version = "0.2", features = ["unstable"] }
vcell = "0.1"

libboard_zynq = { path = "@@ZYNQ_RS@@/libboard_zynq", features = ["ipv6"]}
libsupport_zynq = { path = "@@ZYNQ_RS@@/libsupport_zynq", default-features = false, features = ["alloc_core"] }
libcortex_a9 = { path = "@@ZYNQ_RS@@/libcortex_a9" }
libasync = { path = "@@ZYNQ_RS@@/libasync" }
libregister = { path = "@@ZYNQ_RS@@/libregister" }
libconfig = { path = "@@ZYNQ_RS@@/libconfig", features = ["fat_lfn", "ipv6"] }

dyld = { path = "../libdyld" }
dwarf = { path = "../libdwarf" }
unwind = { path = "../libunwind" }
libc = { path = "../libc" }
io = { path = "../libio" }
libboard_artiq = { path = "../libboard_artiq" }

[dependencies.nalgebra]
git = "https://git.m-labs.hk/M-labs/nalgebra.git"
rev = "dd00f9b"
default-features = false
features = ["libm", "alloc"]
