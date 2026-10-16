use super::*;

#[test]
fn disk_capacity_must_be_nonzero_whole_sectors() {
    for capacity in [0, 1, 511, 513, u64::MAX] {
        assert!(DiskImage::zeroed(capacity).is_err(), "{capacity}");
    }
    for length in [0, 1, 511, 1025] {
        assert!(DiskImage::from_bytes(vec![0; length]).is_err(), "{length}");
    }
    let error = DiskImage::zeroed(1).unwrap_err().to_string();
    assert!(error.contains("disk capacity 1 bytes"), "{error}");
}

#[test]
fn zeroed_disk_reports_capacity_without_materializing_it() {
    // An eagerly filled exabyte would abort this test.
    let capacity = u64::MAX - 511;
    let disk = DiskImage::zeroed(capacity).unwrap();
    assert_eq!(disk.capacity_bytes(), capacity);
    assert_eq!(disk.sectors(), capacity / 512);
    assert!(disk.image().is_none());
    assert_eq!(
        format!("{disk:?}"),
        format!("DiskImage {{ contents: \"zeroed\", capacity_bytes: {capacity} }}")
    );
}

#[test]
fn image_is_owned_shared_by_clones_and_compared_by_contents() {
    let mut bytes = vec![0; 1024];
    bytes[700] = 7;
    let disk = DiskImage::from_bytes(bytes.clone()).unwrap();
    assert_eq!(disk.capacity_bytes(), 1024);
    assert_eq!(disk.sectors(), 2);
    assert_eq!(&disk.image().unwrap()[..], &bytes[..]);
    let clone = disk.clone();
    assert!(Arc::ptr_eq(disk.image().unwrap(), clone.image().unwrap()));
    assert_eq!(disk, DiskImage::from_bytes(bytes).unwrap());
    assert_ne!(disk, DiskImage::zeroed(1024).unwrap());
    assert_eq!(
        DiskImage::zeroed(1024).unwrap(),
        DiskImage::from_bytes(vec![0; 1024]).unwrap()
    );
    assert_ne!(
        DiskImage::zeroed(512).unwrap(),
        DiskImage::from_bytes(vec![0; 1024]).unwrap()
    );
    assert_ne!(
        DiskImage::zeroed(512).unwrap(),
        DiskImage::zeroed(1024).unwrap()
    );
    assert_eq!(
        format!("{disk:?}"),
        "DiskImage { contents: \"image\", capacity_bytes: 1024 }"
    );
}
