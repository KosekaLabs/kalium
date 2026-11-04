//! The binary decoder: bytes in, [`Module`] out.
//!
//! Every error carries the byte offset at which decoding failed and a
//! message using the wording of the spec test suite where one exists. The
//! decoder never panics and never allocates proportionally to a length it
//! has not yet checked against the remaining input, so garbage input costs
//! at most time linear in its size.
//!
//! Only binary-format well-formedness is checked here (the spec's
//! "malformed" category plus the few structural checks the spec places in the
//! binary format, like matching function/code section lengths). Type and
//! index checking is the validator's job.

use crate::error::Error;
use crate::instr::{BlockType, BrTable, ConstExpr, FuncBody, Instr, MemArg};
use crate::module::{
    Data, DataMode, ElemItems, ElemMode, Element, Export, ExportDesc, Global, Import, ImportDesc,
    Module,
};
use crate::types::{FuncType, GlobalType, MemType, RefType, ResizableLimits, TableType, ValType};
use alloc::string::String;
use alloc::vec::Vec;

/// Upper bound on the number of declared locals of one function. Real
/// toolchains stay far below this; it keeps a hostile module from making us
/// allocate gigabytes before validation rejects it.
pub const MAX_LOCALS: u32 = 1 << 20;

/// Decode a module without validating it. [`Module::new`] decodes *and*
/// validates; this is for tools and tests that want the raw IR.
pub fn decode(bytes: &[u8]) -> Result<Module, Error> {
    let mut r = Reader::new(bytes);
    r.magic()?;
    let mut m = Module::default();
    let mut last_rank = 0u8;
    let mut seen_code = false;
    let mut uses_data_count = false;

    while !r.eof() {
        let id_off = r.pos;
        let id = r.byte()?;
        let size = r.u32()? as usize;
        let start = r.pos;
        if size > r.remaining() {
            return Err(r.err_at(start, "length out of bounds"));
        }
        let mut s = r.sub(size);
        if id != 0 {
            let rank = section_rank(id).ok_or_else(|| r.err_at(id_off, "malformed section id"))?;
            if rank <= last_rank {
                return Err(r.err_at(id_off, "unexpected content after last section"));
            }
            last_rank = rank;
        }
        match id {
            0 => {
                // Custom section: the name must still be valid UTF-8; the
                // payload is ignored.
                let _ = s.name()?;
                s.pos = s.limit;
            }
            1 => m.types = s.vec(|s| s.func_type())?,
            2 => m.imports = s.vec(|s| s.import())?,
            3 => m.funcs = s.vec(|s| s.u32())?,
            4 => m.tables = s.vec(|s| s.table_type())?,
            5 => m.memories = s.vec(|s| s.mem_type())?,
            6 => m.globals = s.vec(|s| s.global())?,
            7 => m.exports = s.vec(|s| s.export())?,
            8 => m.start = Some(s.u32()?),
            9 => m.elements = s.vec(|s| s.element())?,
            12 => m.data_count = Some(s.u32()?),
            10 => {
                seen_code = true;
                let count = s.u32()? as usize;
                if count != m.funcs.len() {
                    return Err(
                        s.err_at(start, "function and code section have inconsistent lengths")
                    );
                }
                let mut code = Vec::with_capacity(count.min(s.remaining()));
                for _ in 0..count {
                    let body = s.func_body(&mut uses_data_count)?;
                    code.push(body);
                }
                m.code = code;
            }
            11 => m.data = s.vec(|s| s.data())?,
            _ => return Err(r.err_at(id_off, "malformed section id")),
        }
        if s.pos != s.limit {
            return Err(s.err_at(s.pos, "section size mismatch"));
        }
        r.pos = s.limit;
    }

    if !seen_code && !m.funcs.is_empty() {
        return Err(r.err_at(r.pos, "function and code section have inconsistent lengths"));
    }
    if let Some(n) = m.data_count {
        if n as usize != m.data.len() {
            return Err(r.err_at(
                r.pos,
                "data count and data section have inconsistent lengths",
            ));
        }
    } else if uses_data_count {
        return Err(r.err_at(r.pos, "data count section required"));
    }
    Ok(m)
}

/// Position of each section id in the required order (custom sections are
/// handled separately). The data count section (12) sits between element
/// and code.
fn section_rank(id: u8) -> Option<u8> {
    Some(match id {
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 5,
        6 => 6,
        7 => 7,
        8 => 8,
        9 => 9,
        12 => 10,
        10 => 11,
        11 => 12,
        _ => return None,
    })
}

