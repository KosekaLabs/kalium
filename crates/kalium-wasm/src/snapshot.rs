//! Instance snapshots: the state of an instance's own objects, captured
//! for a fast restart.
//!
//! A [`Snapshot`] holds the contents of an instance's linear memory, the
//! values of its globals, the entries of its tables and which data and
//! element segments it has dropped: everything that running the module's
//! initialisation (its active segments, its start function, the
//! constructors a language runtime runs before `main`) leaves behind, and
//! nothing about an execution in flight (no value stack, no frames). It is
//! taken by [`Instance::snapshot`](crate::Instance::snapshot) and restored
//! by [`Instance::from_snapshot`](crate::Instance::from_snapshot), which
//! allocates a new instance with these contents instead of applying the
//! module's segments: a memcpy of the memory, a copy of the table entries,
//! and nothing run. The restored instance's `init_segments` is a no-op.
//!
//! Function references in globals and tables are stored as *function
//! indices* of the instance, not function addresses, so that the snapshot
//! does not depend on the function address range the instance was given
//! and can be restored at any `func_base`; an import's reference is
//! restored as the address of whatever the import resolves to. A reference
//! to a function of another instance (a foreign address, which can only
//! get into an unshared table through the host) cannot be captured.
//!
//! # Byte format
//!
//! [`Snapshot::to_bytes`] and [`Snapshot::from_bytes`] use a small
//! versioned little-endian encoding, so that the kernel can keep a
//! snapshot in a memory object and the decoder can reject anything that
//! does not fit (every length is checked against the bytes that remain
//! before anything is allocated):
//!
//! ```text
//! magic        4 bytes   "KSNP"
//! version      u32       1
//! pages        u32       size of the memory in 64 KiB pages
//! memory       pages * 65536 bytes
//! globals      u32 count, then count values
//! tables       u32 count, then per table: u32 length, then length values
//! data flags   u32 count, then count bytes (1 = dropped)
//! elem flags   u32 count, then count bytes (1 = dropped)
//! ```
//!
//! A value is 9 bytes: a type tag (the binary format's `valtype` byte:
//! `0x7F` i32, `0x7E` i64, `0x7D` f32, `0x7C` f64, `0x70` funcref, `0x6F`
//! externref) and a `u64` payload: the integer (an `i32` zero-extended
//! from its `u32` bits), the float's bits, or the reference (its function
//! index or extern value, `u64::MAX` for null). Nothing may follow the
//! last flag.

use crate::error::Error;
use crate::exec::Value;
use crate::types::PAGE_SIZE;
use alloc::vec::Vec;
use core::fmt;

const MAGIC: &[u8; 4] = b"KSNP";
const VERSION: u32 = 1;
const VALUE_LEN: usize = 9;
const NULL: u64 = u64::MAX;

const TAG_I32: u8 = 0x7F;
const TAG_I64: u8 = 0x7E;
const TAG_F32: u8 = 0x7D;
const TAG_F64: u8 = 0x7C;
const TAG_FUNCREF: u8 = 0x70;
const TAG_EXTERNREF: u8 = 0x6F;

fn malformed(msg: &'static str) -> Error {
    Error::Snapshot { msg }
}

/// The captured state of an instance's own memory, globals and tables
/// (see the module documentation). Built by
/// [`Instance::snapshot`](crate::Instance::snapshot) or
/// [`Snapshot::from_bytes`].
#[derive(Clone, PartialEq)]
pub struct Snapshot {
    /// The memory's contents, a whole number of pages (empty for a module
    /// without a memory).
    pub(crate) memory: Vec<u8>,
    /// The globals' values, in index order. A `FuncRef` holds a function
    /// index, not an address.
    pub(crate) globals: Vec<Value>,
    /// The tables' entries, in index order; references as in `globals`.
    pub(crate) tables: Vec<Vec<Value>>,
    /// Which data segments were dropped (active ones count as dropped once
    /// applied).
    pub(crate) data_dropped: Vec<bool>,
    /// Which element segments were dropped (active and declarative ones
    /// count as dropped once instantiated).
    pub(crate) elem_dropped: Vec<bool>,
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("memory_pages", &self.memory_pages())
            .field("globals", &self.globals)
            .field(
                "tables",
                &self.tables.iter().map(Vec::len).collect::<Vec<_>>(),
            )
            .field("data_dropped", &self.data_dropped)
            .field("elem_dropped", &self.elem_dropped)
            .finish()
    }
}

impl Snapshot {
    /// The size of the captured memory in pages.
    pub fn memory_pages(&self) -> u32 {
        (self.memory.len() / PAGE_SIZE) as u32
    }

    /// The captured memory contents.
    pub fn memory(&self) -> &[u8] {
        &self.memory
    }

    /// The number of globals captured.
    pub fn num_globals(&self) -> usize {
        self.globals.len()
    }

    /// The number of tables captured.
    pub fn num_tables(&self) -> usize {
        self.tables.len()
    }

