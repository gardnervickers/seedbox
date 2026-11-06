//! The static metadata of an ext4 filesystem on a store, as `disk_error`'s
//! `data` scope spares it: what `mke2fs` lays out at format time and never
//! moves. That is the boot block and superblocks, group descriptor tables
//! and their reserved growth, block and inode bitmaps, inode tables, and the
//! blocks of the reserved inodes (the journal among them) and the orphan
//! file. Metadata ext4 allocates later (directories, extent-index and xattr
//! blocks) lies among data blocks; the host keeps no live filesystem model,
//! so it is data here.
//!
//! The layout is a pure function of the store's current bytes, read when a
//! request needs it, so restores and replays see the same answer.
use super::{Store, SECTOR};

/// Static metadata as merged, sorted sector ranges `[start, end)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub ranges: Vec<(u64, u64)>,
}

impl Layout {
    /// Whether any of `sectors` sectors from `sector` is static metadata.
    pub fn overlaps(&self, sector: u64, sectors: u64) -> bool {
        let end = sector.saturating_add(sectors.max(1));
        self.ranges.iter().any(|&(s, e)| sector < e && s < end)
    }
}

/// Read `buffer.len()` bytes at byte `offset`; false past the end.
fn read_at(store: &Store, offset: u64, buffer: &mut [u8]) -> bool {
    let first = offset / SECTOR as u64;
    let skip = (offset % SECTOR as u64) as usize;
    let sectors = (skip + buffer.len()).div_ceil(SECTOR);
    let mut bytes = vec![0; sectors * SECTOR];
    if store.read(first, &mut bytes).is_err() {
        return false;
    }
    buffer.copy_from_slice(&bytes[skip..skip + buffer.len()]);
    true
}

