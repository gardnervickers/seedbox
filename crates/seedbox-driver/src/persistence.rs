//! Closed Driver continuation format v1. Field/tag lists are explicit at owners.
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    io::{self, Read, Write},
    num::{NonZeroI64, NonZeroU32},
    time::Duration,
};

/// A SHA-256 content-addressed encoded snapshot, independent of backend or host paths.
/// Verify identity against the original encoded bytes, not a semantic decode
/// followed by re-encoding: accepted encodings need not be canonical.
// TODO: Re-evaluate: the outer execution envelope chooses the backend; Driver
// only needs immutable content identity and length, not a backend registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotReference {
    digest: [u8; 32],
    length: u64,
}
impl SnapshotReference {
    /// Describe an artifact; its resolver must verify digest and encoded length.
    pub const fn new(digest: [u8; 32], length: u64) -> Self {
        Self { digest, length }
    }
    /// The artifact's content digest, not an engine/build provenance stamp.
    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
    /// Exact encoded artifact length in bytes; verified by the offline resolver.
    pub const fn encoded_length(&self) -> u64 {
        self.length
    }
}

/// Explicit offline decode budgets. These bound logical allocations and input,
/// not process RSS, allocator overhead, or native restore resource requirements.
// TODO: Re-evaluate: import must bound counts before allocation without imposing
// backend resource minima or an artificial RAM cap on future definitions.
#[derive(Clone, Copy, Debug)]
pub struct DecodeLimits {
    /// Whole encoded object, including its fixed header.
    pub encoded_bytes: u64,
    /// Logical retained vector/string/element storage plus bounded validation
    /// scratch (at least twice non-blob metadata storage is reserved for pure
    /// validation). Released archive scratch may be reused; not process RSS.
    pub allocation_bytes: u64,
    /// Maximum items in any single collection.
    pub collection_items: u64,
    /// Aggregate collection items, including nested and import ownership maps.
    /// Before validation, two additional temporary index entries per already
    /// charged entry are conservatively reserved (three times that running sum).
    pub total_items: u64,
    /// Maximum UTF-8 bytes in one string.
    pub string_bytes: u64,
    /// Maximum bytes in one raw byte vector.
    pub blob_bytes: u64,
    /// Maximum wire nesting of structured records, collections and JSON values,
    /// not JSON depth alone (an array adds both a value and a collection level).
    /// 128 is a sensible starting point for the current schema. The trusted
    /// caller must choose a bound suitable for its stack: decoding and later
    /// JSON clone/drop recurse. Arbitrarily large limits can exhaust the stack;
    /// this logical quota is not process isolation or a hard RSS guarantee.
    pub depth: u32,
    /// Maximum tar entries, including implicit directories, per future archive.
    pub archive_entries: u64,
    /// Aggregate retained path/link bytes per future archive.
    pub archive_path_bytes: u64,
    /// Maximum expanded file bytes validated per future archive.
    pub archive_expanded_bytes: u64,
}
/// Malformed/unsupported input, an explicit budget failure, or I/O failure.
#[derive(Debug)]
pub enum CodecError {
    Invalid(&'static str),
    Limit(&'static str),
    Archive(seedbox_machine::definition::DefinitionError),
    Io(io::Error),
}
impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(s) => write!(f, "invalid driver checkpoint: {s}"),
            Self::Limit(s) => write!(f, "driver checkpoint decode limit: {s}"),
            Self::Io(e) => write!(f, "driver checkpoint I/O: {e}"),
            Self::Archive(e) => write!(f, "driver checkpoint future filesystem: {e}"),
        }
    }
}
impl std::error::Error for CodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Archive(e) => Some(e),
            _ => None,
        }
    }
}
impl From<io::Error> for CodecError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
pub(crate) type Result<T> = std::result::Result<T, CodecError>;
pub(crate) fn require(ok: bool, message: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(CodecError::Invalid(message))
    }
}
pub(crate) struct Encoder<W> {
    pub(crate) output: W,
}
pub(crate) struct Decoder<R> {
    pub(crate) input: R,
    pub(crate) remaining: u64,
    pub(crate) limits: DecodeLimits,
    pub(crate) allocation: u64,
    pub(crate) bulk: u64,
    items: u64,
    depth: u32,
}
impl<R: Read> Decoder<R> {
    pub(crate) fn new(input: R, remaining: u64, limits: DecodeLimits) -> Self {
        Self {
            input,
            remaining,
            limits,
            allocation: 0,
            bulk: 0,
            items: 0,
            depth: 0,
        }
    }
    fn bytes(&mut self, out: &mut [u8]) -> Result<()> {
        let n = out.len() as u64;
        if n > self.remaining {
            return Err(CodecError::Invalid("truncated field"));
        }
        self.input.read_exact(out)?;
        self.remaining -= n;
        Ok(())
    }
    pub(crate) fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.depth >= self.limits.depth {
            return Err(CodecError::Limit("nesting"));
        }
        self.depth += 1;
        let result = f(self);
        self.depth -= 1;
        result
    }
    fn byte_count(&mut self, n: u64) -> Result<usize> {
        if n > self.remaining {
            return Err(CodecError::Invalid("truncated byte body"));
        }
        self.charge(n)?;
        usize::try_from(n).map_err(|_| CodecError::Limit("host byte length"))
    }
    pub(crate) fn charge_bulk(&mut self, n: u64) -> Result<()> {
        self.charge(n)?;
        self.bulk = self
            .bulk
            .checked_add(n)
            .ok_or(CodecError::Limit("bulk accounting overflow"))?;
        Ok(())
    }
    fn blob_count(&mut self, n: u64) -> Result<usize> {
        if n > self.remaining {
            return Err(CodecError::Invalid("truncated blob"));
        }
        self.charge_bulk(n)?;
        usize::try_from(n).map_err(|_| CodecError::Limit("host blob length"))
    }
    fn count<T>(&mut self, n: u64) -> Result<usize> {
        if n > self.limits.collection_items || n > self.remaining {
            return Err(CodecError::Limit("collection length"));
        }
        self.claim_scratch::<T>(n)
    }
    /// Charge a separately constructed identity/name index or ownership map
    /// before its allocation. Its source count was already decoded/bounded;
    /// unlike a wire count, it need not fit the as-yet unread suffix again.
    pub(crate) fn claim_scratch<T>(&mut self, n: u64) -> Result<usize> {
        if n > self.limits.collection_items {
            return Err(CodecError::Limit("collection length"));
        }
        self.items = self
            .items
            .checked_add(n)
            .ok_or(CodecError::Limit("item count overflow"))?;
        if self.items > self.limits.total_items {
            return Err(CodecError::Limit("aggregate items"));
        }
        self.charge(
            n.checked_mul(std::mem::size_of::<T>() as u64)
                .ok_or(CodecError::Limit("allocation overflow"))?,
        )?;
        usize::try_from(n).map_err(|_| CodecError::Limit("host collection length"))
    }
    /// Canonical tracker capacity and minimum field size are known before any
    /// map entry is read or allocated; retain the ordinary aggregate quotas.
    pub(crate) fn bounded_count<T>(
        &mut self,
        n: u64,
        capacity: u64,
        minimum: u64,
    ) -> Result<usize> {
        if n > capacity {
            return Err(CodecError::Invalid("connection capacity"));
        }
        if minimum == 0 || n > self.remaining / minimum {
            return Err(CodecError::Invalid("connection count exceeds body"));
        }
        self.count::<T>(n)
    }
    /// Conservatively reserve validation indexes before any validator runs.
    /// At most two temporary entries per decoded/import ownership entry cover
    /// simultaneous names, MACs, cuts, handle texts and pending-owner sets.
    /// Bulk byte bodies are excluded from the independently supplied byte bound.
    pub(crate) fn validation_scratch(&mut self, bytes: u64) -> Result<()> {
        self.items = self
            .items
            .checked_mul(3)
            .ok_or(CodecError::Limit("item count overflow"))?;
        if self.items > self.limits.total_items {
            return Err(CodecError::Limit("aggregate items"));
        }
        self.charge(bytes)
    }
    pub(crate) fn charge(&mut self, n: u64) -> Result<()> {
        self.allocation = self
            .allocation
            .checked_add(n)
            .ok_or(CodecError::Limit("allocation overflow"))?;
        if self.allocation > self.limits.allocation_bytes {
            Err(CodecError::Limit("aggregate allocation"))
        } else {
            Ok(())
        }
    }
}
pub(crate) trait Wire: Sized {
    const RAW_BYTES: bool = false;
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()>;
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self>;
    fn put_slice<W: Write>(values: &[Self], e: &mut Encoder<W>) -> Result<()> {
        for value in values {
            value.put(e)?
        }
        Ok(())
    }
    fn get_vec<R: Read>(d: &mut Decoder<R>, n: usize) -> Result<Vec<Self>> {
        let mut v = Vec::new();
        v.try_reserve_exact(n)
            .map_err(|_| CodecError::Limit("allocation failed"))?;
        for _ in 0..n {
            v.push(Self::get(d)?)
        }
        Ok(v)
    }
}
macro_rules! wire_struct {
    ($ty:ty { $($field:ident),* $(,)? }) => { impl $crate::persistence::Wire for $ty {
        fn put<W:std::io::Write>(&self,e:&mut $crate::persistence::Encoder<W>)->$crate::persistence::Result<()> {$( $crate::persistence::Wire::put(&self.$field,e)?; )* Ok(())}
        fn get<R:std::io::Read>(d:&mut $crate::persistence::Decoder<R>)->$crate::persistence::Result<Self> { d.nested(|d|Ok(Self{$($field:$crate::persistence::Wire::get(d)?),*})) }
    }};
}
macro_rules! wire_tuple_struct {
    ($ty:ty) => {
        impl $crate::persistence::Wire for $ty {
            fn put<W: std::io::Write>(
                &self,
                e: &mut $crate::persistence::Encoder<W>,
            ) -> $crate::persistence::Result<()> {
                $crate::persistence::Wire::put(&self.0, e)
            }
            fn get<R: std::io::Read>(
                d: &mut $crate::persistence::Decoder<R>,
            ) -> $crate::persistence::Result<Self> {
                Ok(Self($crate::persistence::Wire::get(d)?))
            }
        }
    };
}
macro_rules! wire_enum {
    // Only these foreign non_exhaustive enums need a write fallback. Owned
    // enums deliberately use exhaustive matches, so new variants fail to build.
    (foreign Request; $($body:tt)*) => {wire_enum!(@impl Request; {_=>Err($crate::persistence::CodecError::Invalid("unsupported wire variant")),} $($body)*);};
    (foreign Response; $($body:tt)*) => {wire_enum!(@impl Response; {_=>Err($crate::persistence::CodecError::Invalid("unsupported wire variant")),} $($body)*);};
    ($ty:ty; $($body:tt)*) => {wire_enum!(@impl $ty; {} $($body)*);};
    (@impl $ty:ty; {$($fallback:tt)*} units {$($ut:literal => $uv:ident),* $(,)?} tuples {$($tt:literal => $tv:ident($($tf:ident),*)),* $(,)?} records {$($rt:literal => $rv:ident{$($rf:ident),*}),* $(,)?}) => {impl $crate::persistence::Wire for $ty {
        fn put<W:std::io::Write>(&self,e:&mut $crate::persistence::Encoder<W>)->$crate::persistence::Result<()> {use $crate::persistence::Wire;match self{
            $(Self::$uv=>Wire::put(&($ut as u32),e),)*
            $(Self::$tv($($tf),*)=>{Wire::put(&($tt as u32),e)?;$(Wire::put($tf,e)?;)*Ok(())},)*
            $(Self::$rv{$($rf),*}=>{Wire::put(&($rt as u32),e)?;$(Wire::put($rf,e)?;)*Ok(())},)*
            $($fallback)*
        }}
        fn get<R:std::io::Read>(d:&mut $crate::persistence::Decoder<R>)->$crate::persistence::Result<Self>{d.nested(|d|match <u32 as $crate::persistence::Wire>::get(d)?{
            $($ut=>Ok(Self::$uv),)*
            $($tt=>Ok(Self::$tv($(wire_get!(d,$tf)),*)),)*
            $($rt=>Ok(Self::$rv{$($rf:$crate::persistence::Wire::get(d)?),*}),)*
            _=>Err($crate::persistence::CodecError::Invalid(concat!("unknown ",stringify!($ty)," tag"))),
        })}
    }};
}
// Tuple construction needs a repetition keyed by the binding identifier.
macro_rules! wire_get {
    ($d:ident,$name:ident) => {
        $crate::persistence::Wire::get($d)?
    };
}
macro_rules! integer {($($t:ty),*)=>{$(impl Wire for $t{
 fn put<W:Write>(&self,e:&mut Encoder<W>)->Result<()>{e.output.write_all(&self.to_le_bytes())?;Ok(())}
 fn get<R:Read>(d:&mut Decoder<R>)->Result<Self>{let mut b=[0;std::mem::size_of::<Self>()];d.bytes(&mut b)?;Ok(Self::from_le_bytes(b))}
})*};}
integer!(u16, u32, u64, i32, i64, i128, f64);
impl Wire for u8 {
    const RAW_BYTES: bool = true;
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        e.output.write_all(&[*self])?;
        Ok(())
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        let mut b = [0];
        d.bytes(&mut b)?;
        Ok(b[0])
    }
    fn put_slice<W: Write>(v: &[Self], e: &mut Encoder<W>) -> Result<()> {
        e.output.write_all(v)?;
        Ok(())
    }
    fn get_vec<R: Read>(d: &mut Decoder<R>, n: usize) -> Result<Vec<Self>> {
        let mut v = Vec::new();
        v.try_reserve_exact(n)
            .map_err(|_| CodecError::Limit("allocation failed"))?;
        v.resize(n, 0);
        d.bytes(&mut v)?;
        Ok(v)
    }
}
impl Wire for usize {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        u64::try_from(*self)
            .map_err(|_| CodecError::Invalid("index overflow"))?
            .put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        usize::try_from(u64::get(d)?).map_err(|_| CodecError::Limit("host index width"))
    }
}
impl Wire for bool {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        u8::from(*self).put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        match u8::get(d)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CodecError::Invalid("boolean tag")),
        }
    }
}
impl<T: Wire> Wire for Option<T> {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        match self {
            None => 0u8.put(e),
            Some(x) => {
                1u8.put(e)?;
                x.put(e)
            }
        }
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        d.nested(|d| match u8::get(d)? {
            0 => Ok(None),
            1 => Ok(Some(T::get(d)?)),
            _ => Err(CodecError::Invalid("option tag")),
        })
    }
}
impl<T: Wire> Wire for Vec<T> {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        (self.len() as u64).put(e)?;
        T::put_slice(self, e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        d.nested(|d| {
            let n = u64::get(d)?;
            if T::RAW_BYTES && n > d.limits.blob_bytes {
                return Err(CodecError::Limit("blob bytes"));
            }
            let n = if T::RAW_BYTES {
                d.blob_count(n)?
            } else {
                d.count::<T>(n)?
            };
            T::get_vec(d, n)
        })
    }
}
impl Wire for String {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        (self.len() as u64).put(e)?;
        e.output.write_all(self.as_bytes())?;
        Ok(())
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        let n = u64::get(d)?;
        if n > d.limits.string_bytes {
            return Err(CodecError::Limit("string bytes"));
        }
        let n = d.byte_count(n)?;
        String::from_utf8(u8::get_vec(d, n)?).map_err(|_| CodecError::Invalid("UTF-8"))
    }
}
impl Wire for &'static str {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        match *self {
            "realtime step ended with its machine incarnation" => 0u8.put(e),
            "zero realtime step" => 1u8.put(e),
            _ => Err(CodecError::Invalid("unknown static note")),
        }
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        match u8::get(d)? {
            0 => Ok("realtime step ended with its machine incarnation"),
            1 => Ok("zero realtime step"),
            _ => Err(CodecError::Invalid("unknown note tag")),
        }
    }
}
impl<T: Wire> Wire for VecDeque<T> {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        (self.len() as u64).put(e)?;
        for x in self {
            x.put(e)?
        }
        Ok(())
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        d.nested(|d| {
            let n = u64::get(d)?;
            let n = d.count::<T>(n)?;
            let mut v = VecDeque::new();
            v.try_reserve_exact(n)
                .map_err(|_| CodecError::Limit("allocation failed"))?;
            for _ in 0..n {
                v.push_back(T::get(d)?)
            }
            Ok(v)
        })
    }
}
impl<K: Wire + Ord, V: Wire> Wire for BTreeMap<K, V> {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        (self.len() as u64).put(e)?;
        for (k, v) in self {
            k.put(e)?;
            v.put(e)?
        }
        Ok(())
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        d.nested(|d| {
            let n = u64::get(d)?;
            let n = d.count::<(K, V)>(n)?;
            let mut m = Self::new();
            for _ in 0..n {
                let k = K::get(d)?;
                require(
                    m.last_key_value().is_none_or(|(last, _)| last < &k),
                    "duplicate or unordered map key",
                )?;
                let v = V::get(d)?;
                m.insert(k, v);
            }
            Ok(m)
        })
    }
}
impl<T: Wire + Ord> Wire for BTreeSet<T> {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        (self.len() as u64).put(e)?;
        for x in self {
            x.put(e)?
        }
        Ok(())
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        d.nested(|d| {
            let n = u64::get(d)?;
            let n = d.count::<T>(n)?;
            let mut m = Self::new();
            for _ in 0..n {
                let k = T::get(d)?;
                require(
                    m.last().is_none_or(|last| last < &k),
                    "duplicate or unordered set member",
                )?;
                m.insert(k);
            }
            Ok(m)
        })
    }
}
macro_rules! tuple {($($t:ident:$i:tt),*)=>{impl<$($t:Wire),*> Wire for ($($t,)*){fn put<W:Write>(&self,e:&mut Encoder<W>)->Result<()>{$(self.$i.put(e)?;)*Ok(())}fn get<R:Read>(d:&mut Decoder<R>)->Result<Self>{Ok(($($t::get(d)?,)*))}}};}
tuple!(A:0,B:1);
tuple!(A:0,B:1,C:2);
tuple!(A:0,B:1,C:2,D:3);
impl<const N: usize> Wire for [u8; N] {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        e.output.write_all(self)?;
        Ok(())
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        let mut v = [0; N];
        d.bytes(&mut v)?;
        Ok(v)
    }
}
macro_rules! nonzero {
    ($t:ty,$inner:ty) => {
        impl Wire for $t {
            fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
                self.get().put(e)
            }
            fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
                Self::new(<$inner>::get(d)?).ok_or(CodecError::Invalid("zero correlation ID/value"))
            }
        }
    };
}
nonzero!(NonZeroU32, u32);
nonzero!(NonZeroI64, i64);
impl Wire for Duration {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        u64::try_from(self.as_nanos())
            .map_err(|_| CodecError::Invalid("duration overflow"))?
            .put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        Ok(Self::from_nanos(u64::get(d)?))
    }
}
wire_struct!(SnapshotReference { digest, length });
mod common;
impl Wire for serde_json::Value {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        use serde_json::Value::*;
        match self {
            Null => 0u8.put(e),
            Bool(v) => {
                1u8.put(e)?;
                v.put(e)
            }
            Number(v) => {
                if let Some(n) = v.as_i64().filter(|n| &serde_json::Number::from(*n) == v) {
                    2u8.put(e)?;
                    n.put(e)
                } else if let Some(n) = v.as_u64().filter(|n| &serde_json::Number::from(*n) == v) {
                    3u8.put(e)?;
                    n.put(e)
                } else if let Some(n) = v
                    .as_f64()
                    .filter(|n| serde_json::Number::from_f64(*n).as_ref() == Some(v))
                {
                    4u8.put(e)?;
                    n.put(e)
                } else {
                    // Preserve exact decimal forms if serde_json's optional
                    // arbitrary-precision representation is unified by a caller.
                    8u8.put(e)?;
                    v.to_string().put(e)
                }
            }
            String(v) => {
                5u8.put(e)?;
                v.put(e)
            }
            Array(v) => {
                6u8.put(e)?;
                v.put(e)
            }
            Object(v) => {
                7u8.put(e)?;
                (v.len() as u64).put(e)?;
                for (k, v) in v {
                    k.put(e)?;
                    v.put(e)?
                }
                Ok(())
            }
        }
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        d.nested(|d| {
            use serde_json::Value::*;
            Ok(match u8::get(d)? {
                0 => Null,
                1 => Bool(bool::get(d)?),
                2 => Number(i64::get(d)?.into()),
                3 => Number(u64::get(d)?.into()),
                4 => Number(
                    serde_json::Number::from_f64(f64::get(d)?)
                        .ok_or(CodecError::Invalid("nonfinite JSON number"))?,
                ),
                5 => String(<std::string::String>::get(d)?),
                6 => Array(Vec::get(d)?),
                7 => {
                    let n = u64::get(d)?;
                    let n = d.count::<(std::string::String, Self)>(n)?;
                    let mut object = serde_json::Map::new();
                    for _ in 0..n {
                        let key = <std::string::String>::get(d)?;
                        require(!object.contains_key(&key), "duplicate JSON object key")?;
                        object.insert(key, <Self as Wire>::get(d)?);
                    }
                    Object(object)
                }
                8 => {
                    let text = <std::string::String>::get(d)?;
                    d.charge(
                        (text.len() as u64)
                            .checked_mul(2)
                            .ok_or(CodecError::Limit("number allocation overflow"))?,
                    )?;
                    let number: serde_json::Number = text
                        .parse()
                        .map_err(|_| CodecError::Invalid("JSON decimal number"))?;
                    require(
                        number.to_string() == text,
                        "inexact or trailing JSON decimal",
                    )?;
                    Number(number)
                }
                _ => return Err(CodecError::Invalid("JSON tag")),
            })
        })
    }
}
