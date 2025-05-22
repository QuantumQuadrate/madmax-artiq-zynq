use core::{arch::{asm, naked_asm},
           sync::atomic::{AtomicBool, Ordering}};

use libboard_zynq::{gic, mpcore, println, stdio};
use libcortex_a9::{asm, interrupt_handler, notify_spin_lock, regs::MPIDR, spin_lock_yield};
use libregister::RegisterR;

extern "C" {
    static mut __stack1_start: u32;
    fn main_core1() -> !;
}

static CORE1_RESTART: AtomicBool = AtomicBool::new(false);

interrupt_handler!(IRQ, irq, __irq_stack0_start, __irq_stack1_start, {
    if MPIDR.read().cpu_id() == 1 {
        let mpcore = mpcore::RegisterBlock::mpcore();
        let mut gic = gic::InterruptController::gic(mpcore);
        let id = gic.get_interrupt_id();
        if id.0 == 0 {
            gic.end_interrupt(id);
            asm::exit_irq();
            asm!("b core1_restart");
        }
    }
    stdio::drop_uart();
    println!("IRQ");
    loop {}
});

// This is actually not an interrupt handler, just use the macro for convenience.
// This function would be called in normal mode (instead of interrupt mode), the outer naked
// function wrapper is to tell libunwind to stop when it reaches here.
interrupt_handler!(core1_restart, core1_restart_impl, __stack0_start, __stack1_start, {
    asm::enable_irq();
    CORE1_RESTART.store(false, Ordering::Relaxed);
    notify_spin_lock();
    main_core1();
});

pub fn restart_core1() {
    let mut interrupt_controller = gic::InterruptController::gic(mpcore::RegisterBlock::mpcore());
    CORE1_RESTART.store(true, Ordering::Relaxed);
    interrupt_controller.send_sgi(gic::InterruptId(0), gic::CPUCore::Core1.into());
    while CORE1_RESTART.load(Ordering::Relaxed) {
        spin_lock_yield();
    }
}
