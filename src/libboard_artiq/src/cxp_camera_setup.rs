use core::fmt;

use embedded_hal::blocking::delay::DelayMs;
use libboard_zynq::{time::Milliseconds, timer::GlobalTimer};
use log::debug;

use crate::{cxp_ctrl::Error as CtrlErr,
            cxp_packet::{read_u32, read_u64, reset_tag, send_test_packet, write_bytes_no_ack, write_u32, write_u64},
            cxp_phys::{rx, tx, CXPSpeed},
            pl::csr};

// Bootstrap registers address
const REVISION: u32 = 0x0004;
const CONNECTION_RESET: u32 = 0x4000;
const DEVICE_CONNECTION_ID: u32 = 0x4004;
const MASTER_HOST_CONNECTION_ID: u32 = 0x4008;

const STREAM_PACKET_SIZE_MAX: u32 = 0x4010;
const CONNECTION_CFG: u32 = 0x4014;
const CONNECTION_CFG_DEFAULT: u32 = 0x4018;

const TESTMODE: u32 = 0x401C;
const TEST_ERROR_COUNT_SELECTOR: u32 = 0x4020;
const TEST_ERROR_COUNT: u32 = 0x4024;
const TEST_PACKET_COUNT_TX: u32 = 0x4028;
const TEST_PACKET_COUNT_RX: u32 = 0x4030;

const VERSION_SUPPORTED: u32 = 0x4044;
const VERSION_USED: u32 = 0x4048;

// Setup const
const CHANNEL_LEN: u8 = 1;
const HOST_CONNECTION_ID: u32 = 0x00006303; // TODO: rename to CXP grabber sinara number when it comes out
// The MAX_STREAM_PAK_SIZE should be set as large as possible - Section 9.5.2 (CXP-001-2021)
// Since the ROI pipeline just consume all pixel data without buffering, any big number will do.
const MAX_STREAM_PAK_SIZE: u32 = 16384; // 16 KiB
const TX_TEST_CNT: u8 = 10;
// From DS191 (v1.18.1), max CDR time lock is 37*10^6 UI,
// 37*10^6 UI at lowest CXP linerate of 1.25Gbps = 29.6 ms, double it to account for overhead
const MONITOR_TIMEOUT_MS: u64 = 60;

pub enum Error {
    CameraNotDetected,
    ConnectionLost,
    UnstableRX,
    UnstableTX,
    UnsupportedSpeed(u32),
    UnsupportedTopology,
    UnsupportedVersion,
    CtrlPacketError(CtrlErr),
}

impl From<CtrlErr> for Error {
    fn from(value: CtrlErr) -> Error {
        Error::CtrlPacketError(value)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Error::CameraNotDetected => write!(f, "CameraNotDetected"),
            &Error::ConnectionLost => write!(f, "ConnectionLost - Channel #0 cannot be detected"),
            &Error::UnstableRX => write!(f, "UnstableRX - RX connection test failed"),
            &Error::UnstableTX => write!(f, "UnstableTX - TX connection test failed"),
            &Error::UnsupportedSpeed(linerate_code) => write!(
                f,
                "UnsupportedSpeed - {:#X} linerate code is not supported",
                linerate_code
            ),
            &Error::UnsupportedTopology => {
                write!(
                    f,
                    "UnsupportedTopology - Channel #0 should be connected to the master channel"
                )
            }
            &Error::UnsupportedVersion => write!(
                f,
                "UnsupportedVersion - Cannot find a compatible protocol version between the cxp grabber & camera"
            ),
            &Error::CtrlPacketError(ref err) => write!(f, "{}", err),
        }
    }
}

pub fn master_channel_ready() -> bool {
    unsafe { csr::cxp_grabber::core_rx_ready_read() == 1 }
}

fn monitor_channel_status_timeout(timer: GlobalTimer) -> Result<(), Error> {
    let limit = timer.get_time() + Milliseconds(MONITOR_TIMEOUT_MS);
    while timer.get_time() < limit {
        if master_channel_ready() {
            return Ok(());
        }
    }
    Err(Error::ConnectionLost)
}

