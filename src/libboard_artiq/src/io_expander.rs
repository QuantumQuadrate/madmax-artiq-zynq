use libboard_zynq::i2c;
use log::info;

#[cfg(has_virtual_leds)]
use crate::pl::csr;

// Only the bare minimum registers. Bits/IO connections equivalent between IC types.
struct Registers {
    // PCA9539 equivalent register names in comments
    iodira: u8, // Configuration Port 0
    iodirb: u8, // Configuration Port 1
    gpioa: u8,  // Output Port 0
    gpiob: u8,  // Output Port 1
}

//IO expanders pins
const IODIR_OUT_SFP_TX_DISABLE: u8 = 0x02;
const IODIR_OUT_SFP_LED: u8 = 0x40;
#[cfg(hw_rev = "v1.0")]
const IODIR_OUT_SFP0_LED: u8 = 0x40;
#[cfg(hw_rev = "v1.1")]
const IODIR_OUT_SFP0_LED: u8 = 0x80;
#[cfg(has_si549)]
const IODIR_CLK_SEL: u8 = 0x80; // out
#[cfg(has_si5324)]
const IODIR_CLK_SEL: u8 = 0x00; // in

//IO expander port direction
const IODIR0: [u8; 2] = [
    0xFF & !IODIR_OUT_SFP_TX_DISABLE & !IODIR_OUT_SFP0_LED,
    0xFF & !IODIR_OUT_SFP_TX_DISABLE & !IODIR_OUT_SFP_LED & !IODIR_CLK_SEL,
];

const IODIR1: [u8; 2] = [
    0xFF & !IODIR_OUT_SFP_TX_DISABLE & !IODIR_OUT_SFP_LED,
    0xFF & !IODIR_OUT_SFP_TX_DISABLE & !IODIR_OUT_SFP_LED,
];

pub struct IoExpander {
    address: u8,
    #[cfg(has_virtual_leds)]
    virtual_led_mapping: &'static [(u8, u8, u8)],
    iodir: [u8; 2],
    out_current: [u8; 2],
    out_target: [u8; 2],
    registers: Registers,
}

impl IoExpander {
    pub fn new(i2c: &mut i2c::I2c, index: u8) -> Result<Self, &'static str> {
        #[cfg(all(hw_rev = "v1.0", has_virtual_leds))]
        const VIRTUAL_LED_MAPPING0: [(u8, u8, u8); 2] = [(0, 0, 6), (1, 1, 6)];
        #[cfg(all(hw_rev = "v1.1", has_virtual_leds))]
        const VIRTUAL_LED_MAPPING0: [(u8, u8, u8); 2] = [(0, 0, 7), (1, 1, 6)];
        #[cfg(has_virtual_leds)]
        const VIRTUAL_LED_MAPPING1: [(u8, u8, u8); 2] = [(2, 0, 6), (3, 1, 6)];

        // Both expanders on SHARED I2C bus
        let mut io_expander = match index {
            0 => IoExpander {
                address: 0x40,
                #[cfg(has_virtual_leds)]
                virtual_led_mapping: &VIRTUAL_LED_MAPPING0,
                iodir: IODIR0,
                out_current: [0; 2],
                out_target: [0; 2],
                registers: Registers {
                    iodira: 0x00,
                    iodirb: 0x01,
                    gpioa: 0x12,
                    gpiob: 0x13,
                },
            },
            1 => IoExpander {
                address: 0x42,
                #[cfg(has_virtual_leds)]
                virtual_led_mapping: &VIRTUAL_LED_MAPPING1,
                iodir: IODIR1,
                out_current: [0; 2],
                out_target: [0; 2],
                registers: Registers {
                    iodira: 0x00,
                    iodirb: 0x01,
                    gpioa: 0x12,
                    gpiob: 0x13,
                },
            },
            _ => return Err("incorrect I/O expander index"),
        };
        if !io_expander.check_ack(i2c)? {
            info!("MCP23017 io expander {} not found. Checking for PCA9539.", index);
            io_expander.address += 0xa8; // translate to PCA9539 addresses (see schematic)
            io_expander.registers = Registers {
                iodira: 0x06,
                iodirb: 0x07,
                gpioa: 0x02,
                gpiob: 0x03,
            };
            if !io_expander.check_ack(i2c)? {
                return Err("Neither MCP23017 nor PCA9539 io expander found.");
            };
        }
        Ok(io_expander)
    }

    fn select(&self, i2c: &mut i2c::I2c) -> Result<(), &'static str> {
        i2c.pca954x_select(0x70, None)?;
        i2c.pca954x_select(0x71, Some(3))?;
        Ok(())
    }

    fn write(&self, i2c: &mut i2c::I2c, addr: u8, value: u8) -> Result<(), &'static str> {
        i2c.start()?;
        let write_res = i2c
            .write(self.address)
            .and_then(|_| i2c.write(addr))
            .and_then(|_| i2c.write(value));
        let stop_res = i2c.stop();
        write_res.and(stop_res).map_err(|e| e.into())
    }

    fn check_ack(&self, i2c: &mut i2c::I2c) -> Result<bool, &'static str> {
        // Check for ack from io expander
        self.select(i2c)?;
        i2c.start()?;
        let ack = match i2c.write(self.address) {
            Ok(()) => true,
            Err(i2c::Error::Nack) => false,
            Err(e) => return Err(e.into()),
        };
        i2c.stop()?;
        Ok(ack)
    }

    fn update_iodir(&self, i2c: &mut i2c::I2c) -> Result<(), &'static str> {
        self.write(i2c, self.registers.iodira, self.iodir[0])?;
        self.write(i2c, self.registers.iodirb, self.iodir[1])?;
        Ok(())
    }

    pub fn init(&mut self, i2c: &mut i2c::I2c) -> Result<(), &'static str> {
        self.select(i2c)?;

        self.update_iodir(i2c)?;

        self.out_current[0] = 0x00;
        self.write(i2c, self.registers.gpioa, 0x00)?;
        self.out_current[1] = 0x00;
        self.write(i2c, self.registers.gpiob, 0x00)?;
        Ok(())
    }

    pub fn set_oe(&mut self, i2c: &mut i2c::I2c, port: u8, outputs: u8) -> Result<(), &'static str> {
        self.iodir[port as usize] &= !outputs;
        self.update_iodir(i2c)?;
        Ok(())
    }

    pub fn set(&mut self, port: u8, bit: u8, high: bool) {
        if high {
            self.out_target[port as usize] |= 1 << bit;
        } else {
            self.out_target[port as usize] &= !(1 << bit);
        }
    }

    pub fn service(&mut self, i2c: &mut i2c::I2c) -> Result<(), &'static str> {
        #[cfg(has_virtual_leds)]
        for (led, port, bit) in self.virtual_led_mapping.iter() {
            let level = unsafe { csr::virtual_leds::status_read() >> led & 1 };
            self.set(*port, *bit, level != 0);
        }

        if self.out_target != self.out_current {
            self.select(i2c)?;
            if self.out_target[0] != self.out_current[0] {
                self.write(i2c, self.registers.gpioa, self.out_target[0])?;
                self.out_current[0] = self.out_target[0];
            }
            if self.out_target[1] != self.out_current[1] {
                self.write(i2c, self.registers.gpiob, self.out_target[1])?;
                self.out_current[1] = self.out_target[1];
            }
        }

        Ok(())
    }
}
