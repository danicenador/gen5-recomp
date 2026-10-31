//! The ARM9 address space as 4 KiB pages of host memory.
//!
//! Recompiled code reads and writes RAM through the page tables directly;
//! pages left null (I/O, unmapped space) go through the runtime. Mirrors are
//! modelled at page granularity, which is exact for every region except
//! palette and OAM (2 KiB, stored in a 4 KiB page each).

pub const PAGE_SHIFT: u32 = 12;
const PAGE_COUNT: usize = 1 << (32 - PAGE_SHIFT);

pub const MAIN_RAM_SIZE: usize = 4 * 1024 * 1024;
const ITCM_SIZE: usize = 32 * 1024;
const DTCM_SIZE: usize = 16 * 1024;
const SHARED_WRAM_SIZE: usize = 32 * 1024;
const VRAM_SIZE: usize = 1024 * 1024;
const PAGE: usize = 1 << PAGE_SHIFT;

pub struct Memory {
    pub main: Vec<u8>,
    pub itcm: Vec<u8>,
    pub dtcm: Vec<u8>,
    pub shared_wram: Vec<u8>,
    pub palette: Vec<u8>,
    pub vram: Vec<u8>,
    pub oam: Vec<u8>,
    pub bios: Vec<u8>,
    pub read_pages: Vec<*mut u8>,
    pub write_pages: Vec<*mut u8>,
    /// Tightly coupled memory layout, set through CP15.
    pub itcm_enabled: bool,
    pub itcm_end: u32,
    pub dtcm_enabled: bool,
    pub dtcm_base: u32,
    pub dtcm_size: u32,
}

impl Memory {
    pub fn new() -> Self {
        let mut memory = Self {
            main: vec![0; MAIN_RAM_SIZE],
            itcm: vec![0; ITCM_SIZE],
            dtcm: vec![0; DTCM_SIZE],
            shared_wram: vec![0; SHARED_WRAM_SIZE],
            palette: vec![0; PAGE],
            vram: vec![0; VRAM_SIZE],
            oam: vec![0; PAGE],
            bios: vec![0; 32 * 1024],
            read_pages: vec![std::ptr::null_mut(); PAGE_COUNT],
            write_pages: vec![std::ptr::null_mut(); PAGE_COUNT],
            // The ARM9 BIOS leaves ITCM over the first 32 MiB and DTCM at
            // 0x00800000; the game's startup code moves DTCM.
            itcm_enabled: true,
            itcm_end: 0x0200_0000,
            dtcm_enabled: true,
            dtcm_base: 0x0080_0000,
            dtcm_size: DTCM_SIZE as u32,
        };
        memory.remap();
        memory
    }

    /// Rebuild the page tables after a layout change.
    pub fn remap(&mut self) {
        self.read_pages.fill(std::ptr::null_mut());
        self.write_pages.fill(std::ptr::null_mut());
        let map = |pages: &mut Vec<*mut u8>, start: u32, end: u32, buffer: &mut Vec<u8>| {
            let mask = buffer.len() as u32 - 1;
            let base = buffer.as_mut_ptr();
            for page in (start >> PAGE_SHIFT)..(end >> PAGE_SHIFT) {
                let offset = ((page << PAGE_SHIFT) & mask) as usize;
                // SAFETY: `offset` is inside `buffer`, which lives as long as
                // `self` and is never reallocated.
                pages[page as usize] = unsafe { base.add(offset) };
            }
        };
        for pages in [&mut self.read_pages, &mut self.write_pages] {
            map(pages, 0x0200_0000, 0x0300_0000, &mut self.main);
            map(pages, 0x0300_0000, 0x0400_0000, &mut self.shared_wram);
            map(pages, 0x0500_0000, 0x0600_0000, &mut self.palette);
            map(pages, 0x0600_0000, 0x0700_0000, &mut self.vram);
            map(pages, 0x0700_0000, 0x0800_0000, &mut self.oam);
            if self.itcm_enabled {
                map(pages, 0, self.itcm_end, &mut self.itcm);
            }
            if self.dtcm_enabled {
                let end = self
                    .dtcm_base
                    .saturating_add(self.dtcm_size.max(PAGE as u32));
                map(pages, self.dtcm_base, end, &mut self.dtcm);
            }
        }
        map(
            &mut self.read_pages,
            0xFFFF_0000,
            0xFFFF_F000,
            &mut self.bios,
        );
        // `map` cannot reach the last page because `end` would overflow.
        self.read_pages[PAGE_COUNT - 1] = unsafe { self.bios.as_mut_ptr().add(0x7000) };
    }

    /// The host byte for a guest address, if the address is mapped.
    pub fn byte(&self, address: u32, write: bool) -> Option<*mut u8> {
        let pages = if write {
            &self.write_pages
        } else {
            &self.read_pages
        };
        let page = pages[(address >> PAGE_SHIFT) as usize];
        // SAFETY: page pointers point at least PAGE bytes into a buffer.
        (!page.is_null()).then(|| unsafe { page.add((address & (PAGE as u32 - 1)) as usize) })
    }

    pub fn main_offset(address: u32) -> usize {
        (address as usize) & (MAIN_RAM_SIZE - 1)
    }
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_ram_mirrors_and_dtcm_overrides() {
        let mut memory = Memory::new();
        memory.main[0x3F_FE00] = 0xAB;
        // SAFETY: mapped addresses point into live buffers.
        unsafe {
            assert_eq!(*memory.byte(0x027F_FE00, false).unwrap(), 0xAB);
            assert_eq!(*memory.byte(0x023F_FE00, false).unwrap(), 0xAB);
        }
        memory.dtcm_base = 0x027E_0000;
        memory.remap();
        memory.dtcm[0x10] = 0x55;
        unsafe {
            assert_eq!(*memory.byte(0x027E_0010, false).unwrap(), 0x55);
        }
        assert!(memory.byte(0x0400_0000, false).is_none());
        assert!(memory.byte(0xFFFF_0000, true).is_none());
        assert!(memory.byte(0xFFFF_FFFC, false).is_some());
    }
}
