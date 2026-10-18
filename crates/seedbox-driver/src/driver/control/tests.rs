use super::*;
use crate::driver::fixture;
#[test]
fn duplicate_display_names_never_alias_sources_and_unnamed_additions_route() {
    let mut control = Control::new(&[fixture::definition(0), fixture::definition(0)], &[0, 1]);
    assert!(control.target("fixture-0").is_err());
    for index in 0..2 {
        assert_eq!(
            control
                .reply_target(&Control::source(MachineId(index)))
                .unwrap(),
            MachineId(index)
        );
    }
    control.add_unnamed(2);
    assert_eq!(
        control
            .reply_target(&Control::source(MachineId(2)))
            .unwrap(),
        MachineId(2)
    );
    assert!(control.target("machine:2").is_err());
}
#[test]
fn boot_allocator_restarts_per_boot_and_never_wraps() {
    let mut commands = Commands::default();
    assert_eq!(
        commands
            .reserve(
                7,
                CommandKind::Application(crate::pv::ApplicationCommand::Stop)
            )
            .unwrap()
            .get(),
        1
    );
    commands.reset();
    assert_eq!(
        commands
            .reserve(
                8,
                CommandKind::Application(crate::pv::ApplicationCommand::Start)
            )
            .unwrap()
            .get(),
        1
    );
    commands.next = u32::MAX;
    let before = commands.used.clone();
    assert!(commands
        .reserve(
            9,
            CommandKind::Application(crate::pv::ApplicationCommand::Pause)
        )
        .is_err());
    assert_eq!(commands.used, before);
}
