use alloc::alloc::{Layout, LayoutError, alloc_zeroed, dealloc};
use core::{mem,
           ops::{Deref, DerefMut, Range},
           slice};

use super::{Error, elf::*};

pub struct DynamicSection {
    pub strtab: Range<usize>,
    pub symtab: Range<usize>,
    pub hash: Range<usize>,
    pub hash_bucket: Range<usize>,
    pub hash_chain: Range<usize>,
    pub rel: Range<usize>,
    pub rela: Range<usize>,
    pub pltrel: Range<usize>,
}

/// target memory image
pub struct Image {
    layout: Layout,
    pub data: &'static mut [u8],
}

impl Image {
    pub fn new(size: usize, align: usize) -> Result<Self, LayoutError> {
        let layout = Layout::from_size_align(size, align)?;
        let data = unsafe {
            let ptr = alloc_zeroed(layout);
            slice::from_raw_parts_mut(ptr, size)
        };

        Ok(Image { layout, data })
    }

    /// assumes that self.data is properly aligned
    pub(crate) fn get_ref<T>(&self, offset: usize) -> Option<&T>
    where T: Copy {
        if self.data.len() < offset + mem::size_of::<T>() {
            None
        } else if (self.data.as_ptr() as usize + offset) & (mem::align_of::<T>() - 1) != 0 {
            None
        } else {
            let ptr = self.data.as_ptr().wrapping_offset(offset as isize) as *const T;
            Some(unsafe { &*ptr })
        }
    }

    /// assumes that self.data is properly aligned
    ///
    /// range: in bytes
    pub(crate) fn get_ref_slice_unchecked<T: Copy>(&self, range: &Range<usize>) -> &[T] {
        let offset = range.start;
        let len = (range.end - range.start) / mem::size_of::<T>();

        let ptr = self.data.as_ptr().wrapping_offset(offset as isize) as *const T;
        unsafe { slice::from_raw_parts(ptr, len) }
    }

    fn dyn_headers<'a>(&'a self, range: Range<usize>) -> impl Iterator<Item = &'a Elf32_Dyn> + 'a {
        range
            .step_by(mem::size_of::<Elf32_Dyn>())
            .filter_map(move |offset| self.get_ref::<Elf32_Dyn>(offset))
            .take_while(|d| unsafe { d.d_un.d_val } as i32 != DT_NULL)
    }

    pub fn dyn_section(&self, range: Range<usize>) -> Result<DynamicSection, Error> {
        let (mut strtab_off, mut strtab_sz) = (0, 0);
        let (mut rel_off, mut rel_sz) = (0, 0);
        let (mut rela_off, mut rela_sz) = (0, 0);
        let (mut pltrel_off, mut pltrel_sz) = (0, 0);
        let (mut hash_off, mut hash_sz) = (0, 0);
        let mut symtab_off = 0;
        let mut sym_ent = 0;
        let mut rel_ent = 0;
        let mut rela_ent = 0;
        let mut nbucket = 0;
        let mut nchain = 0;

        for dyn_header in self.dyn_headers(range) {
            let val = unsafe { dyn_header.d_un.d_val } as usize;
            match dyn_header.d_tag {
                DT_NULL => break,
                DT_STRTAB => strtab_off = val,
                DT_STRSZ => strtab_sz = val,
                DT_SYMTAB => symtab_off = val,
                DT_SYMENT => sym_ent = val,
                DT_REL => rel_off = val,
                DT_RELSZ => rel_sz = val,
                DT_RELENT => rel_ent = val,
                DT_RELA => rela_off = val,
                DT_RELASZ => rela_sz = val,
                DT_RELAENT => rela_ent = val,
                DT_JMPREL => pltrel_off = val,
                DT_PLTRELSZ => pltrel_sz = val,
                DT_HASH => {
                    nbucket = *self
                        .get_ref::<Elf32_Word>(val + 0)
                        .ok_or("cannot read hash bucket count")? as usize;
                    nchain = *self
                        .get_ref::<Elf32_Word>(val + 4)
                        .ok_or("cannot read hash chain count")? as usize;
                    hash_off = val + 8;
                    hash_sz = (nbucket + nchain) * mem::size_of::<Elf32_Word>();
                }
                _ => (),
            }
        }

        // These are the same--there are as many chains as buckets, and the chains only contain
        // the symbols that overflowed the bucket.
        let symtab_sz = nchain * mem::size_of::<Elf32_Sym>();

        if strtab_off + strtab_sz > self.data.len() {
            return Err("invalid strtab offset/size")?;
        }
        if symtab_off + symtab_sz > self.data.len() {
            return Err("invalid symtab offset/size")?;
        }
        if sym_ent != mem::size_of::<Elf32_Sym>() {
            return Err("incorrect symbol entry size")?;
        }
        if rel_off + rel_sz > self.data.len() {
            return Err("invalid rel offset/size")?;
        }
        if rel_ent != 0 && rel_ent != mem::size_of::<Elf32_Rel>() {
            return Err("incorrect relocation entry size")?;
        }
        if rela_off + rela_sz > self.data.len() {
            return Err("invalid rela offset/size")?;
        }
        if rela_ent != 0 && rela_ent != mem::size_of::<Elf32_Rela>() {
            return Err("incorrect relocation entry size")?;
        }
        if pltrel_off + pltrel_sz > self.data.len() {
            return Err("invalid pltrel offset/size")?;
        }

        Ok(DynamicSection {
            strtab: strtab_off..strtab_off + strtab_sz,
            symtab: symtab_off..symtab_off + symtab_sz,
            hash: hash_off..hash_off + hash_sz,
            hash_bucket: 0..nbucket,
            hash_chain: nbucket..nbucket + nchain,
            rel: rel_off..rel_off + rel_sz,
            rela: rela_off..rela_off + rela_sz,
            pltrel: pltrel_off..pltrel_off + pltrel_sz,
        })
    }

    pub fn ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    pub fn write(&self, offset: usize, value: Elf32_Word) -> Result<(), Error> {
        if offset + mem::size_of::<Elf32_Addr>() > self.data.len() {
            return Err("relocation out of image bounds")?;
        }

        let ptr = (self.data.as_ptr() as usize + offset) as *mut Elf32_Addr;
        Ok(unsafe { *ptr = value })
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.data.as_mut_ptr(), self.layout);
        }
    }
}

impl Deref for Image {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.data
    }
}

impl DerefMut for Image {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.data
    }
}
