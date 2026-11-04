//! A minimal flattened device tree (DTB) reader.
//!
//! The kernel needs exactly one thing from the device tree QEMU installs at
//! the start of RAM: the `/memory` node's `reg` property, to size the heap.
//! This module walks the structure block to find it and nothing more. Every
//! read is bounds-checked against the blob the caller handed over, every
//! failure is an `Option::None`, and no loop runs longer than the blob has
//! tokens, so a malformed, truncated or hostile blob cannot panic or hang the
//! kernel; the caller then falls back to the linker's RAM size.
//!
//! Format reference: devicetree specification v0.4, chapter 5. All header
//! and structure words are big-endian `u32`s; `reg` cells are big-endian and
//! their widths come from the root node's `#address-cells` and
//! `#size-cells` (defaults 2 and 1).

use core::ops::Range;

/// Big-endian magic at offset 0 of every DTB.
pub const FDT_MAGIC: u32 = 0xd00d_feed;

/// Largest blob the reader accepts (QEMU's buffer is 1 MiB). Keeps a bad
/// `totalsize` from steering reads outside memory the caller owns.
pub const MAX_SIZE: usize = 2 << 20;

const HEADER_LEN: usize = 40;

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// True if `word` (as read from memory) is the DTB magic.
pub fn is_fdt_magic(word_in_memory: u32) -> bool {
    u32::from_be(word_in_memory) == FDT_MAGIC
}

/// One `reg` entry of the memory node: `[start, end)` as physical addresses.
pub type MemoryRegion = Range<u64>;

/// A parsed, validated view over a device tree blob.
pub struct Fdt<'a> {
    blob: &'a [u8],
    structs: Range<usize>,
    strings: Range<usize>,
}

