from migen.build.generic_platform import *

fmc_adapter_io = [
    # CoaXPress high speed link
    ("CXP_HS", 0,
        Subsignal("rxp", Pins("HPC:DP0_M2C_P")),
        Subsignal("rxn", Pins("HPC:DP0_M2C_N")),
    ),
    ("CXP_HS", 1,
        Subsignal("rxp", Pins("HPC:DP1_M2C_P")),
        Subsignal("rxn", Pins("HPC:DP1_M2C_N")),
    ),
    ("CXP_HS", 2,
        Subsignal("rxp", Pins("HPC:DP2_M2C_P")),
        Subsignal("rxn", Pins("HPC:DP2_M2C_N")),
    ),
    ("CXP_HS", 3,
        Subsignal("rxp", Pins("HPC:DP3_M2C_P")),
        Subsignal("rxn", Pins("HPC:DP3_M2C_N")),
    ),

    # CoaXPress low speed link
    ("CXP_LS", 0, Pins("HPC:LA00_CC_P"), IOStandard("LVCMOS33")),
    ("CXP_LS", 1, Pins("HPC:LA01_CC_N"), IOStandard("LVCMOS33")),
    ("CXP_LS", 2, Pins("HPC:LA01_CC_P"), IOStandard("LVCMOS33")),
    ("CXP_LS", 3, Pins("HPC:LA02_N"), IOStandard("LVCMOS33")),

    # CoaXPress green and red LED
    ("CXP_LED", 0,
        Subsignal("green", Pins("HPC:LA11_P"), IOStandard("LVCMOS33")),
        Subsignal("red", Pins("HPC:LA11_N"), IOStandard("LVCMOS33")),
    ),
    ("CXP_LED", 1,
        Subsignal("green", Pins("HPC:LA12_P"), IOStandard("LVCMOS33")),
        Subsignal("red", Pins("HPC:LA12_N"), IOStandard("LVCMOS33")),
    ),
    ("CXP_LED", 2,
        Subsignal("green", Pins("HPC:LA13_P"), IOStandard("LVCMOS33")),
        Subsignal("red", Pins("HPC:LA13_N"), IOStandard("LVCMOS33")),
    ),
    ("CXP_LED", 3,
        Subsignal("green", Pins("HPC:LA14_P"), IOStandard("LVCMOS33")),
        Subsignal("red", Pins("HPC:LA14_N"), IOStandard("LVCMOS33")),
    ),

    # Power over CoaXPress 
    ("PoCXP", 0,
        Subsignal("enable", Pins("HPC:LA21_N"), IOStandard("LVCMOS33")),
        Subsignal("alert", Pins("HPC:LA18_CC_P"), IOStandard("LVCMOS33")),
    ),
    ("PoCXP", 1,
        Subsignal("enable", Pins("HPC:LA21_P"), IOStandard("LVCMOS33")),
        Subsignal("alert", Pins("HPC:LA19_N"), IOStandard("LVCMOS33")),
    ),
    ("PoCXP", 2,
        Subsignal("enable", Pins("HPC:LA22_N"), IOStandard("LVCMOS33")),
        Subsignal("alert", Pins("HPC:LA19_P"), IOStandard("LVCMOS33")),
    ),
    ("PoCXP", 3,
        Subsignal("enable", Pins("HPC:LA22_P"), IOStandard("LVCMOS33")),
        Subsignal("alert", Pins("HPC:LA20_N"), IOStandard("LVCMOS33")),
    ),
    ("i2c", 0,
        Subsignal("scl", Pins("HPC:IIC_SCL")),
        Subsignal("sda", Pins("HPC:IIC_SDA")),
        IOStandard("LVCMOS33")
    ),

    # On board 125MHz reference 
    ("clk125", 0,
        Subsignal("p", Pins("HPC:GBTCLK0_M2C_P")),
        Subsignal("n", Pins("HPC:GBTCLK0_M2C_N")),
    ),
]
