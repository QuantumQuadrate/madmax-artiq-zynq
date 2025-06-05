use core::result;

use libboard_zynq::{i2c::{Error as I2cError, I2c},
                    timer};
use log::info;

#[cfg(not(si5324_soft_reset))]
use crate::pl::csr;

type Result<T> = result::Result<T, &'static str>;

const ADDRESS: u8 = 0x68;

#[cfg(not(si5324_soft_reset))]
fn hard_reset() {
    unsafe {
        csr::si5324_rst_n::out_write(0);
    }
    timer::delay_us(1_000);
    unsafe {
        csr::si5324_rst_n::out_write(1);
    }
    timer::delay_us(10_000);
}

// NOTE: the logical parameters DO NOT MAP to physical values written
// into registers. They have to be mapped; see the datasheet.
// DSPLLsim reports the logical parameters in the design summary, not
// the physical register values.
pub struct FrequencySettings {
    pub n1_hs: u8,
    pub nc1_ls: u32,
    pub n2_hs: u8,
    pub n2_ls: u32,
    pub n31: u32,
    pub n32: u32,
    pub bwsel: u8,
    pub crystal_as_ckin2: bool,
}

pub enum Input {
    Ckin1,
    Ckin2,
}

fn map_frequency_settings(settings: &FrequencySettings) -> Result<FrequencySettings> {
    if settings.nc1_ls != 0 && (settings.nc1_ls % 2) == 1 {
        return Err("NC1_LS must be 0 or even");
    }
    if settings.nc1_ls > (1 << 20) {
        return Err("NC1_LS is too high");
    }
    if (settings.n2_ls % 2) == 1 {
        return Err("N2_LS must be even");
    }
    if settings.n2_ls > (1 << 20) {
        return Err("N2_LS is too high");
    }
    if settings.n31 > (1 << 19) {
        return Err("N31 is too high");
    }
    if settings.n32 > (1 << 19) {
        return Err("N32 is too high");
    }
    let r = FrequencySettings {
        n1_hs: match settings.n1_hs {
            4 => 0b000,
            5 => 0b001,
            6 => 0b010,
            7 => 0b011,
            8 => 0b100,
            9 => 0b101,
            10 => 0b110,
            11 => 0b111,
            _ => return Err("N1_HS has an invalid value"),
        },
        nc1_ls: settings.nc1_ls - 1,
        n2_hs: match settings.n2_hs {
            4 => 0b000,
            5 => 0b001,
            6 => 0b010,
            7 => 0b011,
            8 => 0b100,
            9 => 0b101,
            10 => 0b110,
            11 => 0b111,
            _ => return Err("N2_HS has an invalid value"),
        },
        n2_ls: settings.n2_ls - 1,
        n31: settings.n31 - 1,
        n32: settings.n32 - 1,
        bwsel: settings.bwsel,
        crystal_as_ckin2: settings.crystal_as_ckin2,
    };
    Ok(r)
}

fn write(i2c: &mut I2c, reg: u8, val: u8) -> Result<()> {
    i2c.start().unwrap();
    i2c.write(ADDRESS << 1).map_err(|err| match err {
        I2cError::Nack => "Si5324 failed to ack write address",
        err => err.into(),
    })?;
    i2c.write(reg).map_err(|err| match err {
        I2cError::Nack => "Si5324 failed to ack register",
        err => err.into(),
    })?;
    i2c.write(val).map_err(|err| match err {
        I2cError::Nack => "Si5324 failed to ack value",
        err => err.into(),
    })?;
    i2c.stop().unwrap();
    Ok(())
}

#[allow(dead_code)]
fn write_no_ack_value(i2c: &mut I2c, reg: u8, val: u8) -> Result<()> {
    i2c.start().unwrap();
    i2c.write(ADDRESS << 1).map_err(|err| match err {
        I2cError::Nack => "Si5324 failed to ack write address",
        err => err.into(),
    })?;
    i2c.write(reg).map_err(|err| match err {
        I2cError::Nack => "Si5324 failed to ack register",
        err => err.into(),
    })?;
    match i2c.write(val) {
        Ok(()) | Err(I2cError::Nack) => (),
        Err(e) => return Err(e.into()),
    }
    i2c.stop().unwrap();
    Ok(())
}

