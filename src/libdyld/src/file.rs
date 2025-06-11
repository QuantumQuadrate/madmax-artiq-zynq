use core::{mem,
           ops::{Deref, Range},
           ptr};

use super::{Arch, elf::*};

fn read_unaligned<T: Copy>(data: &[u8], offset: usize) -> Option<T> {
    if data.len() < offset + mem::size_of::<T>() {
        None
    } else {
        let ptr = data.as_ptr().wrapping_offset(offset as isize) as *const T;
        Some(unsafe { ptr::read_unaligned(ptr) })
    }
}

/// ELF file
pub struct File<'a> {
    pub ehdr: Elf32_Ehdr,
    data: &'a [u8],
}

impl<'a> File<'a> {
    pub fn new(data: &'a [u8]) -> Option<Self> {
        let ehdr = read_unaligned(data, 0)?;
        Some(File { ehdr, data })
    }

    fn read_unaligned<T: Copy>(&self, offset: usize) -> Option<T> {
        read_unaligned(self.data, offset)
    }

    pub fn arch(&self) -> Option<Arch> {
        const IDENT_OPENRISC: [u8; EI_NIDENT] = [
            ELFMAG0,
            ELFMAG1,
            ELFMAG2,
            ELFMAG3,
            ELFCLASS32,
            ELFDATA2MSB,
            EV_CURRENT,
            ELFOSABI_NONE,
            /* ABI version */ 0,
            /* padding */ 0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        const IDENT_ARM: [u8; EI_NIDENT] = [
            ELFMAG0,
            ELFMAG1,
            ELFMAG2,
            ELFMAG3,
            ELFCLASS32,
            ELFDATA2LSB,
            EV_CURRENT,
            ELFOSABI_NONE,
            /* ABI version */ 0,
            /* padding */ 0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];

        match (self.ehdr.e_ident, self.ehdr.e_machine) {
            (IDENT_ARM, EM_ARM) => Some(Arch::Arm),
            (IDENT_OPENRISC, EM_OPENRISC) => Some(Arch::OpenRisc),
            _ => None,
        }
    }

    pub fn program_headers<'b>(&'b self) -> impl Iterator<Item = Option<Elf32_Phdr>> + 'b {
        (0..self.ehdr.e_phnum).map(move |i| {
            let phdr_off = self.ehdr.e_phoff as usize + mem::size_of::<Elf32_Phdr>() * i as usize;
            self.read_unaligned::<Elf32_Phdr>(phdr_off)
        })
    }

    pub fn section_headers<'b>(&'b self) -> impl Iterator<Item = Option<Elf32_Shdr>> + 'b {
        (0..self.ehdr.e_shnum).map(move |i| {
            let shdr_off = self.ehdr.e_shoff as usize + mem::size_of::<Elf32_Shdr>() * i as usize;
            self.read_unaligned::<Elf32_Shdr>(shdr_off)
        })
    }

    pub fn dyn_header_vaddr(&self) -> Option<Range<usize>> {
        self.program_headers()
            .filter_map(|phdr| phdr)
            .find(|phdr| phdr.p_type == PT_DYNAMIC)
            .map(|phdr| phdr.p_vaddr as usize..(phdr.p_vaddr + phdr.p_filesz) as usize)
    }
}

impl Deref for File<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.data
    }
}
