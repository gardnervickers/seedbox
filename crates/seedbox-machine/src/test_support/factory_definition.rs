use crate::definition::{
    FilesystemArchive, Launch, MachineDefinition, PreparedApplication, Resources,
};
pub fn definition(index: usize) -> MachineDefinition {
    MachineDefinition {
        name: format!("fixture-{index}"),
        resources: Resources {
            memory_bytes: 128 << 20,
        },
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(tar::Builder::new(Vec::new()).into_inner().unwrap())
                .unwrap(),
            launch: Launch {
                argv: vec!["/fixture".into()],
                environment: Default::default(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
        disk: None,
        network: None,
    }
}