/// A cursor over the module bytes, bounded by `limit` (the end of the
/// enclosing section or function body). Offsets reported in errors are
/// always absolute module offsets.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    limit: usize,
    /// True for a reader bounded to a section or function body.
    nested: bool,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader {
            bytes,
            pos: 0,
            limit: bytes.len(),
            nested: false,
        }
    }

    /// A reader over the next `len` bytes (caller checked `len <= remaining`).
    fn sub(&self, len: usize) -> Reader<'a> {
        Reader {
            bytes: self.bytes,
            pos: self.pos,
            limit: self.pos + len,
            nested: true,
        }
    }

    fn eof(&self) -> bool {
        self.pos >= self.limit
    }

    fn remaining(&self) -> usize {
        self.limit - self.pos
    }

    fn err_at(&self, offset: usize, msg: &'static str) -> Error {
        Error::Malformed { offset, msg }
    }

    fn err(&self, msg: &'static str) -> Error {
        self.err_at(self.pos, msg)
    }

    fn unsupported(&self, offset: usize, msg: &'static str) -> Error {
        Error::Unsupported { offset, msg }
    }

    fn eof_err(&self) -> Error {
        if self.nested {
            self.err("unexpected end of section or function")
        } else {
            self.err("unexpected end")
        }
    }

    fn byte(&mut self) -> Result<u8, Error> {
        match self.bytes.get(self.pos) {
            Some(&b) if self.pos < self.limit => {
                self.pos += 1;
                Ok(b)
            }
            _ => Err(self.eof_err()),
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if n > self.remaining() {
            return Err(self.eof_err());
        }
        let s = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn magic(&mut self) -> Result<(), Error> {
        if self.bytes.len() < 4 || &self.bytes[..4] != b"\0asm" {
            return Err(self.err_at(0, "magic header not detected"));
        }
        if self.bytes.len() < 8 || self.bytes[4..8] != [1, 0, 0, 0] {
            return Err(self.err_at(4, "unknown binary version"));
        }
        self.pos = 8;
        Ok(())
    }

    // ---- integers ---------------------------------------------------------

    /// Unsigned LEB128 of at most `bits` bits.
    fn uleb(&mut self, bits: u32) -> Result<u64, Error> {
        let start = self.pos;
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            let remaining = bits - shift;
            if remaining < 7 && (b & 0x7F) as u32 >= (1 << remaining) {
                return Err(self.err_at(self.pos - 1, "integer too large"));
            }
            result |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= bits {
                return Err(self.err_at(start, "integer representation too long"));
            }
        }
    }

    /// Signed LEB128 of at most `bits` bits.
    fn sleb(&mut self, bits: u32) -> Result<i64, Error> {
        let start = self.pos;
        let mut result = 0i64;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            let remaining = bits - shift;
            if remaining < 7 && b & 0x80 == 0 {
                // The bits above the value's top bit must all equal it.
                let mask = ((-1i32 << (remaining - 1)) & 0x7F) as u8;
                if b & mask != 0 && b & mask != mask {
                    return Err(self.err_at(self.pos - 1, "integer too large"));
                }
            }
            result |= ((b & 0x7F) as i64) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                if shift < 64 && b & 0x40 != 0 {
                    result |= -1i64 << shift;
                }
                return Ok(result);
            }
            if shift >= bits {
                return Err(self.err_at(start, "integer representation too long"));
            }
        }
    }

    fn u32(&mut self) -> Result<u32, Error> {
        Ok(self.uleb(32)? as u32)
    }

    fn i32(&mut self) -> Result<i32, Error> {
        Ok(self.sleb(32)? as i32)
    }

    fn i64(&mut self) -> Result<i64, Error> {
        self.sleb(64)
    }

    fn s33(&mut self) -> Result<i64, Error> {
        self.sleb(33)
    }

    fn f32_bits(&mut self) -> Result<u32, Error> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn f64_bits(&mut self) -> Result<u64, Error> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// The memory index byte of MVP memory instructions.
    fn zero_byte(&mut self) -> Result<(), Error> {
        let off = self.pos;
        if self.byte()? != 0 {
            return Err(self.err_at(off, "zero byte expected"));
        }
        Ok(())
    }

    // ---- vectors and names ----------------------------------------------

    /// A vector: a count followed by that many `f` items. The capacity hint
    /// is bounded by the remaining input so a lying count cannot force a
    /// huge allocation.
    fn vec<T>(
        &mut self,
        mut f: impl FnMut(&mut Reader<'a>) -> Result<T, Error>,
    ) -> Result<Vec<T>, Error> {
        let count = self.u32()? as usize;
        let mut v = Vec::with_capacity(count.min(self.remaining()));
        for _ in 0..count {
            v.push(f(self)?);
        }
        Ok(v)
    }

    fn name(&mut self) -> Result<String, Error> {
        let len = self.u32()? as usize;
        let off = self.pos;
        let bytes = self.take(len)?;
        match core::str::from_utf8(bytes) {
            Ok(s) => Ok(String::from(s)),
            Err(_) => Err(self.err_at(off, "malformed UTF-8 encoding")),
        }
    }

    // ---- types ------------------------------------------------------------

    fn val_type(&mut self) -> Result<ValType, Error> {
        let off = self.pos;
        let b = self.byte()?;
        val_type_from_byte(b).ok_or_else(|| match b {
            0x7B => self.unsupported(off, "SIMD (v128) is not supported"),
            _ => self.err_at(off, "malformed value type"),
        })
    }

    fn ref_type(&mut self) -> Result<RefType, Error> {
        let off = self.pos;
        match self.byte()? {
            0x70 => Ok(RefType::FuncRef),
            0x6F => Ok(RefType::ExternRef),
            _ => Err(self.err_at(off, "malformed reference type")),
        }
    }

    fn func_type(&mut self) -> Result<FuncType, Error> {
        let off = self.pos;
        if self.byte()? != 0x60 {
            return Err(self.err_at(off, "malformed function type"));
        }
        let params = self.vec(|s| s.val_type())?;
        let results = self.vec(|s| s.val_type())?;
        Ok(FuncType { params, results })
    }

    fn limits(&mut self) -> Result<ResizableLimits, Error> {
        let off = self.pos;
        match self.byte()? {
            0x00 => Ok(ResizableLimits {
                min: self.u32()?,
                max: None,
            }),
            0x01 => {
                let min = self.u32()?;
                let max = self.u32()?;
                Ok(ResizableLimits {
                    min,
                    max: Some(max),
                })
            }
            0x02 | 0x03 => Err(self.unsupported(off, "shared memory (threads) is not supported")),
            _ => Err(self.err_at(off, "malformed limits flags")),
        }
    }

    fn table_type(&mut self) -> Result<TableType, Error> {
        let elem = self.ref_type()?;
        let limits = self.limits()?;
        Ok(TableType { elem, limits })
    }

    fn mem_type(&mut self) -> Result<MemType, Error> {
        Ok(MemType {
            limits: self.limits()?,
        })
    }

    fn global_type(&mut self) -> Result<GlobalType, Error> {
        let ty = self.val_type()?;
        let off = self.pos;
        let mutable = match self.byte()? {
            0 => false,
            1 => true,
            _ => return Err(self.err_at(off, "malformed mutability")),
        };
        Ok(GlobalType { ty, mutable })
    }

    fn block_type(&mut self) -> Result<BlockType, Error> {
        let off = self.pos;
        match self.bytes.get(self.pos) {
            Some(0x40) if self.pos < self.limit => {
                self.pos += 1;
                Ok(BlockType::Empty)
            }
            Some(&b) if self.pos < self.limit && val_type_from_byte(b).is_some() => {
                self.pos += 1;
                Ok(BlockType::Val(
                    val_type_from_byte(b).unwrap_or(ValType::I32),
                ))
            }
            Some(0x7B) if self.pos < self.limit => {
                Err(self.unsupported(off, "SIMD (v128) is not supported"))
            }
            _ => {
                let idx = self.s33()?;
                if idx < 0 {
                    return Err(self.err_at(off, "malformed block type"));
                }
                Ok(BlockType::Type(idx as u32))
            }
        }
    }

    // ---- sections ---------------------------------------------------------

    fn import(&mut self) -> Result<Import, Error> {
        let module = self.name()?;
        let name = self.name()?;
        let off = self.pos;
        let desc = match self.byte()? {
            0x00 => ImportDesc::Func(self.u32()?),
            0x01 => ImportDesc::Table(self.table_type()?),
            0x02 => ImportDesc::Memory(self.mem_type()?),
            0x03 => ImportDesc::Global(self.global_type()?),
            _ => return Err(self.err_at(off, "malformed import kind")),
        };
        Ok(Import { module, name, desc })
    }

    fn export(&mut self) -> Result<Export, Error> {
        let name = self.name()?;
        let off = self.pos;
        let kind = self.byte()?;
        let idx = self.u32()?;
        let desc = match kind {
            0x00 => ExportDesc::Func(idx),
            0x01 => ExportDesc::Table(idx),
            0x02 => ExportDesc::Memory(idx),
            0x03 => ExportDesc::Global(idx),
            _ => return Err(self.err_at(off, "malformed export kind")),
        };
        Ok(Export { name, desc })
    }

    fn global(&mut self) -> Result<Global, Error> {
        let ty = self.global_type()?;
        let init = self.const_expr()?;
        Ok(Global { ty, init })
    }

    fn elem_kind(&mut self) -> Result<RefType, Error> {
        let off = self.pos;
        match self.byte()? {
            0x00 => Ok(RefType::FuncRef),
            _ => Err(self.err_at(off, "malformed element kind")),
        }
    }

    fn element(&mut self) -> Result<Element, Error> {
        let off = self.pos;
        let flags = self.u32()?;
        Ok(match flags {
            0 => {
                let offset = self.const_expr()?;
                let items = ElemItems::Funcs(self.vec(|s| s.u32())?);
                Element {
                    ty: RefType::FuncRef,
                    items,
                    mode: ElemMode::Active { table: 0, offset },
                }
            }
            1 => {
                let ty = self.elem_kind()?;
                let items = ElemItems::Funcs(self.vec(|s| s.u32())?);
                Element {
                    ty,
                    items,
                    mode: ElemMode::Passive,
                }
            }
            2 => {
                let table = self.u32()?;
                let offset = self.const_expr()?;
                let ty = self.elem_kind()?;
                let items = ElemItems::Funcs(self.vec(|s| s.u32())?);
                Element {
                    ty,
                    items,
                    mode: ElemMode::Active { table, offset },
                }
            }
            3 => {
                let ty = self.elem_kind()?;
                let items = ElemItems::Funcs(self.vec(|s| s.u32())?);
                Element {
                    ty,
                    items,
                    mode: ElemMode::Declarative,
                }
            }
            4 => {
                let offset = self.const_expr()?;
                let items = ElemItems::Exprs(self.vec(|s| s.const_expr())?);
                Element {
                    ty: RefType::FuncRef,
                    items,
                    mode: ElemMode::Active { table: 0, offset },
                }
            }
            5 => {
                let ty = self.ref_type()?;
                let items = ElemItems::Exprs(self.vec(|s| s.const_expr())?);
                Element {
                    ty,
                    items,
                    mode: ElemMode::Passive,
                }
            }
            6 => {
                let table = self.u32()?;
                let offset = self.const_expr()?;
                let ty = self.ref_type()?;
                let items = ElemItems::Exprs(self.vec(|s| s.const_expr())?);
                Element {
                    ty,
                    items,
                    mode: ElemMode::Active { table, offset },
                }
            }
            7 => {
                let ty = self.ref_type()?;
                let items = ElemItems::Exprs(self.vec(|s| s.const_expr())?);
                Element {
                    ty,
                    items,
                    mode: ElemMode::Declarative,
                }
            }
            _ => return Err(self.err_at(off, "malformed elements segment kind")),
        })
    }

    fn data(&mut self) -> Result<Data, Error> {
        let off = self.pos;
        let flags = self.u32()?;
        let mode = match flags {
            0 => DataMode::Active {
                memory: 0,
                offset: self.const_expr()?,
            },
            1 => DataMode::Passive,
            2 => {
                let memory = self.u32()?;
                let offset = self.const_expr()?;
                DataMode::Active { memory, offset }
            }
            _ => return Err(self.err_at(off, "malformed data segment kind")),
        };
        let len = self.u32()? as usize;
        let bytes = self.take(len)?.to_vec();
        Ok(Data { mode, bytes })
    }

    // ---- code -------------------------------------------------------------

    fn func_body(&mut self, uses_data_count: &mut bool) -> Result<FuncBody, Error> {
        let size = self.u32()? as usize;
        let start = self.pos;
        if size > self.remaining() {
            return Err(self.err_at(start, "length out of bounds"));
        }
        let mut b = self.sub(size);

        // Locals: a vector of (count, type) runs, expanded here.
        let runs = b.u32()?;
        let mut total: u32 = 0;
        let mut locals = Vec::new();
        for _ in 0..runs {
            let off = b.pos;
            let n = b.u32()?;
            let ty = b.val_type()?;
            total = match total.checked_add(n) {
                Some(t) if t <= MAX_LOCALS => t,
                _ => return Err(b.err_at(off, "too many locals")),
            };
            locals.extend(core::iter::repeat_n(ty, n as usize));
        }

        let mut br_tables = Vec::new();
        let mut decoder = InstrDecoder::new(&mut b);
        let instrs = decoder.decode(&mut br_tables)?;
        if decoder.uses_data_count {
            *uses_data_count = true;
        }
        if !b.eof() {
            return Err(b.err("operators remaining after end of function"));
        }
        self.pos = b.limit;
        Ok(FuncBody {
            locals,
            instrs,
            br_tables,
        })
    }

    /// A constant expression: instructions up to and excluding the
    /// terminating `end`. Any instruction decodes here; the validator
    /// rejects non-constant ones.
    fn const_expr(&mut self) -> Result<ConstExpr, Error> {
        let off = self.pos;
        let mut br_tables = Vec::new();
        let mut instrs = InstrDecoder::new(self).decode(&mut br_tables)?;
        if !br_tables.is_empty() {
            return Err(Error::Invalid {
                msg: "constant expression required",
                func: None,
                instr: None,
            }
            .with_offset(off));
        }
        instrs.pop(); // the trailing `end`
        Ok(ConstExpr { instrs })
    }
}