fn read(i2c: &mut I2c, reg: u8) -> Result<u8> {
    i2c.start().unwrap();
    i2c.write(ADDRESS << 1).map_err(|err| match err {
        I2cError::Nack => "Si5324 failed to ack write address",
        err => err.into(),
    })?;
    i2c.write(reg).map_err(|err| match err {
        I2cError::Nack => "Si5324 failed to ack register",
        err => err.into(),
    })?;
    i2c.restart().unwrap();
    i2c.write((ADDRESS << 1) | 1).map_err(|err| match err {
        I2cError::Nack => "Si5324 failed to ack read address",
        err => err.into(),
    })?;
    let val = i2c.read(false).unwrap();
    i2c.stop().unwrap();
    Ok(val)
}

fn rmw<F>(i2c: &mut I2c, reg: u8, f: F) -> Result<()>
where F: Fn(u8) -> u8 {
    let value = read(i2c, reg)?;
    write(i2c, reg, f(value))?;
    Ok(())
}

fn ident(i2c: &mut I2c) -> Result<u16> {
    Ok(((read(i2c, 134)? as u16) << 8) | (read(i2c, 135)? as u16))
}

#[cfg(si5324_soft_reset)]
fn soft_reset(i2c: &mut I2c) -> Result<()> {
    let val = read(i2c, 136)?;
    write_no_ack_value(i2c, 136, val | 0x80)?;
    timer::delay_us(10_000);
    Ok(())
}

fn has_xtal(i2c: &mut I2c) -> Result<bool> {
    Ok((read(i2c, 129)? & 0x01) == 0) // LOSX_INT=0
}

fn has_ckin(i2c: &mut I2c, input: Input) -> Result<bool> {
    match input {
        Input::Ckin1 => Ok((read(i2c, 129)? & 0x02) == 0), // LOS1_INT=0
        Input::Ckin2 => Ok((read(i2c, 129)? & 0x04) == 0), // LOS2_INT=0
    }
}

fn locked(i2c: &mut I2c) -> Result<bool> {
    Ok((read(i2c, 130)? & 0x01) == 0) // LOL_INT=0
}

fn monitor_lock(i2c: &mut I2c) -> Result<()> {
    info!("waiting for Si5324 lock...");
    let timeout = timer::get_ms() + 20_000;
    while !locked(i2c)? {
        // Yes, lock can be really slow.
        if timer::get_ms() > timeout {
            return Err("Si5324 lock timeout");
        }
    }
    info!("  ...locked");
    Ok(())
}

fn init(i2c: &mut I2c) -> Result<()> {
    #[cfg(not(si5324_soft_reset))]
    hard_reset();

    #[cfg(feature = "target_kasli_soc")]
    {
        i2c.pca954x_select(0x70, None)?;
        i2c.pca954x_select(0x71, Some(3))?;
    }
    #[cfg(feature = "target_zc706")]
    {
        i2c.pca954x_select(0x74, Some(4))?;
    }

    if ident(i2c)? != 0x0182 {
        return Err("Si5324 does not have expected product number");
    }

    #[cfg(si5324_soft_reset)]
    soft_reset(i2c)?;
    Ok(())
}

pub fn bypass(i2c: &mut I2c, input: Input) -> Result<()> {
    let cksel_reg = match input {
        Input::Ckin1 => 0b00,
        Input::Ckin2 => 0b01,
    };
    init(i2c)?;
    rmw(i2c, 21, |v| v & 0xfe)?; // CKSEL_PIN=0
    rmw(i2c, 3, |v| (v & 0x3f) | (cksel_reg << 6))?; // CKSEL_REG
    rmw(i2c, 4, |v| (v & 0x3f) | (0b00 << 6))?; // AUTOSEL_REG=b00
    rmw(i2c, 6, |v| (v & 0xc0) | 0b111111)?; // SFOUT2_REG=b111 SFOUT1_REG=b111
    rmw(i2c, 0, |v| (v & 0xfd) | 0x02)?; // BYPASS_REG=1
    Ok(())
}