pub fn discover_camera(mut timer: GlobalTimer) -> Result<(), Error> {
    // Section 7.6 (CXP-001-2021)
    // 1.25Gbps (CXP_1) and 3.125Gbps (CXP_3) are the discovery rate
    // both linerate need to be checked as camera only support ONE of discovery rates
    for speed in [CXPSpeed::CXP1, CXPSpeed::CXP3].iter() {
        // Section 12.1.2 (CXP-001-2021)
        // set tx linerate -> send ConnectionReset -> wait 200ms -> set rx linerate -> monitor connection status with a timeout
        tx::change_linerate(*speed);
        write_bytes_no_ack(CONNECTION_RESET, &1_u32.to_be_bytes(), false)?;
        timer.delay_ms(200);
        rx::change_linerate(*speed);

        if monitor_channel_status_timeout(timer).is_ok() {
            debug!("camera detected at linerate {:}", speed);
            return Ok(());
        }
    }
    Err(Error::CameraNotDetected)
}

fn check_master_channel() -> Result<(), Error> {
    if read_u32(DEVICE_CONNECTION_ID, false)? == 0 {
        Ok(())
    } else {
        Err(Error::UnsupportedTopology)
    }
}

fn disable_excess_channels(timer: GlobalTimer) -> Result<(), Error> {
    let current_cfg = read_u32(CONNECTION_CFG, false)?;
    let active_camera_chs = current_cfg >> 16;
    // After camera receive ConnectionReset, only the master connection should be active while
    // the extension connections shall not be active - Section 12.3.33 (CXP-001-2021)
    // In case some camera didn't follow the spec properly (e.g. Basler boA2448-250cm),
    // the grabber need to manually disable the excess channels
    if active_camera_chs > CHANNEL_LEN as u32 {
        debug!(
            "only {} channel(s) is available on cxp grabber, disabling excess channels on camera",
            CHANNEL_LEN
        );
        // disable excess channels and preserve the discovery linerate
        write_u32(CONNECTION_CFG, current_cfg & 0xFFFF | (CHANNEL_LEN as u32) << 16, false)?;

        // check if the master channel is down after the cfg change
        monitor_channel_status_timeout(timer)
    } else {
        Ok(())
    }
}

fn set_host_connection_id() -> Result<(), Error> {
    debug!("set host connection id to = {:#X}", HOST_CONNECTION_ID);
    write_u32(MASTER_HOST_CONNECTION_ID, HOST_CONNECTION_ID, false)?;
    Ok(())
}

fn negotiate_cxp_version() -> Result<bool, Error> {
    let rev = read_u32(REVISION, false)?;

    let mut major_rev: u32 = rev >> 16;
    let mut minor_rev: u32 = rev & 0xFF;
    debug!("camera's CoaXPress revision is {}.{}", major_rev, minor_rev);

    // Section 12.1.4 (CXP-001-2021)
    // For CXP 2.0 and onward, Host need to check the VersionSupported register to determine
    // the highest common version that supported by both device & host
    if major_rev >= 2 {
        let reg = read_u32(VERSION_SUPPORTED, false)?;

        // grabber support CXP 2.1, 2.0 and 1.1 only
        if ((reg >> 3) & 1) == 1 {
            major_rev = 2;
            minor_rev = 1;
        } else if ((reg >> 2) & 1) == 1 {
            major_rev = 2;
            minor_rev = 0;
        } else if ((reg >> 1) & 1) == 1 {
            major_rev = 1;
            minor_rev = 1;
        } else {
            return Err(Error::UnsupportedVersion);
        }

        write_u32(VERSION_USED, major_rev << 16 | minor_rev, false)?;
    }
    debug!(
        "both camera and cxp grabber support CoaXPress {}.{}, switch to CoaXPress {}.{} protocol now",
        major_rev, minor_rev, major_rev, minor_rev
    );

    Ok(major_rev >= 2)
}

fn negotiate_pak_max_size(with_tag: bool) -> Result<(), Error> {
    write_u32(STREAM_PACKET_SIZE_MAX, MAX_STREAM_PAK_SIZE, with_tag)?;
    Ok(())
}