impl Error {
    /// `br_table` inside a constant expression is reported as malformed by
    /// the decoder (it has no way to represent it in a `ConstExpr`).
    fn with_offset(self, offset: usize) -> Error {
        match self {
            Error::Invalid { msg, .. } => Error::Malformed { offset, msg },
            e => e,
        }
    }
}

fn val_type_from_byte(b: u8) -> Option<ValType> {
    Some(match b {
        0x7F => ValType::I32,
        0x7E => ValType::I64,
        0x7D => ValType::F32,
        0x7C => ValType::F64,
        0x70 => ValType::FuncRef,
        0x6F => ValType::ExternRef,
        _ => return None,
    })
}

/// An open structured block while decoding an instruction sequence.
struct Frame {
    /// Index of the opening instruction (`Block`/`Loop`/`If`).
    opener: u32,
    /// Index of the `Else`, once seen.
    else_: Option<u32>,
    kind: FrameKind,
}

#[derive(PartialEq, Eq)]
enum FrameKind {
    Func,
    Block,
    Loop,
    If,
}

/// Decodes one instruction sequence (a function body or a constant
/// expression) up to and including the `end` that closes it, resolving
/// the control-flow structure as it goes.
struct InstrDecoder<'r, 'a> {
    r: &'r mut Reader<'a>,
    uses_data_count: bool,
}

