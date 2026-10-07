fn chunk_index(records: &[[u64; 5]]) -> Vec<u8> {
    let mut bytes = b"SBLOG001".to_vec();
    for record in records {
        for value in record { bytes.extend_from_slice(&value.to_le_bytes()); }
    }
    bytes
}
fn add_log(path: &std::path::Path, record: &mut sb::orchestration::RunRecord, name: &str, bytes: &[u8]) {
    let file=path.join("output").join(name);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file,bytes).unwrap();
    record.logs.insert(name.into(),sb::store::ContentDigest::of(bytes));
}
fn timed_fixture(path: &std::path::Path) -> sb::orchestration::RunRecord {
    let mut record=fixture();
    add_log(path,&mut record,"machines/workload.console",b"\0\xffA\nB");
    add_log(path,&mut record,"machines/workload.console.chunks",&chunk_index(&[[10,0,0,0,2],[10,0,2,2,2],[20,1,3,4,1]]));
    add_log(path,&mut record,"machines/workload.mgmt",b"\xfe");
    add_log(path,&mut record,"machines/workload.mgmt.chunks",&chunk_index(&[[10,0,1,0,1]]));
    write_fixture(path,&mut record,None,None);
    record
}
