TARGET := zc706
GWARGS := -V nist_clock

all: runtime

runtime: ../build/runtime.bin

satman: ../build/satman.bin

.PHONY: all manifests

manifests = libboard_artiq/Cargo.toml libc/Cargo.toml libdyld/Cargo.toml libio/Cargo.toml libksupport/Cargo.toml runtime/Cargo.toml satman/Cargo.toml

$(manifests): %.toml: %.toml.tpl
	sed s+@@ZYNQ_RS@@+$(ZYNQ_RS)+g $< > $@

manifests: $(manifests)

../build/pl.rs ../build/rustc-cfg ../build/mem.rs: gateware/* 
	mkdir -p ../build
	python gateware/$(TARGET).py -r ../build/pl.rs -c ../build/rustc-cfg -m ../build/mem.rs $(GWARGS)

../build/firmware/armv7-none-eabihf/release/runtime: ../build/pl.rs ../build/rustc-cfg ../build/mem.rs $(manifests) $(shell find . -type f -not -name Cargo.toml -print)
	cargo build --release \
	    -p runtime \
		--target-dir ../build/firmware \
		--no-default-features --features=target_$(TARGET)

../build/runtime.bin: ../build/firmware/armv7-none-eabihf/release/runtime
	llvm-objcopy -O binary ../build/firmware/armv7-none-eabihf/release/runtime ../build/runtime.bin

../build/firmware/armv7-none-eabihf/release/satman: ../build/pl.rs ../build/rustc-cfg ../build/mem.rs $(manifests) $(shell find . -type f -not -name Cargo.toml -print)
	cargo build --release \
	    -p satman \
		--target-dir ../build/firmware \
		--no-default-features --features=target_$(TARGET)

../build/satman.bin: ../build/firmware/armv7-none-eabihf/release/satman
	llvm-objcopy -O binary ../build/firmware/armv7-none-eabihf/release/satman ../build/satman.bin