impl<'r, 'a> InstrDecoder<'r, 'a> {
    fn new(r: &'r mut Reader<'a>) -> Self {
        InstrDecoder {
            r,
            uses_data_count: false,
        }
    }

    fn decode(&mut self, br_tables: &mut Vec<BrTable>) -> Result<Vec<Instr>, Error> {
        let mut instrs: Vec<Instr> = Vec::new();
        let mut frames: Vec<Frame> = Vec::new();
        frames.push(Frame {
            opener: 0,
            else_: None,
            kind: FrameKind::Func,
        });

        loop {
            let off = self.r.pos;
            let idx = u32::try_from(instrs.len()).map_err(|_| Error::Limit {
                msg: "function body too large",
            })?;
            let op = self.r.byte()?;
            let instr = match op {
                0x00 => Instr::Unreachable,
                0x01 => Instr::Nop,
                0x02 => {
                    let ty = self.r.block_type()?;
                    frames.push(Frame {
                        opener: idx,
                        else_: None,
                        kind: FrameKind::Block,
                    });
                    Instr::Block { ty, end: 0 }
                }
                0x03 => {
                    let ty = self.r.block_type()?;
                    frames.push(Frame {
                        opener: idx,
                        else_: None,
                        kind: FrameKind::Loop,
                    });
                    Instr::Loop { ty, end: 0 }
                }
                0x04 => {
                    let ty = self.r.block_type()?;
                    frames.push(Frame {
                        opener: idx,
                        else_: None,
                        kind: FrameKind::If,
                    });
                    Instr::If {
                        ty,
                        else_: 0,
                        end: 0,
                    }
                }
                0x05 => {
                    let frame = match frames.last_mut() {
                        Some(f) if f.kind == FrameKind::If && f.else_.is_none() => f,
                        _ => return Err(self.r.err_at(off, "unexpected else")),
                    };
                    frame.else_ = Some(idx);
                    Instr::Else { end: 0 }
                }
                0x0B => {
                    // `frames` is never empty: the Func frame is popped only
                    // on the return path below.
                    let frame = match frames.pop() {
                        Some(f) => f,
                        None => return Err(self.r.err_at(off, "unexpected end")),
                    };
                    instrs.push(Instr::End);
                    if frame.kind == FrameKind::Func {
                        return Ok(instrs);
                    }
                    patch_end(&mut instrs, &frame, idx);
                    continue;
                }
                0x0C => Instr::Br(self.r.u32()?),
                0x0D => Instr::BrIf(self.r.u32()?),
                0x0E => {
                    let targets = self.r.vec(|s| s.u32())?;
                    let default = self.r.u32()?;
                    let table = u32::try_from(br_tables.len()).map_err(|_| Error::Limit {
                        msg: "too many br_table instructions",
                    })?;
                    br_tables.push(BrTable { targets, default });
                    Instr::BrTable(table)
                }
                0x0F => Instr::Return,
                0x10 => Instr::Call(self.r.u32()?),
                0x11 => {
                    let ty = self.r.u32()?;
                    let table = self.r.u32()?;
                    Instr::CallIndirect { ty, table }
                }
                0x12 | 0x13 => return Err(self.r.unsupported(off, "tail calls are not supported")),
                0x06..=0x0A | 0x18 | 0x19 => {
                    return Err(self
                        .r
                        .unsupported(off, "exception handling is not supported"))
                }
                0x14 | 0x15 => {
                    return Err(self
                        .r
                        .unsupported(off, "typed function references are not supported"))
                }

                0x1A => Instr::Drop,
                0x1B => Instr::Select,
                0x1C => {
                    let types = self.r.vec(|s| s.val_type())?;
                    match types.as_slice() {
                        [t] => Instr::SelectT(*t),
                        _ => return Err(self.r.err_at(off, "invalid result arity")),
                    }
                }

                0x20 => Instr::LocalGet(self.r.u32()?),
                0x21 => Instr::LocalSet(self.r.u32()?),
                0x22 => Instr::LocalTee(self.r.u32()?),
                0x23 => Instr::GlobalGet(self.r.u32()?),
                0x24 => Instr::GlobalSet(self.r.u32()?),
                0x25 => Instr::TableGet(self.r.u32()?),
                0x26 => Instr::TableSet(self.r.u32()?),

                0x28 => Instr::I32Load(self.memarg()?),
                0x29 => Instr::I64Load(self.memarg()?),
                0x2A => Instr::F32Load(self.memarg()?),
                0x2B => Instr::F64Load(self.memarg()?),
                0x2C => Instr::I32Load8S(self.memarg()?),
                0x2D => Instr::I32Load8U(self.memarg()?),
                0x2E => Instr::I32Load16S(self.memarg()?),
                0x2F => Instr::I32Load16U(self.memarg()?),
                0x30 => Instr::I64Load8S(self.memarg()?),
                0x31 => Instr::I64Load8U(self.memarg()?),
                0x32 => Instr::I64Load16S(self.memarg()?),
                0x33 => Instr::I64Load16U(self.memarg()?),
                0x34 => Instr::I64Load32S(self.memarg()?),
                0x35 => Instr::I64Load32U(self.memarg()?),
                0x36 => Instr::I32Store(self.memarg()?),
                0x37 => Instr::I64Store(self.memarg()?),
                0x38 => Instr::F32Store(self.memarg()?),
                0x39 => Instr::F64Store(self.memarg()?),
                0x3A => Instr::I32Store8(self.memarg()?),
                0x3B => Instr::I32Store16(self.memarg()?),
                0x3C => Instr::I64Store8(self.memarg()?),
                0x3D => Instr::I64Store16(self.memarg()?),
                0x3E => Instr::I64Store32(self.memarg()?),
                0x3F => {
                    self.r.zero_byte()?;
                    Instr::MemorySize
                }
                0x40 => {
                    self.r.zero_byte()?;
                    Instr::MemoryGrow
                }

                0x41 => Instr::I32Const(self.r.i32()?),
                0x42 => Instr::I64Const(self.r.i64()?),
                0x43 => Instr::F32Const(self.r.f32_bits()?),
                0x44 => Instr::F64Const(self.r.f64_bits()?),

                0x45 => Instr::I32Eqz,
                0x46 => Instr::I32Eq,
                0x47 => Instr::I32Ne,
                0x48 => Instr::I32LtS,
                0x49 => Instr::I32LtU,
                0x4A => Instr::I32GtS,
                0x4B => Instr::I32GtU,
                0x4C => Instr::I32LeS,
                0x4D => Instr::I32LeU,
                0x4E => Instr::I32GeS,
                0x4F => Instr::I32GeU,

                0x50 => Instr::I64Eqz,
                0x51 => Instr::I64Eq,
                0x52 => Instr::I64Ne,
                0x53 => Instr::I64LtS,
                0x54 => Instr::I64LtU,
                0x55 => Instr::I64GtS,
                0x56 => Instr::I64GtU,
                0x57 => Instr::I64LeS,
                0x58 => Instr::I64LeU,
                0x59 => Instr::I64GeS,
                0x5A => Instr::I64GeU,

                0x5B => Instr::F32Eq,
                0x5C => Instr::F32Ne,
                0x5D => Instr::F32Lt,
                0x5E => Instr::F32Gt,
                0x5F => Instr::F32Le,
                0x60 => Instr::F32Ge,

                0x61 => Instr::F64Eq,
                0x62 => Instr::F64Ne,
                0x63 => Instr::F64Lt,
                0x64 => Instr::F64Gt,
                0x65 => Instr::F64Le,
                0x66 => Instr::F64Ge,

                0x67 => Instr::I32Clz,
                0x68 => Instr::I32Ctz,
                0x69 => Instr::I32Popcnt,
                0x6A => Instr::I32Add,
                0x6B => Instr::I32Sub,
                0x6C => Instr::I32Mul,
                0x6D => Instr::I32DivS,
                0x6E => Instr::I32DivU,
                0x6F => Instr::I32RemS,
                0x70 => Instr::I32RemU,
                0x71 => Instr::I32And,
                0x72 => Instr::I32Or,
                0x73 => Instr::I32Xor,
                0x74 => Instr::I32Shl,
                0x75 => Instr::I32ShrS,
                0x76 => Instr::I32ShrU,
                0x77 => Instr::I32Rotl,
                0x78 => Instr::I32Rotr,

                0x79 => Instr::I64Clz,
                0x7A => Instr::I64Ctz,
                0x7B => Instr::I64Popcnt,
                0x7C => Instr::I64Add,
                0x7D => Instr::I64Sub,
                0x7E => Instr::I64Mul,
                0x7F => Instr::I64DivS,
                0x80 => Instr::I64DivU,
                0x81 => Instr::I64RemS,
                0x82 => Instr::I64RemU,
                0x83 => Instr::I64And,
                0x84 => Instr::I64Or,
                0x85 => Instr::I64Xor,
                0x86 => Instr::I64Shl,
                0x87 => Instr::I64ShrS,
                0x88 => Instr::I64ShrU,
                0x89 => Instr::I64Rotl,
                0x8A => Instr::I64Rotr,

                0x8B => Instr::F32Abs,
                0x8C => Instr::F32Neg,
                0x8D => Instr::F32Ceil,
                0x8E => Instr::F32Floor,
                0x8F => Instr::F32Trunc,
                0x90 => Instr::F32Nearest,
                0x91 => Instr::F32Sqrt,
                0x92 => Instr::F32Add,
                0x93 => Instr::F32Sub,
                0x94 => Instr::F32Mul,
                0x95 => Instr::F32Div,
                0x96 => Instr::F32Min,
                0x97 => Instr::F32Max,
                0x98 => Instr::F32Copysign,

                0x99 => Instr::F64Abs,
                0x9A => Instr::F64Neg,
                0x9B => Instr::F64Ceil,
                0x9C => Instr::F64Floor,
                0x9D => Instr::F64Trunc,
                0x9E => Instr::F64Nearest,
                0x9F => Instr::F64Sqrt,
                0xA0 => Instr::F64Add,
                0xA1 => Instr::F64Sub,
                0xA2 => Instr::F64Mul,
                0xA3 => Instr::F64Div,
                0xA4 => Instr::F64Min,
                0xA5 => Instr::F64Max,
                0xA6 => Instr::F64Copysign,

                0xA7 => Instr::I32WrapI64,
                0xA8 => Instr::I32TruncF32S,
                0xA9 => Instr::I32TruncF32U,
                0xAA => Instr::I32TruncF64S,
                0xAB => Instr::I32TruncF64U,
                0xAC => Instr::I64ExtendI32S,
                0xAD => Instr::I64ExtendI32U,
                0xAE => Instr::I64TruncF32S,
                0xAF => Instr::I64TruncF32U,
                0xB0 => Instr::I64TruncF64S,
                0xB1 => Instr::I64TruncF64U,
                0xB2 => Instr::F32ConvertI32S,
                0xB3 => Instr::F32ConvertI32U,
                0xB4 => Instr::F32ConvertI64S,
                0xB5 => Instr::F32ConvertI64U,
                0xB6 => Instr::F32DemoteF64,
                0xB7 => Instr::F64ConvertI32S,
                0xB8 => Instr::F64ConvertI32U,
                0xB9 => Instr::F64ConvertI64S,
                0xBA => Instr::F64ConvertI64U,
                0xBB => Instr::F64PromoteF32,
                0xBC => Instr::I32ReinterpretF32,
                0xBD => Instr::I64ReinterpretF64,
                0xBE => Instr::F32ReinterpretI32,
                0xBF => Instr::F64ReinterpretI64,

                0xC0 => Instr::I32Extend8S,
                0xC1 => Instr::I32Extend16S,
                0xC2 => Instr::I64Extend8S,
                0xC3 => Instr::I64Extend16S,
                0xC4 => Instr::I64Extend32S,

                0xD0 => Instr::RefNull(self.r.ref_type()?),
                0xD1 => Instr::RefIsNull,
                0xD2 => Instr::RefFunc(self.r.u32()?),

                0xFC => self.prefixed_fc(off)?,
                0xFD => return Err(self.r.unsupported(off, "SIMD is not supported")),
                0xFE => {
                    return Err(self
                        .r
                        .unsupported(off, "threads (atomics) are not supported"))
                }
                _ => return Err(self.r.err_at(off, "illegal opcode")),
            };
            instrs.push(instr);
        }
    }