pub fn setup(i2c: &mut I2c, settings: &FrequencySettings, input: Input) -> Result<()> {
    let s = map_frequency_settings(settings)?;
    let cksel_reg = match input {
        Input::Ckin1 => 0b00,
        Input::Ckin2 => 0b01,
    };

    init(i2c)?;
    if settings.crystal_as_ckin2 {
        rmw(i2c, 0, |v| v | 0x40)?; // FREE_RUN=1
    }
    rmw(i2c, 2, |v| (v & 0x0f) | (s.bwsel << 4))?;
    rmw(i2c, 21, |v| v & 0xfe)?; // CKSEL_PIN=0
    rmw(i2c, 3, |v| (v & 0x2f) | (cksel_reg << 6) | 0x10)?; // CKSEL_REG, SQ_ICAL=1
    rmw(i2c, 4, |v| (v & 0x3f) | (0b00 << 6))?; // AUTOSEL_REG=b00
    rmw(i2c, 6, |v| (v & 0xc0) | 0b111111)?; // SFOUT2_REG=b111 SFOUT1_REG=b111
    write(i2c, 25, (s.n1_hs << 5) as u8)?;
    write(i2c, 31, (s.nc1_ls >> 16) as u8)?;
    write(i2c, 32, (s.nc1_ls >> 8) as u8)?;
    write(i2c, 33, (s.nc1_ls) as u8)?;
    write(i2c, 34, (s.nc1_ls >> 16) as u8)?; // write to NC2_LS as well
    write(i2c, 35, (s.nc1_ls >> 8) as u8)?;
    write(i2c, 36, (s.nc1_ls) as u8)?;
    write(i2c, 40, (s.n2_hs << 5) as u8 | (s.n2_ls >> 16) as u8)?;
    write(i2c, 41, (s.n2_ls >> 8) as u8)?;
    write(i2c, 42, (s.n2_ls) as u8)?;
    write(i2c, 43, (s.n31 >> 16) as u8)?;
    write(i2c, 44, (s.n31 >> 8) as u8)?;
    write(i2c, 45, (s.n31) as u8)?;
    write(i2c, 46, (s.n32 >> 16) as u8)?;
    write(i2c, 47, (s.n32 >> 8) as u8)?;
    write(i2c, 48, (s.n32) as u8)?;
    rmw(i2c, 137, |v| v | 0x01)?; // FASTLOCK=1
    rmw(i2c, 136, |v| v | 0x40)?; // ICAL=1

    if !has_xtal(i2c)? {
        return Err("Si5324 misses XA/XB signal");
    }
    if !has_ckin(i2c, input)? {
        return Err("Si5324 misses clock input signal");
    }

    monitor_lock(i2c)?;
    Ok(())
}

pub fn select_input(i2c: &mut I2c, input: Input) -> Result<()> {
    let cksel_reg = match input {
        Input::Ckin1 => 0b00,
        Input::Ckin2 => 0b01,
    };
    rmw(i2c, 3, |v| (v & 0x3f) | (cksel_reg << 6))?;
    if !has_ckin(i2c, input)? {
        return Err("Si5324 misses clock input signal");
    }
    monitor_lock(i2c)?;
    Ok(())
}

#[cfg(has_siphaser)]
pub mod siphaser {
    use super::*;
    use crate::pl::csr;

    pub fn select_recovered_clock(i2c: &mut I2c, rc: bool) -> Result<()> {
        i2c.pca954x_select(0x70, None)?;
        i2c.pca954x_select(0x71, Some(3))?;
        let val = read(i2c, 3)?;
        write(i2c, 3, (val & 0xdf) | (1 << 5))?; // DHOLD=1
        unsafe {
            csr::siphaser::switch_clocks_write(if rc { 1 } else { 0 });
        }
        let val = read(i2c, 3)?;
        write(i2c, 3, (val & 0xdf) | (0 << 5))?; // DHOLD=0
        monitor_lock(i2c)?;
        Ok(())
    }

    fn phase_shift(direction: u8) {
        unsafe {
            csr::siphaser::phase_shift_write(direction);
            while csr::siphaser::phase_shift_done_read() == 0 {}
        }
        // wait for the Si5324 loop to stabilize
        timer::delay_us(500);
    }

    fn has_error() -> bool {
        unsafe {
            csr::siphaser::error_write(1);
        }
        timer::delay_us(5_000);
        unsafe { csr::siphaser::error_read() != 0 }
    }

    fn find_edge(target: bool) -> Result<u32> {
        let mut nshifts = 0;

        let mut previous = has_error();
        loop {
            phase_shift(1);
            nshifts += 1;
            let current = has_error();
            if previous != target && current == target {
                return Ok(nshifts);
            }
            if nshifts > 5000 {
                return Err("failed to find timing error edge");
            }
            previous = current;
        }
    }

    pub fn calibrate_skew() -> Result<()> {
        let jitter_margin = 32;
        let lead = find_edge(false)?;
        for _ in 0..jitter_margin {
            phase_shift(1);
        }
        let width = find_edge(true)? + jitter_margin;
        // width is 360 degrees (one full rotation of the phase between s/h limits) minus jitter
        info!(
            "calibration successful, lead: {}, width: {} ({}deg)",
            lead,
            width,
            width * 360 / (56 * 8)
        );

        // Apply reverse phase shift for half the width to get into the
        // middle of the working region.
        for _ in 0..width / 2 {
            phase_shift(0);
        }

        Ok(())
    }
}