fn be32(bytes: &[u8], off: usize) -> Option<u32> {
    let b = bytes.get(off..off.checked_add(4)?)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

impl<'a> Fdt<'a> {
    /// Validate the header of a blob held in `bytes`. `bytes` may be longer
    /// than the blob; only `totalsize` bytes are used afterwards.
    pub fn from_bytes(bytes: &'a [u8]) -> Option<Fdt<'a>> {
        if be32(bytes, 0)? != FDT_MAGIC {
            return None;
        }
        let total = be32(bytes, 4)? as usize;
        if !(HEADER_LEN..=MAX_SIZE).contains(&total) || total > bytes.len() {
            return None;
        }
        let blob = &bytes[..total];
        let off_struct = be32(blob, 8)? as usize;
        let off_strings = be32(blob, 12)? as usize;
        let version = be32(blob, 20)?;
        let size_strings = be32(blob, 32)? as usize;
        let size_struct = be32(blob, 36)? as usize;
        if version < 17 {
            // Older versions have no size_dt_struct; refuse rather than guess.
            return None;
        }
        let structs = off_struct..off_struct.checked_add(size_struct)?;
        let strings = off_strings..off_strings.checked_add(size_strings)?;
        if structs.end > total || strings.end > total || !off_struct.is_multiple_of(4) {
            return None;
        }
        Some(Fdt {
            blob,
            structs,
            strings,
        })
    }

    /// Read a blob from memory. `max_len` bounds every access: nothing at or
    /// beyond `ptr + max_len` is touched, whatever the header says.
    ///
    /// # Safety
    ///
    /// `[ptr, ptr + max_len)` must be readable memory that is not written
    /// for the lifetime of the returned value.
    pub unsafe fn from_raw(ptr: *const u8, max_len: usize) -> Option<Fdt<'static>> {
        if ptr.is_null() || max_len < HEADER_LEN {
            return None;
        }
        // Read only the header first so a bogus pointer with a sane limit
        // still touches the minimum.
        let header = core::slice::from_raw_parts(ptr, HEADER_LEN);
        if be32(header, 0)? != FDT_MAGIC {
            return None;
        }
        let total = (be32(header, 4)? as usize).min(max_len);
        Fdt::from_bytes(core::slice::from_raw_parts(ptr, total))
    }

    /// `totalsize` from the header.
    pub fn total_size(&self) -> usize {
        self.blob.len()
    }

    fn token(&self, off: usize) -> Option<u32> {
        if off < self.structs.start || off.checked_add(4)? > self.structs.end {
            return None;
        }
        be32(self.blob, off)
    }

    fn string(&self, nameoff: usize) -> Option<&'a [u8]> {
        let start = self.strings.start.checked_add(nameoff)?;
        if start >= self.strings.end {
            return None;
        }
        let bytes = &self.blob[start..self.strings.end];
        let len = bytes.iter().position(|&b| b == 0)?;
        Some(&bytes[..len])
    }

    /// Walk the structure block, calling `visit(depth, node_name, prop_name,
    /// value)` for every property. Returns `None` on a malformed block.
    fn for_each_property(
        &self,
        mut visit: impl FnMut(usize, &'a [u8], &'a [u8], &'a [u8]),
    ) -> Option<()> {
        // Node names of the current path, by depth; only the leaf name is
        // needed, so remember the name at each depth.
        const MAX_DEPTH: usize = 16;
        let mut names: [&[u8]; MAX_DEPTH] = [&[]; MAX_DEPTH];
        let mut depth = 0usize;
        let mut off = self.structs.start;
        // Every iteration consumes at least one 4-byte token.
        let max_iters = self.structs.len() / 4 + 1;
        for _ in 0..max_iters {
            match self.token(off)? {
                FDT_BEGIN_NODE => {
                    let name_start = off + 4;
                    let rest = self.blob.get(name_start..self.structs.end)?;
                    let name_len = rest.iter().position(|&b| b == 0)?;
                    let name = &rest[..name_len];
                    if depth >= MAX_DEPTH {
                        return None;
                    }
                    names[depth] = name;
                    depth += 1;
                    // Name plus NUL, padded to 4 bytes.
                    off = name_start.checked_add((name_len + 1 + 3) & !3)?;
                }
                FDT_END_NODE => {
                    depth = depth.checked_sub(1)?;
                    off += 4;
                }
                FDT_PROP => {
                    let len = self.token(off + 4)? as usize;
                    let nameoff = self.token(off + 8)? as usize;
                    let value_start = off + 12;
                    let value_end = value_start.checked_add(len)?;
                    if value_end > self.structs.end {
                        return None;
                    }
                    let value = &self.blob[value_start..value_end];
                    let pname = self.string(nameoff)?;
                    if depth == 0 {
                        return None;
                    }
                    visit(depth, names[depth - 1], pname, value);
                    off = value_start.checked_add((len + 3) & !3)?;
                }
                FDT_NOP => off += 4,
                FDT_END => return if depth == 0 { Some(()) } else { None },
                _ => return None,
            }
        }
        None
    }

    /// The `reg` entries of the `/memory` node(s), written to `out` in
    /// order. Returns how many were found (which may exceed `out.len()`;
    /// only the first `out.len()` are stored). Zero if there is no memory
    /// node or the blob is malformed.
    pub fn memory_regions(&self, out: &mut [MemoryRegion]) -> usize {
        let mut address_cells: u32 = 2;
        let mut size_cells: u32 = 1;
        let mut found = 0usize;

        // One pass: the root's cell counts come before any child in a valid
        // blob (properties precede subnodes), so they are known by the time
        // the memory node's `reg` shows up.
        let walked = self.for_each_property(|depth, node, pname, value| {
            if depth == 1 {
                match pname {
                    b"#address-cells" => address_cells = be32(value, 0).unwrap_or(2),
                    b"#size-cells" => size_cells = be32(value, 0).unwrap_or(1),
                    _ => {}
                }
            } else if depth == 2 && is_memory_node(node) && pname == b"reg" {
                // Decode now; a later memory node appends after this one.
                let n = decode_reg(value, address_cells, size_cells, out, found);
                found = found.saturating_add(n);
            }
        });
        if walked.is_none() {
            return 0;
        }
        found
    }

    /// The memory region that contains `addr`, if any.
    pub fn memory_containing(&self, addr: u64) -> Option<MemoryRegion> {
        let mut regions: [MemoryRegion; 8] = Default::default();
        let n = self.memory_regions(&mut regions).min(regions.len());
        regions[..n].iter().find(|r| r.contains(&addr)).cloned()
    }
}

fn is_memory_node(name: &[u8]) -> bool {
    name == b"memory" || name.starts_with(b"memory@")
}

fn read_cells(value: &[u8], off: usize, cells: u32) -> Option<u64> {
    // More than two cells cannot be represented; refuse them.
    if cells == 0 || cells > 2 {
        return None;
    }
    let mut v: u64 = 0;
    for i in 0..cells as usize {
        v = (v << 32) | u64::from(be32(value, off + 4 * i)?);
    }
    Some(v)
}

/// Decode `reg` into `(base, size)` pairs, storing from `out[start..]`.
/// Returns the number of entries decoded (stored or not).
fn decode_reg(
    value: &[u8],
    address_cells: u32,
    size_cells: u32,
    out: &mut [MemoryRegion],
    start: usize,
) -> usize {
    let (Some(ac), Some(sc)) = (
        usize::try_from(address_cells).ok(),
        usize::try_from(size_cells).ok(),
    ) else {
        return 0;
    };
    let Some(entry_len) = ac.checked_add(sc).and_then(|c| c.checked_mul(4)) else {
        return 0;
    };
    if entry_len == 0 {
        return 0;
    }
    let mut n = 0;
    let mut off = 0;
    while off + entry_len <= value.len() {
        let (Some(base), Some(size)) = (
            read_cells(value, off, address_cells),
            read_cells(value, off + ac * 4, size_cells),
        ) else {
            return n;
        };
        if size != 0 {
            if let Some(slot) = out.get_mut(start + n) {
                *slot = base..base.saturating_add(size);
            }
            n += 1;
        }
        off += entry_len;
    }
    n
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    /// Build a blob by hand: header, structure block, strings block.
    struct Builder {
        structs: Vec<u8>,
        strings: Vec<u8>,
    }

    impl Builder {
        fn new() -> Self {
            Builder {
                structs: Vec::new(),
                strings: Vec::new(),
            }
        }
        fn tok(&mut self, t: u32) {
            self.structs.extend_from_slice(&t.to_be_bytes());
        }
        fn begin(&mut self, name: &str) {
            self.tok(FDT_BEGIN_NODE);
            self.structs.extend_from_slice(name.as_bytes());
            self.structs.push(0);
            while !self.structs.len().is_multiple_of(4) {
                self.structs.push(0);
            }
        }
        fn end(&mut self) {
            self.tok(FDT_END_NODE);
        }
        fn prop(&mut self, name: &str, value: &[u8]) {
            let nameoff = self.strings.len() as u32;
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
            self.tok(FDT_PROP);
            self.tok(value.len() as u32);
            self.tok(nameoff);
            self.structs.extend_from_slice(value);
            while !self.structs.len().is_multiple_of(4) {
                self.structs.push(0);
            }
        }
        fn prop_u32(&mut self, name: &str, v: u32) {
            self.prop(name, &v.to_be_bytes());
        }
        fn finish(mut self) -> Vec<u8> {
            self.tok(FDT_END);
            let off_struct = HEADER_LEN as u32;
            let off_strings = off_struct + self.structs.len() as u32;
            let total = off_strings + self.strings.len() as u32;
            let mut blob = Vec::new();
            for word in [
                FDT_MAGIC,
                total,
                off_struct,
                off_strings,
                0,  // off_mem_rsvmap
                17, // version
                16, // last_comp_version
                0,  // boot_cpuid_phys
                self.strings.len() as u32,
                self.structs.len() as u32,
            ] {
                blob.extend_from_slice(&word.to_be_bytes());
            }
            blob.extend_from_slice(&self.structs);
            blob.extend_from_slice(&self.strings);
            blob
        }
    }

    fn reg64(entries: &[(u64, u64)]) -> Vec<u8> {
        let mut v = Vec::new();
        for (b, s) in entries {
            v.extend_from_slice(&b.to_be_bytes());
            v.extend_from_slice(&s.to_be_bytes());
        }
        v
    }

    fn qemu_like(ram: u64) -> Vec<u8> {
        let mut b = Builder::new();
        b.begin("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.prop("compatible", b"linux,dummy-virt\0");
        b.begin("chosen");
        b.prop("stdout-path", b"/pl011@9000000\0");
        b.end();
        b.begin("memory@40000000");
        b.prop("device_type", b"memory\0");
        b.prop("reg", &reg64(&[(0x4000_0000, ram)]));
        b.end();
        b.begin("cpus");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 0);
        b.begin("cpu@0");
        b.prop("compatible", b"arm,cortex-a72\0");
        b.end();
        b.end();
        b.end();
        b.finish()
    }

    #[test]
    fn finds_the_memory_node() {
        let blob = qemu_like(256 << 20);
        let fdt = Fdt::from_bytes(&blob).expect("valid blob");
        assert_eq!(fdt.total_size(), blob.len());
        let mut regions: [MemoryRegion; 2] = Default::default();
        assert_eq!(fdt.memory_regions(&mut regions), 1);
        assert_eq!(regions[0], 0x4000_0000..0x5000_0000);
        assert_eq!(
            fdt.memory_containing(0x4020_0000),
            Some(0x4000_0000..0x5000_0000)
        );
        assert_eq!(fdt.memory_containing(0x5000_0000), None);
        // The cpus node's own cell counts must not leak into the decode.
        let blob = qemu_like(512 << 20);
        let fdt = Fdt::from_bytes(&blob).unwrap();
        assert_eq!(
            fdt.memory_containing(0x4000_0000),
            Some(0x4000_0000..0x6000_0000)
        );
    }

    #[test]
    fn default_cell_sizes_and_multiple_entries() {
        let mut b = Builder::new();
        b.begin("");
        b.begin("memory");
        // 2 address cells, 1 size cell (the defaults).
        let mut reg = Vec::new();
        for (base, size) in [(0x4000_0000u64, 0x1000_0000u32), (0x8000_0000, 0x2000_0000)] {
            reg.extend_from_slice(&base.to_be_bytes());
            reg.extend_from_slice(&size.to_be_bytes());
        }
        b.prop("reg", &reg);
        b.end();
        b.end();
        let blob = b.finish();
        let fdt = Fdt::from_bytes(&blob).unwrap();
        let mut regions: [MemoryRegion; 1] = Default::default();
        // Two found, one stored.
        assert_eq!(fdt.memory_regions(&mut regions), 2);
        assert_eq!(regions[0], 0x4000_0000..0x5000_0000);
        assert_eq!(
            fdt.memory_containing(0x9000_0000),
            Some(0x8000_0000..0xa000_0000)
        );
    }

    #[test]
    fn rejects_bad_headers() {
        let blob = qemu_like(1 << 20);
        assert!(Fdt::from_bytes(&blob[..HEADER_LEN - 1]).is_none());
        let mut bad_magic = blob.clone();
        bad_magic[0] ^= 1;
        assert!(Fdt::from_bytes(&bad_magic).is_none());
        // totalsize larger than the slice.
        let mut big = blob.clone();
        big[4..8].copy_from_slice(&((blob.len() as u32) + 1).to_be_bytes());
        assert!(Fdt::from_bytes(&big).is_none());
        // Structure block pointing past the end.
        let mut off = blob.clone();
        off[8..12].copy_from_slice(&0xffff_fff0u32.to_be_bytes());
        assert!(Fdt::from_bytes(&off).is_none());
        // Unaligned structure block.
        let mut unaligned = blob.clone();
        unaligned[8..12].copy_from_slice(&(HEADER_LEN as u32 + 2).to_be_bytes());
        assert!(Fdt::from_bytes(&unaligned).is_none());
        // Version too old.
        let mut old = blob.clone();
        old[20..24].copy_from_slice(&16u32.to_be_bytes());
        assert!(Fdt::from_bytes(&old).is_none());
        // Oversized totalsize.
        let mut huge = blob;
        huge[4..8].copy_from_slice(&((MAX_SIZE as u32) + 4).to_be_bytes());
        assert!(Fdt::from_bytes(&huge).is_none());
    }

    #[test]
    fn malformed_structure_never_panics() {
        let good = qemu_like(64 << 20);
        let fdt = Fdt::from_bytes(&good).unwrap();
        let mut regions: [MemoryRegion; 4] = Default::default();
        assert_eq!(fdt.memory_regions(&mut regions), 1);

        // Flip every byte of the structure and strings blocks in turn, and
        // also truncate at every length: the result must be None or a
        // (possibly empty) answer, never a panic.
        for i in HEADER_LEN..good.len() {
            let mut blob = good.clone();
            blob[i] ^= 0xff;
            if let Some(f) = Fdt::from_bytes(&blob) {
                let _ = f.memory_regions(&mut regions);
                let _ = f.memory_containing(0x4000_0000);
            }
            let mut blob = good.clone();
            blob[i] = 0;
            if let Some(f) = Fdt::from_bytes(&blob) {
                let _ = f.memory_regions(&mut regions);
            }
            let mut blob = good.clone();
            blob[i] = 0xff;
            if let Some(f) = Fdt::from_bytes(&blob) {
                let _ = f.memory_regions(&mut regions);
            }
        }
        // Word-level corruption of every token.
        for i in (HEADER_LEN..good.len() - 4).step_by(4) {
            for word in [0u32, 1, 2, 3, 4, 9, 0xffff_ffff, 0x7fff_ffff] {
                let mut blob = good.clone();
                blob[i..i + 4].copy_from_slice(&word.to_be_bytes());
                if let Some(f) = Fdt::from_bytes(&blob) {
                    let _ = f.memory_regions(&mut regions);
                }
            }
        }
        // Lying size fields in the header.
        for i in (8..HEADER_LEN).step_by(4) {
            for word in [0u32, 1, 3, 0x28, 0x100, 0xffff_ffff] {
                let mut blob = good.clone();
                blob[i..i + 4].copy_from_slice(&word.to_be_bytes());
                if let Some(f) = Fdt::from_bytes(&blob) {
                    let _ = f.memory_regions(&mut regions);
                }
            }
        }
    }

    #[test]
    fn unterminated_and_deep_blobs_fail_cleanly() {
        let mut regions: [MemoryRegion; 1] = Default::default();

        // FDT_END replaced by a NOP: the walker runs off the structure block
        // and reports nothing instead of reading on.
        let mut b = Builder::new();
        b.begin("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin("memory");
        b.prop("reg", &reg64(&[(0x4000_0000, 1 << 20)]));
        b.end();
        b.end();
        let mut blob = b.finish();
        assert_eq!(
            Fdt::from_bytes(&blob).unwrap().memory_regions(&mut regions),
            1
        );
        let size_struct = u32::from_be_bytes(blob[36..40].try_into().unwrap()) as usize;
        let end_tok = HEADER_LEN + size_struct - 4;
        blob[end_tok..end_tok + 4].copy_from_slice(&FDT_NOP.to_be_bytes());
        assert_eq!(
            Fdt::from_bytes(&blob).unwrap().memory_regions(&mut regions),
            0
        );

        // FDT_END while nodes are still open.
        let mut b = Builder::new();
        b.begin("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin("memory");
        b.prop("reg", &reg64(&[(0x4000_0000, 1 << 20)]));
        let blob = b.finish();
        assert_eq!(
            Fdt::from_bytes(&blob).unwrap().memory_regions(&mut regions),
            0
        );

        // Nesting deeper than the walker tracks.
        let mut b = Builder::new();
        for _ in 0..40 {
            b.begin("n");
        }
        for _ in 0..40 {
            b.end();
        }
        let blob = b.finish();
        assert_eq!(
            Fdt::from_bytes(&blob).unwrap().memory_regions(&mut regions),
            0
        );

        // Unsupported cell widths and zero-sized entries are refused or
        // skipped.
        let mut b = Builder::new();
        b.begin("");
        b.prop_u32("#address-cells", 3);
        b.prop_u32("#size-cells", 2);
        b.begin("memory@0");
        b.prop("reg", &[0u8; 20]);
        b.end();
        b.end();
        let blob = b.finish();
        assert_eq!(
            Fdt::from_bytes(&blob).unwrap().memory_regions(&mut regions),
            0
        );
        let mut b = Builder::new();
        b.begin("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin("memory@0");
        b.prop("reg", &reg64(&[(0x4000_0000, 0), (0x8000_0000, 4096)]));
        b.end();
        b.end();
        let blob = b.finish();
        assert_eq!(
            Fdt::from_bytes(&blob).unwrap().memory_regions(&mut regions),
            1
        );
        assert_eq!(regions[0], 0x8000_0000..0x8000_1000);
    }

    #[test]
    fn from_raw_respects_the_limit() {
        let blob = qemu_like(128 << 20);
        // A limit shorter than totalsize truncates and fails validation
        // rather than reading past it.
        let short = unsafe { Fdt::from_raw(blob.as_ptr(), blob.len() - 8) };
        assert!(short.is_none());
        let ok = unsafe { Fdt::from_raw(blob.as_ptr(), blob.len() + 1000) }.unwrap();
        assert_eq!(
            ok.memory_containing(0x4000_0000),
            Some(0x4000_0000..0x4800_0000)
        );
        assert!(unsafe { Fdt::from_raw(core::ptr::null(), 4096) }.is_none());
        assert!(unsafe { Fdt::from_raw(blob.as_ptr(), 8) }.is_none());
    }
}
