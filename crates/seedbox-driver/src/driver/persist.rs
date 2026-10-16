//! Version 1 field/tag order. Do not renumber tags or reorder fields.
use super::*;
wire_tuple_struct!(MachineId);
wire_struct!(MachineResponse {
    machine,
    boot,
    at,
    payload
});
wire_struct!(Position {
    boundary_index,
    completed_quanta,
    at
});
wire_enum!(DriverEvent; units {} tuples {0=>Pause(v0),1=>Resume(v0)} records {2=>SetExecution{machine,config},3=>SetClockRate{machine,rate}});
wire_enum!(Staged; units {} tuples {0=>Driver(v0,v1),1=>Notice(v0)} records {});
wire_enum!(Scheduled; units {} tuples {0=>Driver(v0),1=>Control(v0)} records {2=>Input{id,input}});
wire_struct!(OwnedHandle { record_id, text });
wire_enum!(LiveInput; units {0=>Pending} tuples {2=>Handle(v0)} records {1=>Admitted{handle,record_id}});
