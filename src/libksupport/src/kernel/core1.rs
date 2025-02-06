//! Kernel prologue/epilogue that runs on the 2nd CPU core

use alloc::borrow::ToOwned;
use core::{cell::UnsafeCell, mem, ptr};

use cslice::CSlice;
use dyld::{self, elf::EXIDX_Entry, Library};
use libboard_zynq::{gic, mpcore};
use libcortex_a9::{asm::{dsb, isb},
                   cache::{bpiall, dcci_slice, iciallu},
                   enable_fpu, sync_channel};
use libsupport_zynq::ram;
use log::{debug, error, info};

use super::{api::resolve, dma, rpc::rpc_send_async, Message, CHANNEL_0TO1, CHANNEL_1TO0, CHANNEL_SEM, INIT_LOCK,
            KERNEL_CHANNEL_0TO1, KERNEL_CHANNEL_1TO0, KERNEL_IMAGE};
use crate::{eh_artiq, get_async_errors, rtio};

// linker symbols
extern "C" {
    static __text_start: u32;
    static __text_end: u32;
    static __exidx_start: EXIDX_Entry;
    static __exidx_end: EXIDX_Entry;
}

unsafe fn attribute_writeback(typeinfo: *const ()) {
    #[repr(C)]
    struct Attr {
        offset: usize,
        tag: CSlice<'static, u8>,
        name: CSlice<'static, u8>,
    }

    #[repr(C)]
    struct Type {
        attributes: *const *const Attr,
        objects: *const *const (),
    }

    let mut tys = typeinfo as *const *const Type;
    while !(*tys).is_null() {
        let ty = *tys;
        tys = tys.offset(1);

        let mut objects = (*ty).objects;
        while !(*objects).is_null() {
            let object = *objects;
            objects = objects.offset(1);

            let mut attributes = (*ty).attributes;
            while !(*attributes).is_null() {
                let attribute = *attributes;
                attributes = attributes.offset(1);

                if (*attribute).tag.len() > 0 {
                    rpc_send_async(
                        0,
                        &(*attribute).tag,
                        [
                            &object as *const _ as *const (),
                            &(*attribute).name as *const _ as *const (),
                            (object as usize + (*attribute).offset) as *const (),
                        ]
                        .as_ptr(),
                    );
                }
            }
        }
    }
}

pub struct KernelImage {
    library: UnsafeCell<Library>,
    __modinit__: u32,
    typeinfo: Option<u32>,
}

impl KernelImage {
    pub fn new(library: Library) -> Result<Self, dyld::Error> {
        let __modinit__ = library
            .lookup(b"__modinit__")
            .ok_or(dyld::Error::Lookup("__modinit__".to_owned()))?;
        let typeinfo = library.lookup(b"typeinfo");

        // clear .bss
        let bss_start = library.lookup(b"__bss_start");
        let end = library.lookup(b"_end");
        if let Some(bss_start) = bss_start {
            let end = end.ok_or(dyld::Error::Lookup("_end".to_owned()))?;
            unsafe {
                ptr::write_bytes(bss_start as *mut u8, 0, (end - bss_start) as usize);
            }
        }

        Ok(KernelImage {
            library: UnsafeCell::new(library),
            __modinit__,
            typeinfo,
        })
    }

    pub unsafe fn rebind(&self, name: &[u8], addr: *const ()) -> Result<(), dyld::Error> {
        let library = self.library.get().as_mut().unwrap();
        library.rebind(name, addr)
    }

    pub unsafe fn exec(&self) {
        // Flush data cache entries for the image in DDR, including
        // Memory/Instruction Synchronization Barriers
        dcci_slice(self.library.get().as_ref().unwrap().image.data);
        iciallu();
        bpiall();
        dsb();
        isb();

        (mem::transmute::<u32, extern "C" fn()>(self.__modinit__))();

        if let Some(typeinfo) = self.typeinfo {
            attribute_writeback(typeinfo as *const ());
        }
    }

    pub fn get_load_addr(&self) -> usize {
        unsafe { self.library.get().as_ref().unwrap().image.as_ptr() as usize }
    }
}