    /// The length of [`Snapshot::to_bytes`]'s output.
    pub fn encoded_len(&self) -> usize {
        let values = self.globals.len() + self.tables.iter().map(Vec::len).sum::<usize>();
        4 + 4
            + 4
            + self.memory.len()
            + 4
            + 4
            + 4 * self.tables.len()
            + values * VALUE_LEN
            + 4
            + self.data_dropped.len()
            + 4
            + self.elem_dropped.len()
    }

    /// Encode the snapshot (see the module documentation).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&self.memory_pages().to_le_bytes());
        out.extend_from_slice(&self.memory);
        out.extend_from_slice(&(self.globals.len() as u32).to_le_bytes());
        for v in &self.globals {
            encode_value(&mut out, *v);
        }
        out.extend_from_slice(&(self.tables.len() as u32).to_le_bytes());
        for t in &self.tables {
            out.extend_from_slice(&(t.len() as u32).to_le_bytes());
            for v in t {
                encode_value(&mut out, *v);
            }
        }
        out.extend_from_slice(&(self.data_dropped.len() as u32).to_le_bytes());
        out.extend(self.data_dropped.iter().map(|&d| d as u8));
        out.extend_from_slice(&(self.elem_dropped.len() as u32).to_le_bytes());
        out.extend(self.elem_dropped.iter().map(|&d| d as u8));
        out
    }

    /// Decode a snapshot written by [`Snapshot::to_bytes`]. Every length
    /// is checked against the bytes that remain before it is used, so a
    /// truncated, oversized or otherwise malformed encoding is
    /// [`Error::Snapshot`], never a panic or an outsized allocation.
    /// Whether the snapshot fits a module is checked on restore.
    pub fn from_bytes(bytes: &[u8]) -> Result<Snapshot, Error> {
        let mut r = Reader { bytes, pos: 0 };
        if r.take(4)? != MAGIC {
            return Err(malformed("not a snapshot (bad magic)"));
        }
        if r.u32()? != VERSION {
            return Err(malformed("unsupported snapshot version"));
        }
        let pages = r.u32()? as usize;
        let len = pages
            .checked_mul(PAGE_SIZE)
            .filter(|&n| n <= r.remaining())
            .ok_or_else(|| malformed("memory contents truncated"))?;
        let memory = r.take(len)?.to_vec();
        let num_globals = r.count(VALUE_LEN)?;
        let globals = r.values(num_globals)?;
        let num_tables = r.count(4)?;
        let mut tables = Vec::with_capacity(num_tables);
        for _ in 0..num_tables {
            let n = r.count(VALUE_LEN)?;
            tables.push(r.values(n)?);
        }
        let data_dropped = r.flags()?;
        let elem_dropped = r.flags()?;
        if r.remaining() != 0 {
            return Err(malformed("trailing bytes after the snapshot"));
        }
        Ok(Snapshot {
            memory,
            globals,
            tables,
            data_dropped,
            elem_dropped,
        })
    }
}

fn encode_value(out: &mut Vec<u8>, v: Value) {
    let (tag, payload) = match v {
        Value::I32(x) => (TAG_I32, x as u32 as u64),
        Value::I64(x) => (TAG_I64, x as u64),
        Value::F32(x) => (TAG_F32, x.to_bits() as u64),
        Value::F64(x) => (TAG_F64, x.to_bits()),
        Value::FuncRef(r) => (TAG_FUNCREF, r.map_or(NULL, u64::from)),
        Value::ExternRef(r) => (TAG_EXTERNREF, r.map_or(NULL, u64::from)),
    };
    out.push(tag);
    out.extend_from_slice(&payload.to_le_bytes());
}

fn decode_value(bytes: &[u8]) -> Result<Value, Error> {
    let (&tag, payload) = bytes
        .split_first()
        .ok_or_else(|| malformed("value truncated"))?;
    let payload = u64::from_le_bytes(
        payload
            .try_into()
            .map_err(|_| malformed("value truncated"))?,
    );
    let narrow = || u32::try_from(payload).map_err(|_| malformed("value payload out of range"));
    let reference = || match payload {
        NULL => Ok(None),
        _ => narrow().map(Some),
    };
    Ok(match tag {
        TAG_I32 => Value::I32(narrow()? as i32),
        TAG_I64 => Value::I64(payload as i64),
        TAG_F32 => Value::F32(f32::from_bits(narrow()?)),
        TAG_F64 => Value::F64(f64::from_bits(payload)),
        TAG_FUNCREF => Value::FuncRef(reference()?),
        TAG_EXTERNREF => Value::ExternRef(reference()?),
        _ => return Err(malformed("unknown value type tag")),
    })
}

