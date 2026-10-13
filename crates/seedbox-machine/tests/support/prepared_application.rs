//! Convert the retained static guest fixture archive into shared workload inputs.
//! This is test input adaptation, not production image preparation.
use super::machine_crate::definition::{
    FilesystemArchive, Launch, MachineDefinition, PreparedApplication, Resources,
};

pub fn definition(archive: Vec<u8>, memory_bytes: u64) -> MachineDefinition {
    // The optional base may be compressed. Locate and validate complete newc
    // records instead of treating bytes inside that base as a filesystem.
    let entry = |wanted: &str| {
        let mut found = None;
        let mut offset = 0;
        while offset + 110 <= archive.len() {
            if &archive[offset..offset + 6] != b"070701" {
                offset += 1;
                continue;
            }
            let field = |index: usize| {
                std::str::from_utf8(&archive[offset + 6 + index * 8..offset + 14 + index * 8])
                    .ok()
                    .and_then(|s| usize::from_str_radix(s, 16).ok())
            };
            let (Some(size), Some(namesize)) = (field(6), field(11)) else {
                offset += 1;
                continue;
            };
            let name_start = offset + 110;
            let Some(name_end) = name_start.checked_add(namesize) else {
                break;
            };
            let data_start = (name_end + 3) & !3;
            let Some(data_end) = data_start.checked_add(size) else {
                break;
            };
            if namesize == 0 || data_end > archive.len() {
                offset += 1;
                continue;
            }
            if archive[name_end - 1] == 0 && &archive[name_start..name_end - 1] == wanted.as_bytes()
            {
                found = Some(archive[data_start..data_end].to_vec());
            }
            offset = (data_end + 3) & !3;
        }
        found
    };
    let program = entry("rootfs/application")
        .or_else(|| entry("application"))
        .expect("fixture archive missing application");
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(program.len() as u64);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, "application", &program[..])
        .unwrap();
    let filesystem = FilesystemArchive::new(tar.into_inner().unwrap()).unwrap();
    MachineDefinition {
        name: "fixture".into(),
        resources: Resources { memory_bytes },
        application: PreparedApplication {
            filesystem,
            launch: Launch {
                argv: vec!["/application".into()],
                environment: Default::default(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
    }
}
