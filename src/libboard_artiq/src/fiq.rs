use core::arch::asm;

use libboard_zynq::{println, stdio};
use libcortex_a9::{interrupt_handler, regs::MPIDR};
use libregister::RegisterR;

#[cfg(has_si549)]
use crate::si549;

interrupt_handler!(FIQ, fiq, __irq_stack0_start, __irq_stack1_start, {
    match MPIDR.read().cpu_id() {
        0 => {
            // nFIQ is driven directly and bypass GIC
            #[cfg(has_si549)]
            si549::wrpll::interrupt_handler();
            return;
        }
        _ => {}
    };

    stdio::drop_uart();
    println!("FIQ");
    loop {}
});