    fn memarg(&mut self) -> Result<MemArg, Error> {
        let off = self.r.pos;
        let align = self.r.u32()?;
        // The alignment exponent must fit a 32-bit power of two; larger
        // values (and the multi-memory flag bit) are malformed, not merely
        // over-aligned (align.wast "malformed memop flags").
        if align >= 32 {
            return Err(self.r.err_at(off, "malformed memop flags"));
        }
        let offset = self.r.u32()?;
        Ok(MemArg { align, offset })
    }

    fn prefixed_fc(&mut self, off: usize) -> Result<Instr, Error> {
        Ok(match self.r.u32()? {
            0 => Instr::I32TruncSatF32S,
            1 => Instr::I32TruncSatF32U,
            2 => Instr::I32TruncSatF64S,
            3 => Instr::I32TruncSatF64U,
            4 => Instr::I64TruncSatF32S,
            5 => Instr::I64TruncSatF32U,
            6 => Instr::I64TruncSatF64S,
            7 => Instr::I64TruncSatF64U,
            8 => {
                let data = self.r.u32()?;
                self.r.zero_byte()?;
                self.uses_data_count = true;
                Instr::MemoryInit(data)
            }
            9 => {
                self.uses_data_count = true;
                Instr::DataDrop(self.r.u32()?)
            }
            10 => {
                self.r.zero_byte()?;
                self.r.zero_byte()?;
                Instr::MemoryCopy
            }
            11 => {
                self.r.zero_byte()?;
                Instr::MemoryFill
            }
            12 => {
                let elem = self.r.u32()?;
                let table = self.r.u32()?;
                Instr::TableInit { elem, table }
            }
            13 => Instr::ElemDrop(self.r.u32()?),
            14 => {
                let dst = self.r.u32()?;
                let src = self.r.u32()?;
                Instr::TableCopy { dst, src }
            }
            15 => Instr::TableGrow(self.r.u32()?),
            16 => Instr::TableSize(self.r.u32()?),
            17 => Instr::TableFill(self.r.u32()?),
            _ => return Err(self.r.err_at(off, "illegal opcode")),
        })
    }
}

