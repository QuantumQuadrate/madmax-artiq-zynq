core_addr = "192.168.1.57"

device_db = {
    "core": {
        "type": "local",
        "module": "artiq.coredevice.core",
        "class": "Core",
        "arguments": {
            "host": core_addr,
            "ref_period": 1e-9,
            "target": "cortexa9",
        },
    },
    "core_log": {
        "type": "controller",
        "host": "::1",
        "port": 1068,
        "command": "aqctl_corelog -p {port} --bind {bind} " + core_addr,
    },
    "core_moninj": {
        "type": "controller",
        "host": "::1",
        "port_proxy": 1383,
        "port": 1384,
        "command": "aqctl_moninj_proxy --port-proxy {port_proxy} --port-control {port} --bind {bind} "
        + core_addr,
    },
    "core_analyzer": {
        "type": "controller",
        "host": "::1",
        "port_proxy": 1385,
        "port": 1386,
        "command": "aqctl_coreanalyzer_proxy --port-proxy {port_proxy} --port-control {port} --bind {bind} "
        + core_addr,
    },
    "core_cache": {
        "type": "local",
        "module": "artiq.coredevice.cache",
        "class": "CoreCache",
    },
    "core_dma": {"type": "local", "module": "artiq.coredevice.dma", "class": "CoreDMA"},
    "led0": {
        "type": "local",
        "module": "artiq.coredevice.ttl",
        "class": "TTLOut",
        "arguments": {"channel": 0},
    },
    "led1": {
        "type": "local",
        "module": "artiq.coredevice.ttl",
        "class": "TTLOut",
        "arguments": {"channel": 1},
    },
}


device_db.update(
    spi0={
        "type": "local",
        "module": "artiq.coredevice.spi2",
        "class": "SPIMaster",
        "arguments": {"channel": 2},
    },
    dds0={
        "type": "local",
        "module": "artiq.coredevice.ad9834",
        "class": "AD9834",
        "arguments": {"spi_device": "spi0"},
    },
)
