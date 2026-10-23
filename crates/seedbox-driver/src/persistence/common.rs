use super::*;
use seedbox_machine::{
    clock::ClockRate,
    definition::*,
    execution::ExecutionConfig,
    machine::{EvidenceDisposition, ExitReason, StopReason, Timestamp},
    pv::*,
};
wire_enum!(ApplicationCommand; units {0=>Start,1=>Stop,2=>Pause,3=>Resume} tuples {} records {});
wire_enum!(CommandResult; units {0=>Completed,1=>Rejected} tuples {} records {});
wire_enum!(Stream; units {0=>Stdout,1=>Stderr} tuples {} records {});
wire_struct!(SimulationControlId { session, request });
wire_enum!(foreign Request; units {0=>Crash,1=>Reboot,2=>Shutdown} tuples {} records {3=>SimulationReply{id,payload},4=>Application{id,command},5=>StepRealtime{id,nanoseconds},6=>ReceiveFrame{bytes}});
wire_enum!(EvidenceDisposition; units {0=>Live,1=>Crashed,2=>Shutdown} tuples {} records {3=>Exited{code}});
use seedbox_engine_protocol::out_of_memory::Kill;
wire_struct!(Kill {
    pid,
    command,
    anon_rss_kib,
    application
});
wire_enum!(foreign Response; units {1=>Ready} tuples {} records {0=>SimulationControl{id,payload},2=>BootConsole{bytes},3=>ApplicationOutput{stream,bytes},4=>ApplicationOutcome{code},5=>CommandAcknowledged{id,result},6=>TransmitFrame{bytes},8=>OutOfMemoryKill{kill},7=>BootEnded{ended_boot,disposition,native_booted,disk_lost_sectors,surviving_disk,retired_state_digest}});
wire_struct!(Input { at, payload });
wire_enum!(StopReason; units {0=>BoundaryReached} tuples {} records {1=>Blocked{reached,deadline},2=>Exited{reached,reason}});
wire_enum!(ExitReason; units {0=>Shutdown,2=>Crashed} tuples {} records {1=>ProgramExit{code}});
wire_struct!(MachineDefinition {
    name,
    resources,
    application,
    disk,
    network
});
wire_struct!(Resources { memory_bytes });
wire_struct!(PreparedApplication { filesystem, launch });
wire_struct!(Launch {
    argv,
    environment,
    working_directory,
    user
});

impl Wire for Timestamp {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        self.as_nanos().put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        Ok(Self::from_nanos(u64::get(d)?))
    }
}
impl Wire for ExecutionConfig {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        self.factor_permille().put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        Self::new(u32::get(d)?).map_err(|_| CodecError::Invalid("execution factor"))
    }
}
impl Wire for ClockRate {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        self.ppb().put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        Self::new(i64::from(i32::get(d)?)).map_err(|_| CodecError::Invalid("clock rate"))
    }
}
impl Wire for MacAddress {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        self.octets().put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        Self::new(<[u8; 6]>::get(d)?).map_err(|_| CodecError::Invalid("MAC address"))
    }
}
impl Wire for NetworkInterface {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        self.mac().put(e)?;
        self.ipv4().octets().put(e)?;
        self.prefix_len().put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        Self::new(
            MacAddress::get(d)?,
            std::net::Ipv4Addr::from(<[u8; 4]>::get(d)?),
            u8::get(d)?,
        )
        .map_err(|_| CodecError::Invalid("network interface"))
    }
}
impl Wire for FilesystemArchive {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        let b = self.as_bytes();
        (b.len() as u64).put(e)?;
        e.output.write_all(b)?;
        Ok(())
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        let bytes = Vec::<u8>::get(d)?;
        Self::new_bounded(
            bytes,
            ArchiveValidationLimits {
                entries: d.limits.archive_entries,
                path_bytes: d.limits.archive_path_bytes,
                expanded_bytes: d.limits.archive_expanded_bytes,
                metadata_bytes: d.limits.allocation_bytes - d.allocation,
            },
        )
        .map_err(CodecError::Archive)
    }
}
impl Wire for DiskImage {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        self.capacity_bytes().put(e)?;
        match self.image() {
            None => 0u8.put(e),
            Some(b) => {
                1u8.put(e)?;
                (b.len() as u64).put(e)?;
                e.output.write_all(b)?;
                Ok(())
            }
        }
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        let capacity = u64::get(d)?;
        match u8::get(d)? {
            0 => Self::zeroed(capacity).map_err(|_| CodecError::Invalid("zero disk capacity")),
            1 => {
                let b = Vec::<u8>::get(d)?;
                require(b.len() as u64 == capacity, "disk base capacity mismatch")?;
                d.charge_bulk(capacity)?;
                Self::from_bytes(b).map_err(|_| CodecError::Invalid("image disk capacity"))
            }
            _ => Err(CodecError::Invalid("disk kind")),
        }
    }
}
