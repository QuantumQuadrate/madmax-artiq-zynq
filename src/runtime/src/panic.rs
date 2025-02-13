#[cfg(feature = "target_kasli_soc")]
use libboard_zynq::error_led::ErrorLED;
use libboard_zynq::{print, println, timer::GlobalTimer};
use libconfig::Config;
use libcortex_a9::regs::MPIDR;
use libregister::RegisterR;
use log::error;
use unwind::backtrace;

use crate::comms::soft_panic_main;

static mut PANICKED: [bool; 2] = [false; 2];
static mut SOFT_PANICKED: bool = false;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let id = MPIDR.read().cpu_id() as usize;
    let soft_panicked = unsafe { SOFT_PANICKED };
    print!("Core {} panic at ", id);
    if let Some(location) = info.location() {
        print!("{}:{}:{}", location.file(), location.line(), location.column());
    } else {
        print!("unknown location");
    }
    if let Some(message) = info.message() {
        println!(": {}", message);
    } else {
        println!("");
    }
    unsafe {
        // soft panics only allowed for core 0
        if PANICKED[id] && (SOFT_PANICKED || id == 1) {
            println!("nested panic!");
            loop {}
        }
        SOFT_PANICKED = true;
        PANICKED[id] = true;
    }
    #[cfg(feature = "target_kasli_soc")]
    {
        let mut err_led = ErrorLED::error_led();
        err_led.toggle(true);
    }
    println!("Backtrace: ");
    let _ = backtrace(|ip| {
        // Backtrace gives us the return address, i.e. the address after the delay slot,
        // but we're interested in the call instruction.
        print!("{:#08x} ", ip - 2 * 4);
    });
    println!("\nEnd backtrace");
    if !soft_panicked && id == 0 {
        soft_panic(info);
    }
    loop {}
}

fn soft_panic(info: &core::panic::PanicInfo) -> ! {
    // write panic info to log, so coremgmt can also read it
    if let Some(location) = info.location() {
        error!("panic at {}:{}:{}", location.file(), location.line(), location.column());
    } else {
        error!("panic at unknown location");
    }
    if let Some(message) = info.message() {
        error!("panic message: {}", message);
    }
    let timer = GlobalTimer::start();
    let cfg = match Config::new() {
        Ok(cfg) => cfg,
        Err(_) => Config::new_dummy(),
    };
    soft_panic_main(timer, cfg);
}

#[lang = "eh_personality"]
#[no_mangle]
pub extern "C" fn rust_eh_personality() {}
