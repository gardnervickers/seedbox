use seedbox_machine::snapshot::{CodecError, Reader, Result, Writer};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{Read, Write},
    num::NonZeroU32,
};
pub(crate) trait Record: Sized {
    const MIN: u64;
    fn encode<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()>;
    fn decode<R: Read>(r: &mut Reader<'_, R>) -> Result<Self>;
    fn encode_slice<W: Write>(values: &[Self], w: &mut Writer<'_, W>) -> Result<()> {
        w.u64(values.len() as u64)?;
        for v in values {
            v.encode(w)?;
        }
        Ok(())
    }
    fn decode_vec<R: Read>(r: &mut Reader<'_, R>) -> Result<Vec<Self>> {
        let n = r.count::<Self>(Self::MIN)?;
        let mut out = Vec::new();
        seedbox_machine::snapshot::reserve(&mut out, n)?;
        for _ in 0..n {
            out.push(Self::decode(r)?);
        }
        Ok(out)
    }
    fn encode_deque<W: Write>(values: &VecDeque<Self>, w: &mut Writer<'_, W>) -> Result<()> {
        w.u64(values.len() as u64)?;
        for v in values {
            v.encode(w)?;
        }
        Ok(())
    }
}
macro_rules! scalar {
    ($t:ty,$m:ident,$n:expr) => {
        impl Record for $t {
            const MIN: u64 = $n;
            fn encode<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()> {
                w.$m(*self)
            }
            fn decode<R: Read>(r: &mut Reader<'_, R>) -> Result<Self> {
                r.$m()
            }
        }
    };
}
impl Record for u8 {
    const MIN: u64 = 1;
    fn encode<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()> {
        w.u8(*self)
    }
    fn decode<R: Read>(r: &mut Reader<'_, R>) -> Result<Self> {
        r.u8()
    }
    fn encode_slice<W: Write>(values: &[Self], w: &mut Writer<'_, W>) -> Result<()> {
        w.bytes(values)
    }
    fn decode_vec<R: Read>(r: &mut Reader<'_, R>) -> Result<Vec<Self>> {
        r.bytes()
    }
    fn encode_deque<W: Write>(values: &VecDeque<Self>, w: &mut Writer<'_, W>) -> Result<()> {
        w.u64(values.len() as u64)?;
        let (first, second) = values.as_slices();
        w.raw(first)?;
        w.raw(second)
    }
}
scalar!(u16, u16, 2);
scalar!(u32, u32, 4);
scalar!(u64, u64, 8);
impl Record for bool {
    const MIN: u64 = 1;
    fn encode<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()> {
        w.boolean(*self)
    }
    fn decode<R: Read>(r: &mut Reader<'_, R>) -> Result<Self> {
        r.boolean()
    }
}
impl Record for NonZeroU32 {
    const MIN: u64 = 4;
    fn encode<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()> {
        w.u32(self.get())
    }
    fn decode<R: Read>(r: &mut Reader<'_, R>) -> Result<Self> {
        Self::new(r.u32()?).ok_or(CodecError::Invalid("zero identity"))
    }
}
impl Record for String {
    const MIN: u64 = 8;
    fn encode<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()> {
        w.string(self)
    }
    fn decode<R: Read>(r: &mut Reader<'_, R>) -> Result<Self> {
        r.string()
    }
}
impl<T: Record> Record for Option<T> {
    const MIN: u64 = 1;
    fn encode<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()> {
        match self {
            None => w.u8(0),
            Some(v) => {
                w.u8(1)?;
                v.encode(w)
            }
        }
    }
    fn decode<R: Read>(r: &mut Reader<'_, R>) -> Result<Self> {
        match r.u8()? {
            0 => Ok(None),
            1 => Ok(Some(T::decode(r)?)),
            _ => Err(CodecError::Invalid("option tag")),
        }
    }
}
impl<A: Record, B: Record> Record for (A, B) {
    const MIN: u64 = A::MIN + B::MIN;
    fn encode<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()> {
        self.0.encode(w)?;
        self.1.encode(w)
    }
    fn decode<R: Read>(r: &mut Reader<'_, R>) -> Result<Self> {
        Ok((A::decode(r)?, B::decode(r)?))
    }
}
impl<T: Record, const N: usize> Record for [T; N] {
    const MIN: u64 = T::MIN * N as u64;
    fn encode<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()> {
        for x in self {
            x.encode(w)?;
        }
        Ok(())
    }
    fn decode<R: Read>(r: &mut Reader<'_, R>) -> Result<Self> {
        // Fixed arrays are schema-bounded, never sized by imported input.
        let mut out: [Option<T>; N] = std::array::from_fn(|_| None);
        for x in &mut out {
            *x = Some(T::decode(r)?);
        }
        Ok(out.map(|x| x.expect("initialized fixed array")))
    }
}
impl<T: Record> Record for Vec<T> {
    const MIN: u64 = 8;
    fn encode<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()> {
        T::encode_slice(self, w)
    }
    fn decode<R: Read>(r: &mut Reader<'_, R>) -> Result<Self> {
        T::decode_vec(r)
    }
}
impl<T: Record> Record for VecDeque<T> {
    const MIN: u64 = 8;
    fn encode<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()> {
        T::encode_deque(self, w)
    }
    fn decode<R: Read>(r: &mut Reader<'_, R>) -> Result<Self> {
        // VecDeque::from(Vec) transfers its backing allocation in O(1).
        // The same logical element storage remains charged, without a copy.
        Ok(T::decode_vec(r)?.into())
    }
}
impl<T: Record + Ord + Copy> Record for BTreeSet<T> {
    const MIN: u64 = 8;
    fn encode<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()> {
        w.u64(self.len() as u64)?;
        for x in self {
            x.encode(w)?;
        }
        Ok(())
    }
    fn decode<R: Read>(r: &mut Reader<'_, R>) -> Result<Self> {
        let n = r.count::<T>(T::MIN)?;
        let mut v = BTreeSet::new();
        let mut last = None;
        for _ in 0..n {
            let x = T::decode(r)?;
            if last.is_some_and(|p| p >= x) {
                return Err(CodecError::Invalid("set order or duplicate"));
            }
            last = Some(x);
            v.insert(x);
        }
        Ok(v)
    }
}
impl<K: Record + Ord + Copy, V: Record> Record for BTreeMap<K, V> {
    const MIN: u64 = 8;
    fn encode<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()> {
        w.u64(self.len() as u64)?;
        for (k, v) in self {
            k.encode(w)?;
            v.encode(w)?;
        }
        Ok(())
    }
    fn decode<R: Read>(r: &mut Reader<'_, R>) -> Result<Self> {
        let n = r.count::<(K, V)>(K::MIN + V::MIN)?;
        let mut v = BTreeMap::new();
        let mut last = None;
        for _ in 0..n {
            let k = K::decode(r)?;
            if last.is_some_and(|p| p >= k) {
                return Err(CodecError::Invalid("map order or duplicate"));
            }
            last = Some(k);
            v.insert(k, V::decode(r)?);
        }
        Ok(v)
    }
}
