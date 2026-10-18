//! Version 1 field/tag order. Do not renumber tags or reorder fields.
use super::*;
wire_struct!(Event {
    sequence,
    position,
    at,
    kind
});
wire_enum!(EventKind; units {} tuples {0=>Output(v0),3=>FaultAdmitted(v0)} records {1=>MachineAdded{machine,cause},2=>Transition{machine,boot,change,cause},4=>InputRejected{key,rejection},5=>FaultSettled{record,status,outcome_at,detail,network},6=>TriggerArmed{trigger,source,target,deadline},7=>TriggerResolved{trigger,result,fault},8=>ReplyAdmitted{machine,boot,id}});
wire_enum!(Transition; units {0=>Paused,1=>Resumed,2=>Crashed,4=>ShutdownRequested} tuples {5=>ExecutionSet(v0),6=>ClockRate(v0)} records {3=>Rebooted{new_boot}});
wire_enum!(Cause; units {0=>Initial,1=>Host} tuples {2=>Fault(v0)} records {});
wire_tuple_struct!(FaultId);
wire_struct!(FaultRecord {
    fault,
    origin,
    target,
    action,
    handle,
    requested_at
});
wire_enum!(FaultTarget; units {} tuples {} records {0=>Machine{machine,boot},1=>Links{left,right},2=>DirectedLinks{from,to},3=>Connection{connection}});
wire_enum!(FaultOrigin; units {} tuples {} records {0=>Control{machine,call},1=>Trigger{trigger},2=>Input{key}});
wire_enum!(FaultAction; units {12=>DropConnection,0=>Spawn,1=>Pause,2=>Resume,3=>Crash,4=>Reboot,5=>Shutdown,10=>Partition,11=>Heal} tuples {6=>SetExecution(v0),7=>Application(v0),8=>Drift(v0),9=>StepRealtime(v0)} records {13=>Shape{policy,network_faults}});
wire_enum!(SettlementStatus; units {0=>Applied,1=>Unavailable,2=>Rejected,3=>Failed} tuples {} records {});
wire_enum!(TriggerResult; units {0=>Fired,1=>Rejected,2=>Cancelled,3=>Expired,4=>CutOff} tuples {} records {});