/// The static metadata of the store's ext4 filesystem, or `None` when it
/// holds none this reader understands (including before it is formatted).
pub fn layout(store: &Store) -> Option<Layout> {
    let mut read = |offset: u64, buffer: &mut [u8]| read_at(store, offset, buffer);
    let mut sb = [0u8; 1024];
    if !read(1024, &mut sb) || le16(&sb, 56) != 0xef53 {
        return None;
    }
    let log = le32(&sb, 24);
    if log > 6 {
        return None;
    }
    let block = 1024u64 << log;
    let incompat = le32(&sb, 96);
    let ro_compat = le32(&sb, 100);
    let wide = incompat & 0x80 != 0;
    // META_BG lays descriptors out per metagroup; the guest formatter does
    // not use it, so such a filesystem is not understood.
    if incompat & 0x10 != 0 {
        return None;
    }
    let hi = |lo: u64, at: usize| lo | if wide { le32(&sb, at) << 32 } else { 0 };
    let blocks = hi(le32(&sb, 4), 0x150);
    let first = le32(&sb, 20);
    let per_group = le32(&sb, 32);
    let inodes_per_group = le32(&sb, 40);
    let dynamic = le32(&sb, 76) >= 1;
    let inode_size = if dynamic { le16(&sb, 88) } else { 128 };
    let desc = if wide { le16(&sb, 254).max(32) } else { 32 };
    if per_group == 0 || inodes_per_group == 0 || inode_size < 128 || blocks <= first {
        return None;
    }
    let groups = (blocks - first).div_ceil(per_group);
    if groups > 1 << 24 {
        return None;
    }
    let table_blocks = (inodes_per_group * inode_size).div_ceil(block);
    let gdt_blocks = (groups * desc).div_ceil(block);
    let reserved = le16(&sb, 0xce);
    let sparse = ro_compat & 0x1 != 0;
    let mut ranges: Vec<(u64, u64)> = vec![(0, first + 1)];
    let mut tables = Vec::with_capacity(groups as usize);
    let mut descriptor = vec![0u8; desc as usize];
    for group in 0..groups {
        let start = first + group * per_group;
        if !sparse || has_backup(group) {
            ranges.push((start, start + 1 + gdt_blocks + reserved));
        }
        if !read((first + 1) * block + group * desc, &mut descriptor) {
            return None;
        }
        let field = |lo: usize, hi: usize| {
            le32(&descriptor, lo) | if wide { le32(&descriptor, hi) << 32 } else { 0 }
        };
        let (block_bitmap, inode_bitmap, table) = (field(0, 0x20), field(4, 0x24), field(8, 0x28));
        ranges.push((block_bitmap, block_bitmap + 1));
        ranges.push((inode_bitmap, inode_bitmap + 1));
        ranges.push((table, table + table_blocks));
        tables.push(table);
    }
    // Reserved inodes below the first ordinary one (the journal, root and
    // lost+found among them) and the orphan file. Those mapped without
    // extents (the resize inode) point into the reserved GDT.
    let first_ino = if dynamic { le32(&sb, 0x54) } else { 11 };
    let journal = le32(&sb, 0xe0);
    if le32(&sb, 92) & 0x4 != 0 && journal == 0 {
        return None;
    }
    let mut inodes: Vec<u64> = (1..=first_ino.clamp(11, 64)).collect();
    if le32(&sb, 92) & 0x1000 != 0 {
        inodes.push(le32(&sb, 0x280));
    }
    for number in inodes.into_iter().filter(|n| *n != 0) {
        let index = number - 1;
        let Some(table) = tables.get((index / inodes_per_group) as usize) else {
            continue;
        };
        let mut inode = vec![0u8; inode_size as usize];
        if !read(
            table * block + (index % inodes_per_group) * inode_size,
            &mut inode,
        ) {
            return None;
        }
        if le32(&inode, 0x20) & 0x80000 != 0 && le16(&inode, 0x28) == 0xf30a {
            extents(&mut read, block, &inode[0x28..0x28 + 60], 0, &mut ranges)?;
        } else if number == journal {
            return None;
        } else {
            for i in 0..15 {
                let pointer = le32(&inode, 0x28 + i * 4);
                if pointer != 0 && pointer < blocks {
                    ranges.push((pointer, pointer + 1));
                }
            }
        }
    }
    let sectors = block / SECTOR as u64;
    ranges.retain(|(start, end)| end > start);
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in ranges {
        let (start, end) = (start.checked_mul(sectors)?, end.checked_mul(sectors)?);
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    Some(Layout { ranges: merged })
}

/// Whether a group holds a superblock backup under sparse_super: groups 0
/// and 1 and powers of 3, 5 and 7.
fn has_backup(group: u64) -> bool {
    if group <= 1 {
        return true;
    }
    [3u64, 5, 7].iter().any(|base| {
        let mut n = *base;
        while n < group {
            n *= base;
        }
        n == group
    })
}

/// Add an extent node's blocks and, below the inode, the index nodes.
fn extents(
    read: &mut impl FnMut(u64, &mut [u8]) -> bool,
    block: u64,
    node: &[u8],
    level: u32,
    ranges: &mut Vec<(u64, u64)>,
) -> Option<()> {
    if level > 5 || node.len() < 12 || le16(node, 0) != 0xf30a {
        return None;
    }
    let entries = le16(node, 2) as usize;
    let depth = le16(node, 6);
    if 12 + entries * 12 > node.len() {
        return None;
    }
    for i in 0..entries {
        let entry = &node[12 + i * 12..24 + i * 12];
        if depth == 0 {
            let len = le16(entry, 4);
            let len = if len > 32768 { len - 32768 } else { len };
            let start = le32(entry, 8) | le16(entry, 6) << 32;
            ranges.push((start, start + len));
        } else {
            let child = le32(entry, 4) | le16(entry, 8) << 32;
            ranges.push((child, child + 1));
            let mut buf = vec![0u8; block as usize];
            if !read(child * block, &mut buf) {
                return None;
            }
            extents(read, block, &buf, level + 1, ranges)?;
        }
    }
    Some(())
}

fn le16(b: &[u8], at: usize) -> u64 {
    u64::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

fn le32(b: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]))
}