#[no_mangle]
pub extern "C" fn main_core1() {
    enable_fpu();
    debug!("Core1 started");

    ram::init_alloc_core1();
    gic::InterruptController::gic(mpcore::RegisterBlock::mpcore()).enable_interrupts();

    let (mut core0_tx, mut core1_rx) = sync_channel!(Message, 4);
    let (mut core1_tx, core0_rx) = sync_channel!(Message, 4);
    unsafe {
        INIT_LOCK.lock();
        core0_tx.reset();
        core1_tx.reset();
        if !KERNEL_IMAGE.is_null() {
            // indicates forceful termination of previous kernel
            KERNEL_IMAGE = core::ptr::null();
            debug!("rtio init");
            rtio::init();
        }
        dma::init_dma_recorder();
    }
    *CHANNEL_0TO1.lock() = Some(core0_tx);
    *CHANNEL_1TO0.lock() = Some(core0_rx);
    CHANNEL_SEM.signal();

    // set on load, cleared on start
    let mut loaded_kernel = None;
    loop {
        let message = core1_rx.recv();
        match message {
            Message::LoadRequest(data) => {
                let result = dyld::load(&data, &resolve).and_then(KernelImage::new);
                match result {
                    Ok(kernel) => {
                        loaded_kernel = Some(kernel);
                        debug!("kernel loaded");
                        core1_tx.send(Message::LoadCompleted);
                    }
                    Err(error) => {
                        error!("failed to load shared library: {}", error);
                        core1_tx.send(Message::LoadFailed);
                    }
                }
            }
            Message::StartRequest => {
                info!("kernel starting");
                if let Some(kernel) = loaded_kernel.take() {
                    unsafe {
                        eh_artiq::reset_exception_buffer();
                        KERNEL_CHANNEL_0TO1 = Some(core1_rx);
                        KERNEL_CHANNEL_1TO0 = Some(core1_tx);
                        KERNEL_IMAGE = &kernel as *const KernelImage;
                        kernel.exec();
                        KERNEL_IMAGE = ptr::null();
                        core1_rx = KERNEL_CHANNEL_0TO1.take().unwrap();
                        core1_tx = KERNEL_CHANNEL_1TO0.take().unwrap();
                    }
                }
                info!("kernel finished");
                let async_errors = unsafe { get_async_errors() };
                core1_tx.send(Message::KernelFinished(async_errors));
            }
            _ => error!("Core1 received unexpected message: {:?}", message),
        }
    }
}

/// Called by eh_artiq
pub fn terminate(
    exceptions: &'static [Option<eh_artiq::Exception<'static>>],
    stack_pointers: &'static [eh_artiq::StackPointerBacktrace],
    backtrace: &'static mut [(usize, usize)],
) -> ! {
    {
        let core1_tx = unsafe { KERNEL_CHANNEL_1TO0.as_mut().unwrap() };
        let errors = unsafe { get_async_errors() };
        core1_tx.send(Message::KernelException(exceptions, stack_pointers, backtrace, errors));
    }
    loop {}
}

/// Called by llvm_libunwind
#[no_mangle]
extern "C" fn dl_unwind_find_exidx(pc: *const u32, len_ptr: *mut u32) -> *const u32 {
    let length;
    let start: *const EXIDX_Entry;
    unsafe {
        if &__text_start as *const u32 <= pc && pc < &__text_end as *const u32 {
            length = (&__exidx_end as *const EXIDX_Entry).offset_from(&__exidx_start) as u32;
            start = &__exidx_start;
        } else if KERNEL_IMAGE != ptr::null() {
            let exidx = KERNEL_IMAGE
                .as_ref()
                .expect("dl_unwind_find_exidx kernel image")
                .library
                .get()
                .as_ref()
                .unwrap()
                .exidx();
            length = exidx.len() as u32;
            start = exidx.as_ptr();
        } else {
            length = 0;
            start = ptr::null();
        }
        *len_ptr = length;
    }
    start as *const u32
}

pub extern "C" fn rtio_get_destination_status(destination: i32) -> bool {
    #[cfg(has_drtio)]
    if destination > 0 && destination < 255 {
        let reply = unsafe {
            let core1_rx = KERNEL_CHANNEL_0TO1.as_mut().unwrap();
            let core1_tx = KERNEL_CHANNEL_1TO0.as_mut().unwrap();
            core1_tx.send(Message::UpDestinationsRequest(destination));
            core1_rx.recv()
        };
        return match reply {
            Message::UpDestinationsReply(x) => x,
            _ => panic!("received unexpected reply to UpDestinationsRequest: {:?}", reply),
        };
    }

    destination == 0
}
