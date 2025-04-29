use libboard_zynq::i2c;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LEDState {
    Off,
    RedFlash1Hz,      // Not connected
    OrangeFlash12Hz5, // camera setup
    GreenSolid,       // Connected
}

const SFP_SLOT: u8 = 0;
static mut PREVIOUS_STATE: LEDState = LEDState::Off;

const PCA9530_ADDR: u8 = 0x60;
const PSC0_ADDR: u8 = 0x01;
const PWM0_ADDR: u8 = 0x02;
const LS0_ADDR: u8 = 0x05;

pub fn update_led(i2c: &mut i2c::I2c, state: LEDState) {
    if unsafe { state != PREVIOUS_STATE } {
        match write_settings(i2c, state) {
            Ok(_) => unsafe { PREVIOUS_STATE = state },
            Err(_) => {
                // stop i2c in case error happen during read/write operation
                let _ = i2c.stop();
            }
        };
    }
}

fn write_settings(i2c: &mut i2c::I2c, state: LEDState) -> Result<(), i2c::Error> {
    i2c.pca954x_select(0x70, None)?;
    i2c.pca954x_select(0x71, Some(SFP_SLOT))?;
    write_pwm_freq(i2c, state)?;
    write_pwm_duty(i2c, state)?;
    write_pwm_output(i2c, state)?;

    Ok(())
}

fn write_pwm_freq(i2c: &mut i2c::I2c, state: LEDState) -> Result<(), i2c::Error> {
    match state {
        LEDState::OrangeFlash12Hz5 => {
            i2c_write(i2c, PSC0_ADDR, 0xB)?; // set PWM0 frequency to 12.5 Hz
        }
        LEDState::RedFlash1Hz => {
            i2c_write(i2c, PSC0_ADDR, 0x97)?; // set PWM0 frequency to 1 Hz
        }
        _ => {}
    };
    Ok(())
}
fn write_pwm_duty(i2c: &mut i2c::I2c, state: LEDState) -> Result<(), i2c::Error> {
    match state {
        LEDState::OrangeFlash12Hz5 => {
            i2c_write(i2c, PWM0_ADDR, 0x40)?; // set PWM0 duty cycle to 25%
        }
        LEDState::RedFlash1Hz => {
            i2c_write(i2c, PWM0_ADDR, 0x33)?; // set PWM0 duty cycle to 20%
        }
        _ => {}
    };
    Ok(())
}
fn write_pwm_output(i2c: &mut i2c::I2c, state: LEDState) -> Result<(), i2c::Error> {
    let reg = match state {
        LEDState::GreenSolid => 0xF1,       // Green: always on, Red: off
        LEDState::Off => 0xF0,              // Green: off, Red: off
        LEDState::OrangeFlash12Hz5 => 0xFA, // Green: use PWM0, Red: use PWM0
        LEDState::RedFlash1Hz => 0xF8,      // Green: off, Red: use PWM0
    };

    i2c_write(i2c, LS0_ADDR, reg)?;
    Ok(())
}

fn i2c_write(i2c: &mut i2c::I2c, reg_addr: u8, val: u8) -> Result<(), i2c::Error> {
    i2c.start()?;
    i2c.write(PCA9530_ADDR << 1)?;
    i2c.write(reg_addr)?;
    i2c.write(val)?;
    i2c.stop()?;
    Ok(())
}