#[cfg(test)]
pub(crate) mod fixture {
    pub(crate) fn put16(b: &mut [u8], at: usize, v: u16) {
        b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn put32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// Two 4 KiB-block groups of 32768 blocks with flex_bg-style tables
    /// packed in group 0, and a journal inode with one extent, in a 41 KiB
    /// prefix; the rest of the 256 MiB filesystem reads as zeros.
    pub(crate) fn image() -> Vec<u8> {
        let block = 4096usize;
        let mut disk = vec![0u8; 41 * block];
        let sb = &mut disk[1024..2048];
        put32(sb, 4, 65536); // blocks
        put32(sb, 20, 0); // first data block
        put32(sb, 24, 2); // 4 KiB blocks
        put32(sb, 32, 32768); // blocks per group
        put32(sb, 40, 256); // inodes per group
        put16(sb, 56, 0xef53);
        put32(sb, 76, 1); // dynamic revision
        put16(sb, 88, 256); // inode size
        put32(sb, 92, 0x4); // has_journal
        put32(sb, 100, 0x1); // sparse_super
        put16(sb, 0xce, 3); // reserved GDT blocks
        put32(sb, 0xe0, 8); // journal inode
                            // Group descriptors at block 1: bitmaps at 5, 6 and 7, 8; inode
                            // tables at 9 and 25 (16 blocks each: 256 inodes of 256 bytes).
        for (g, (bb, ib, it)) in [(5u32, 7u32, 9u32), (6, 8, 25)].iter().enumerate() {
            let d = &mut disk[block + g * 32..block + g * 32 + 32];
            put32(d, 0, *bb);
            put32(d, 4, *ib);
            put32(d, 8, *it);
        }
        // Journal inode 8: index 7 in group 0's table at block 9.
        let inode = &mut disk[9 * block + 7 * 256..9 * block + 8 * 256];
        put32(inode, 0x20, 0x80000);
        let node = &mut inode[0x28..0x28 + 60];
        put16(node, 0, 0xf30a);
        put16(node, 2, 1);
        put16(node, 6, 0);
        put32(&mut node[12..24], 0, 0);
        put16(&mut node[12..24], 4, 1024);
        put32(&mut node[12..24], 8, 40_000);
        disk
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;

    fn store(prefix: &[u8], sectors: u64) -> Store {
        let mut store = Store::zeroed(sectors).unwrap();
        store.write(0, prefix).unwrap();
        store
    }

    #[test]
    fn static_metadata_covers_superblocks_tables_and_the_journal() {
        let store = store(&image(), 65536 * 8);
        let ranges = layout(&store).unwrap().ranges;
        let s = |block: u64| block * 8;
        assert_eq!(
            ranges,
            vec![
                // Superblock, descriptors, reserved GDT, then the packed
                // bitmaps and both inode tables: blocks 0..41.
                (0, s(41)),
                // Group 1's backup superblock, descriptor and reserved GDT.
                (s(32768), s(32768 + 5)),
                // The journal.
                (s(40_000), s(41_024)),
            ]
        );
        let layout = layout(&store).unwrap();
        assert!(layout.overlaps(s(40), 8));
        assert!(!layout.overlaps(s(41), 8));
        assert!(layout.overlaps(s(41) - 1, 2));
        assert!(!layout.overlaps(s(50_000), 8));
    }

    #[test]
    fn a_store_without_ext4_has_no_layout() {
        assert_eq!(layout(&Store::zeroed(64).unwrap()), None);
        let mut disk = image();
        put32(&mut disk[1024..2048], 96, 0x10); // meta_bg
        assert_eq!(layout(&store(&disk, 65536 * 8)), None);
        // A superblock past the end of a tiny store is no filesystem.
        assert_eq!(layout(&Store::zeroed(2).unwrap()), None);
    }

    #[test]
    fn sparse_superblock_backups() {
        let groups: Vec<u64> = (0..60).filter(|g| has_backup(*g)).collect();
        assert_eq!(groups, [0, 1, 3, 5, 7, 9, 25, 27, 49]);
    }
}
