//! Field and tag order is the checkpoint encoding; changing it needs a
//! checkpoint `VERSION` bump.
use super::*;
impl crate::persistence::Wire for InputKey {
    fn put<W: std::io::Write>(
        &self,
        e: &mut crate::persistence::Encoder<W>,
    ) -> crate::persistence::Result<()> {
        crate::persistence::Wire::put(&self.0, e)
    }
    fn get<R: std::io::Read>(
        d: &mut crate::persistence::Decoder<R>,
    ) -> crate::persistence::Result<Self> {
        Self::new(<String as crate::persistence::Wire>::get(d)?)
            .map_err(|_| crate::persistence::CodecError::Invalid("input key"))
    }
}
impl crate::persistence::Wire for FaultHandle {
    fn put<W: std::io::Write>(
        &self,
        e: &mut crate::persistence::Encoder<W>,
    ) -> crate::persistence::Result<()> {
        crate::persistence::Wire::put(&self.0, e)
    }
    fn get<R: std::io::Read>(
        d: &mut crate::persistence::Decoder<R>,
    ) -> crate::persistence::Result<Self> {
        let text = <String as crate::persistence::Wire>::get(d)?;
        Self::named(text).map_err(|_| crate::persistence::CodecError::Invalid("fault handle"))
    }
}
wire_enum!(Target; units {} tuples {0=>Machine(v0),1=>Name(v0)} records {});
wire_enum!(Action; units {} tuples {0=>Pause(v0),1=>Resume(v0),2=>Crash(v0),3=>Reboot(v0),4=>Shutdown(v0),10=>Heal(v0)} records {5=>Stop{target,deadline},6=>Restart{target,deadline},7=>PauseProcess{target,deadline},8=>ResumeProcess{target,deadline},9=>Throttle{target,config},11=>Spawn{name},12=>Drift{target,rate},13=>StepRealtime{target,nanoseconds},14=>Partition{left,right},15=>Shape{from,to,policy,network_faults},16=>DropConnection{connection},17=>DriftBounded{target,max_rate_ppb},18=>StepRealtimeBounded{target,max_offset_ns},19=>DeviceFault{target,fault}});
wire_enum!(HealTarget; units {} tuples {0=>Handle(v0),1=>Input(v0)} records {});
wire_struct!(InputRejection { reason, detail });
wire_enum!(RejectionReason; units {0=>InvalidState,1=>TargetUnavailable,2=>InvalidRequest,3=>Unsupported} tuples {} records {});
