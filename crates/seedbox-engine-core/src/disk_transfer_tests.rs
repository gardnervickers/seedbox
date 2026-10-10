use crate::disk::{Disk, DiskFaultModel, SECTOR};
use std::io::{self, Cursor, Read, Write};

fn base() -> Disk {
    Disk::new(Some(vec![7; 8 * SECTOR]), None)
        .unwrap()
        .with_fault_model(DiskFaultModel {
            sector_bytes: SECTOR,
            survival_probability: 0.5,
        })
}
fn survivor() -> Disk {
    let mut disk = base();
    disk.write(0, &[1; SECTOR]).unwrap();
    disk.write(6, &[2; SECTOR]).unwrap();
    disk.flush();
    disk.write(0, &[3; SECTOR]).unwrap();
    disk.write(4, &[4; SECTOR]).unwrap();
    disk.crashed_seeded(1009, "server", 3).0
}
fn export(disk: &Disk) -> Vec<u8> {
    let mut bytes = vec![];
    disk.write_crash_transfer(&mut bytes).unwrap();
    bytes
}
#[test]
fn survivor_transfer_preserves_base_layers_digest_and_future_crashes() {
    let source = survivor();
    let restored = Disk::read_crash_transfer(base(), Cursor::new(export(&source))).unwrap();
    assert_eq!(restored.base_sha256(), source.base_sha256());
    assert_eq!(restored.digest(), source.digest());
    assert_eq!(restored.stable_sectors(), source.stable_sectors());
    assert_eq!(restored.volatile_sectors(), 0);
    let mut actual = vec![0; 8 * SECTOR];
    let mut expected = actual.clone();
    restored.read(0, &mut actual).unwrap();
    source.read(0, &mut expected).unwrap();
    assert_eq!(actual, expected);
    for seed in [0, 1009, u64::MAX] {
        let mut a = source.clone();
        let mut b = restored.clone();
        a.write(0, &[9; 2 * SECTOR]).unwrap();
        b.write(0, &[9; 2 * SECTOR]).unwrap();
        let (a, lost_a) = a.crashed_seeded(seed, "server", 4);
        let (b, lost_b) = b.crashed_seeded(seed, "server", 4);
        assert_eq!((a.digest(), lost_a), (b.digest(), lost_b));
    }
    let zeros = Disk::new(None, Some(1)).unwrap();
    assert_eq!(
        Disk::read_crash_transfer(zeros.clone(), Cursor::new(export(&zeros)))
            .unwrap()
            .digest(),
        zeros.digest()
    );
}
#[test]
fn mismatched_or_mutated_destinations_and_uncrashed_sources_are_rejected() {
    let bytes = export(&survivor());
    let wrong_image = Disk::new(Some(vec![8; 8 * SECTOR]), None)
        .unwrap()
        .with_fault_model(base().fault_model());
    let wrong_capacity = Disk::new(None, Some(9)).unwrap();
    let wrong_policy = base().with_fault_model(DiskFaultModel {
        sector_bytes: SECTOR,
        survival_probability: 1.0,
    });
    for disk in [wrong_image, wrong_capacity, wrong_policy] {
        assert!(Disk::read_crash_transfer(disk, Cursor::new(&bytes)).is_err());
    }
    let mut dirty = base();
    dirty.write(0, &[0; SECTOR]).unwrap();
    assert!(dirty.write_crash_transfer(Vec::new()).is_err());
    assert!(Disk::read_crash_transfer(dirty.clone(), Cursor::new(&bytes)).is_err());
    dirty.flush();
    assert!(Disk::read_crash_transfer(dirty, Cursor::new(&bytes)).is_err());
}
#[test]
fn corrupt_count_order_range_payload_digest_and_trailing_data_are_rejected() {
    let bytes = export(&survivor());
    let mut invalid = vec![];
    for index in [0, 8, 16, 24, 25, 97 + 8, bytes.len() - 1] {
        let mut changed = bytes.clone();
        changed[index] ^= 1;
        invalid.push(changed);
    }
    let mut count = bytes.clone();
    count[89..97].copy_from_slice(&u64::MAX.to_le_bytes());
    invalid.push(count);
    let mut range = bytes.clone();
    range[97..105].copy_from_slice(&8_u64.to_le_bytes());
    invalid.push(range);
    let mut duplicate = bytes.clone();
    duplicate[617..625].copy_from_slice(&0_u64.to_le_bytes());
    invalid.push(duplicate);
    let mut trailing = bytes.clone();
    trailing.push(0);
    invalid.push(trailing);
    for bytes in invalid {
        assert!(Disk::read_crash_transfer(base(), Cursor::new(bytes)).is_err());
    }
    for end in 0..bytes.len() {
        assert!(Disk::read_crash_transfer(base(), Cursor::new(&bytes[..end])).is_err());
    }
}
struct BrokenIo;
impl Read for BrokenIo {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("broken reader"))
    }
}
impl Write for BrokenIo {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("broken writer"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("broken flush"))
    }
}
#[test]
fn transport_errors_are_explicit_and_do_not_modify_source() {
    let disk = survivor();
    let before = disk.digest();
    assert!(disk
        .write_crash_transfer(BrokenIo)
        .unwrap_err()
        .contains("broken writer"));
    assert!(Disk::read_crash_transfer(base(), BrokenIo)
        .err()
        .unwrap()
        .contains("broken reader"));
    assert_eq!(disk.digest(), before);
}