fn decode_cxp_speed(linerate_code: u32) -> Option<CXPSpeed> {
    match linerate_code {
        0x28 => Some(CXPSpeed::CXP1),
        0x30 => Some(CXPSpeed::CXP2),
        0x38 => Some(CXPSpeed::CXP3),
        0x40 => Some(CXPSpeed::CXP5),
        0x48 => Some(CXPSpeed::CXP6),
        0x50 => Some(CXPSpeed::CXP10),
        0x58 => Some(CXPSpeed::CXP12),
        _ => None,
    }
}

fn set_operation_linerate(with_tag: bool, timer: GlobalTimer) -> Result<(), Error> {
    let recommended_linerate_code = read_u32(CONNECTION_CFG_DEFAULT, with_tag)? & 0xFFFF;

    if let Some(speed) = decode_cxp_speed(recommended_linerate_code) {
        debug!("changing linerate to {}", speed);

        // preserve the number of active channels
        let current_cfg = read_u32(CONNECTION_CFG, with_tag)?;
        write_u32(
            CONNECTION_CFG,
            current_cfg & 0xFFFF0000 | recommended_linerate_code,
            with_tag,
        )?;

        tx::change_linerate(speed);
        rx::change_linerate(speed);
        monitor_channel_status_timeout(timer)
    } else {
        Err(Error::UnsupportedSpeed(recommended_linerate_code))
    }
}

fn test_counter_reset(with_tag: bool) -> Result<(), Error> {
    unsafe { csr::cxp_grabber::core_rx_test_counts_reset_write(1) };
    write_u32(TEST_ERROR_COUNT_SELECTOR, 0, with_tag)?;
    write_u32(TEST_ERROR_COUNT, 0, with_tag)?;
    write_u64(TEST_PACKET_COUNT_TX, 0, with_tag)?;
    write_u64(TEST_PACKET_COUNT_RX, 0, with_tag)?;
    Ok(())
}

fn verify_test_result(with_tag: bool) -> Result<(), Error> {
    write_u32(TEST_ERROR_COUNT_SELECTOR, 0, with_tag)?;

    // Section 9.9.3 (CXP-001-2021)
    // verify grabber -> camera connection test result
    if read_u64(TEST_PACKET_COUNT_RX, with_tag)? != TX_TEST_CNT as u64 {
        return Err(Error::UnstableTX);
    };
    if read_u32(TEST_ERROR_COUNT, with_tag)? > 0 {
        return Err(Error::UnstableTX);
    };

    // Section 9.9.4 (CXP-001-2021)
    // verify camera -> grabber connection test result
    let camera_test_pak_cnt = read_u64(TEST_PACKET_COUNT_TX, true)?;
    unsafe {
        if csr::cxp_grabber::core_rx_test_packet_counter_read() != camera_test_pak_cnt as u16 {
            return Err(Error::UnstableRX);
        };
        if csr::cxp_grabber::core_rx_test_error_counter_read() > 0 {
            return Err(Error::UnstableRX);
        };
    };
    debug!("channel #0 passed connection test");
    Ok(())
}

fn test_channel_stability(with_tag: bool, mut timer: GlobalTimer) -> Result<(), Error> {
    test_counter_reset(with_tag)?;

    // cxp grabber -> camera connection test
    for _ in 0..TX_TEST_CNT {
        send_test_packet()?;
        // sending the whole test sequence @ 20.833Mbps will take a minimum of 1.972ms
        // and leave some room to send IDLE word
        timer.delay_ms(2);
    }

    // camera -> grabber connection test
    // enabling the TESTMODE on master channel will send test packets on all channels
    // and ctrl packet write overhead is used as a delay
    write_u32(TESTMODE, 1, with_tag)?;
    write_u32(TESTMODE, 0, with_tag)?;

    verify_test_result(with_tag)?;

    Ok(())
}

pub fn camera_setup(timer: GlobalTimer) -> Result<bool, Error> {
    reset_tag();
    check_master_channel()?;

    disable_excess_channels(timer)?;
    set_host_connection_id()?;
    let with_tag = negotiate_cxp_version()?;

    negotiate_pak_max_size(with_tag)?;
    set_operation_linerate(with_tag, timer)?;

    test_channel_stability(with_tag, timer)?;

    Ok(with_tag)
}