/// A bounds-checked cursor over the encoding.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.bytes.len())
            .ok_or_else(|| malformed("snapshot truncated"))?;
        let s = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| malformed("snapshot truncated"))?;
        self.pos = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, Error> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes(
            b.try_into().map_err(|_| malformed("snapshot truncated"))?,
        ))
    }

    /// A count of items of `item_len` bytes each, which must all fit in
    /// what remains (so a bogus count never drives an allocation).
    fn count(&mut self, item_len: usize) -> Result<usize, Error> {
        let n = self.u32()? as usize;
        n.checked_mul(item_len)
            .filter(|&total| total <= self.remaining())
            .map(|_| n)
            .ok_or_else(|| malformed("item count exceeds the snapshot's size"))
    }

    fn values(&mut self, n: usize) -> Result<Vec<Value>, Error> {
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(decode_value(self.take(VALUE_LEN)?)?);
        }
        Ok(v)
    }

    fn flags(&mut self) -> Result<Vec<bool>, Error> {
        let n = self.count(1)?;
        let mut v = Vec::with_capacity(n);
        for &b in self.take(n)? {
            v.push(match b {
                0 => false,
                1 => true,
                _ => return Err(malformed("segment flag is neither 0 nor 1")),
            });
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn sample() -> Snapshot {
        Snapshot {
            memory: {
                let mut m = vec![0u8; PAGE_SIZE];
                m[0] = 1;
                m[PAGE_SIZE - 1] = 0xFE;
                m
            },
            globals: vec![
                Value::I32(-7),
                Value::I64(i64::MIN),
                Value::F32(-1.5),
                Value::F64(-0.0),
                Value::FuncRef(None),
                Value::ExternRef(Some(42)),
            ],
            tables: vec![
                vec![Value::FuncRef(Some(3)), Value::FuncRef(None)],
                vec![],
                vec![Value::ExternRef(None)],
            ],
            data_dropped: vec![true, false],
            elem_dropped: vec![false, true, true],
        }
    }

    #[test]
    fn round_trip() {
        let s = sample();
        let bytes = s.to_bytes();
        assert_eq!(bytes.len(), s.encoded_len());
        assert_eq!(&bytes[..4], b"KSNP");
        let back = Snapshot::from_bytes(&bytes).unwrap();
        assert_eq!(back, s);
        assert!(alloc::format!("{back:?}").contains("tables: [2, 0, 1]"));
        // A NaN payload survives bit-exactly (`Value`'s equality cannot
        // say so: NaN != NaN).
        let mut nan = s.clone();
        nan.globals[2] = Value::F32(f32::from_bits(0x7FC0_1234));
        nan.globals[3] = Value::F64(f64::from_bits(0xFFF8_0000_0000_0001));
        let back = Snapshot::from_bytes(&nan.to_bytes()).unwrap();
        match (back.globals[2], back.globals[3]) {
            (Value::F32(f), Value::F64(d)) => {
                assert_eq!(f.to_bits(), 0x7FC0_1234);
                assert_eq!(d.to_bits(), 0xFFF8_0000_0000_0001);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(back.memory_pages(), 1);
        assert_eq!(back.num_globals(), 6);
        assert_eq!(back.num_tables(), 3);

        let empty = Snapshot {
            memory: vec![],
            globals: vec![],
            tables: vec![],
            data_dropped: vec![],
            elem_dropped: vec![],
        };
        assert_eq!(Snapshot::from_bytes(&empty.to_bytes()).unwrap(), empty);
    }

    #[test]
    fn malformed_encodings_are_errors() {
        let s = sample();
        let bytes = s.to_bytes();
        // Every truncation is an error, never a panic.
        for n in 0..bytes.len() {
            assert!(
                matches!(
                    Snapshot::from_bytes(&bytes[..n]),
                    Err(Error::Snapshot { .. })
                ),
                "truncated to {n}"
            );
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert_eq!(
            Snapshot::from_bytes(&extra),
            Err(malformed("trailing bytes after the snapshot"))
        );
        let mut magic = bytes.clone();
        magic[0] = b'X';
        assert_eq!(
            Snapshot::from_bytes(&magic),
            Err(malformed("not a snapshot (bad magic)"))
        );
        let mut version = bytes.clone();
        version[4] = 2;
        assert_eq!(
            Snapshot::from_bytes(&version),
            Err(malformed("unsupported snapshot version"))
        );
        // A page count far beyond the bytes present is refused before
        // anything is allocated.
        let mut pages = bytes.clone();
        pages[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            Snapshot::from_bytes(&pages),
            Err(malformed("memory contents truncated"))
        );
        // Likewise a global count.
        let mut globals = bytes.clone();
        let at = 12 + PAGE_SIZE;
        globals[at..at + 4].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
        assert_eq!(
            Snapshot::from_bytes(&globals),
            Err(malformed("item count exceeds the snapshot's size"))
        );
        // A bad value tag, an out-of-range i32 payload, a bad flag.
        let mut tag = bytes.clone();
        tag[at + 4] = 0x7B;
        assert_eq!(
            Snapshot::from_bytes(&tag),
            Err(malformed("unknown value type tag"))
        );
        let mut payload = bytes.clone();
        payload[at + 4 + 8] = 1;
        assert_eq!(
            Snapshot::from_bytes(&payload),
            Err(malformed("value payload out of range"))
        );
        let mut flag = bytes.clone();
        let n = flag.len();
        flag[n - 1] = 2;
        assert_eq!(
            Snapshot::from_bytes(&flag),
            Err(malformed("segment flag is neither 0 nor 1"))
        );
    }
}
