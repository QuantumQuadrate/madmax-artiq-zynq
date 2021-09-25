use alloc::string::String;
use log::trace;
use super::{
    Arch,
    elf::*,
    Error,
    image::Image,
    Library,
};
use libcortex_a9::{
    cache::{dcci_slice, iciallu, bpiall},
    asm::{dsb, isb},
};

pub trait Relocatable {
    fn offset(&self) -> usize;
    fn type_info(&self) -> u8;
    fn sym_info(&self) -> u32;
    fn addend(&self, image: &Image) -> i32;
}

impl Relocatable for Elf32_Rel {
    fn offset(&self) -> usize {
        self.r_offset as usize
    }

    fn type_info(&self) -> u8 {
        ELF32_R_TYPE(self.r_info)
    }

    fn sym_info(&self) -> u32 {
        ELF32_R_SYM(self.r_info)
    }

    fn addend(&self, image: &Image) -> i32 {
        *image.get_ref(self.offset()).unwrap()
    }
}

impl Relocatable for Elf32_Rela {
    fn offset(&self) -> usize {
        self.r_offset as usize
    }

    fn type_info(&self) -> u8 {
        ELF32_R_TYPE(self.r_info)
    }

    fn sym_info(&self) -> u32 {
        ELF32_R_SYM(self.r_info)
    }

    fn addend(&self, _: &Image) -> i32 {
        self.r_addend
    }
}

#[derive(Clone, Copy, Debug)]
enum RelType {
    None,
    Relative,
    Lookup,
}

impl RelType {
    pub fn new(arch: Arch, type_info: u8) -> Option<Self> {
        match type_info {
            R_OR1K_NONE if arch == Arch::OpenRisc =>
                Some(RelType::None),
            R_ARM_NONE if arch == Arch::Arm =>
                Some(RelType::None),

            R_OR1K_RELATIVE if arch == Arch::OpenRisc =>
                Some(RelType::Relative),
            R_ARM_RELATIVE if arch == Arch::Arm =>
                Some(RelType::Relative),

            R_OR1K_32 | R_OR1K_GLOB_DAT | R_OR1K_JMP_SLOT
                if arch == Arch::OpenRisc => Some(RelType::Lookup),
            R_ARM_GLOB_DAT | R_ARM_JUMP_SLOT
                if arch == Arch::Arm => Some(RelType::Lookup),

            _ =>
                None
        }
    }
}

fn format_sym_name(sym_name: &[u8]) -> String {
    core::str::from_utf8(sym_name)
        .map(String::from)
        .unwrap_or(String::from("<invalid symbol name>"))
}

pub fn relocate<R: Relocatable>(
    arch: Arch, lib: &Library,
    rel: &R, resolve: &dyn Fn(&[u8]) -> Option<Elf32_Word>
) -> Result<(), Error> {
    let sym;
    if rel.sym_info() == 0 {
        sym = None;
    } else {
        sym = Some(lib.symtab().get(rel.sym_info() as usize)
                   .ok_or("symbol out of bounds of symbol table")?)
    }

    let rel_type = RelType::new(arch, rel.type_info())
        .ok_or("unsupported relocation type")?;
    let value;
    match rel_type {
        RelType::None =>
            return Ok(()),

        RelType::Relative => {
            let addend = rel.addend(&lib.image);
            value = lib.image.ptr().wrapping_offset(addend as isize) as Elf32_Word;
        }

        RelType::Lookup => {
            let sym = sym.ok_or("relocation requires an associated symbol")?;
            let sym_name = lib.name_starting_at(sym.st_name as usize)?;

            if let Some(addr) = lib.lookup(sym_name) {
                // First, try to resolve against itself.
                trace!("looked up symbol {} in image", format_sym_name(sym_name));
                value = addr;
            } else if let Some(addr) = resolve(sym_name) {
                // Second, call the user-provided function.
                trace!("resolved symbol {:?}", format_sym_name(sym_name));
                value = addr;
            } else {
                // We couldn't find it anywhere.
                return Err(Error::Lookup(format_sym_name(sym_name)))
            }
        }
    }

    lib.image.write(rel.offset(), value)
}

pub fn rebind(
    arch: Arch, lib: &Library, name: &[u8], value: Elf32_Word
) -> Result<(), Error> {
    for rela in lib.pltrel() {
        let rel_type = RelType::new(arch, rela.type_info())
            .ok_or("unsupported relocation type")?;
        match rel_type {
            RelType::Lookup => {
                let sym = lib.symtab().get(ELF32_R_SYM(rela.r_info) as usize)
                    .ok_or("symbol out of bounds of symbol table")?;
                let sym_name = lib.name_starting_at(sym.st_name as usize)?;

                if sym_name == name {
                    lib.image.write(rela.offset(), value)?
                }
            }
            // No associated symbols for other relocation types.
            _ => {}
        }
    }
    // FIXME: the cache maintainance operations may be more than enough,
    // may cause performance degradation.
    dcci_slice(lib.image.data);
    iciallu();
    bpiall();
    dsb();
    isb();

    Ok(())
}