/// Write the resolved `end` (and `else`) indices back into the opener of a
/// block that just closed at `end_idx`.
fn patch_end(instrs: &mut [Instr], frame: &Frame, end_idx: u32) {
    if let Some(opener) = instrs.get_mut(frame.opener as usize) {
        match opener {
            Instr::Block { end, .. } | Instr::Loop { end, .. } => *end = end_idx,
            Instr::If { else_, end, .. } => {
                *else_ = frame.else_.unwrap_or(end_idx);
                *end = end_idx;
            }
            _ => {}
        }
    }
    if let Some(else_idx) = frame.else_ {
        if let Some(Instr::Else { end }) = instrs.get_mut(else_idx as usize) {
            *end = end_idx;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leb_u32(bytes: &[u8]) -> Result<u32, Error> {
        Reader::new(bytes).u32()
    }

    fn leb_i32(bytes: &[u8]) -> Result<i32, Error> {
        Reader::new(bytes).i32()
    }

    fn leb_i64(bytes: &[u8]) -> Result<i64, Error> {
        Reader::new(bytes).i64()
    }

    #[test]
    fn uleb() {
        assert_eq!(leb_u32(&[0]).unwrap(), 0);
        assert_eq!(leb_u32(&[0x7F]).unwrap(), 127);
        assert_eq!(leb_u32(&[0x80, 0x01]).unwrap(), 128);
        assert_eq!(leb_u32(&[0xE5, 0x8E, 0x26]).unwrap(), 624485);
        assert_eq!(leb_u32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]).unwrap(), u32::MAX);
        // Redundant but valid encodings.
        assert_eq!(leb_u32(&[0x80, 0x80, 0x80, 0x80, 0x00]).unwrap(), 0);
        assert_eq!(
            leb_u32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x1F]),
            Err(Error::Malformed {
                offset: 4,
                msg: "integer too large"
            })
        );
        assert_eq!(
            leb_u32(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]),
            Err(Error::Malformed {
                offset: 0,
                msg: "integer representation too long"
            })
        );
        assert_eq!(
            leb_u32(&[0x80]),
            Err(Error::Malformed {
                offset: 1,
                msg: "unexpected end"
            })
        );
    }

    #[test]
    fn sleb() {
        assert_eq!(leb_i32(&[0]).unwrap(), 0);
        assert_eq!(leb_i32(&[0x7F]).unwrap(), -1);
        assert_eq!(leb_i32(&[0x3F]).unwrap(), 63);
        assert_eq!(leb_i32(&[0x40]).unwrap(), -64);
        assert_eq!(leb_i32(&[0xC0, 0x00]).unwrap(), 64);
        assert_eq!(leb_i32(&[0xC0, 0xBB, 0x78]).unwrap(), -123456);
        assert_eq!(leb_i32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x07]).unwrap(), i32::MAX);
        assert_eq!(leb_i32(&[0x80, 0x80, 0x80, 0x80, 0x78]).unwrap(), i32::MIN);
        assert_eq!(leb_i32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F]).unwrap(), -1);
        // Sign bits of the last byte must agree.
        assert!(leb_i32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]).is_err());
        assert!(leb_i32(&[0x80, 0x80, 0x80, 0x80, 0x70]).is_err());
        assert!(leb_i32(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]).is_err());

        assert_eq!(leb_i64(&[0x7F]).unwrap(), -1);
        assert_eq!(
            leb_i64(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]).unwrap(),
            i64::MAX
        );
        assert_eq!(
            leb_i64(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x7F]).unwrap(),
            i64::MIN
        );
        assert!(leb_i64(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]).is_err());
        assert!(
            leb_i64(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00]).is_err()
        );
    }

    #[test]
    fn s33_block_types() {
        let mut r = Reader::new(&[0x40]);
        assert_eq!(r.block_type().unwrap(), BlockType::Empty);
        let mut r = Reader::new(&[0x7F]);
        assert_eq!(r.block_type().unwrap(), BlockType::Val(ValType::I32));
        let mut r = Reader::new(&[0x05]);
        assert_eq!(r.block_type().unwrap(), BlockType::Type(5));
        let mut r = Reader::new(&[0x80, 0x01]);
        assert_eq!(r.block_type().unwrap(), BlockType::Type(128));
        // A multi-byte encoding of a negative number is not a type index.
        let mut r = Reader::new(&[0xFF, 0x7F]);
        assert!(r.block_type().is_err());
    }

    #[test]
    fn header() {
        assert_eq!(decode(b"\0asm\x01\0\0\0").unwrap(), Module::default());
        assert_eq!(
            decode(b"\0wasm").unwrap_err(),
            Error::Malformed {
                offset: 0,
                msg: "magic header not detected"
            }
        );
        assert_eq!(
            decode(b"\0asm\x02\0\0\0").unwrap_err(),
            Error::Malformed {
                offset: 4,
                msg: "unknown binary version"
            }
        );
        assert_eq!(
            decode(b"").unwrap_err(),
            Error::Malformed {
                offset: 0,
                msg: "magic header not detected"
            }
        );
    }
}
