//! Rust's saved view, `view.rsnap`: a MAC'd binary snapshot of the replayed
//! board at one journal position. Python keeps `view.json`; neither build
//! reads the other's file, so alternating builds never invalidate each other.
//!
//! Layout:
//!   magic "COLLABRS" | format u32 | reducer [32] | position | 9 x (offset u64, len u64) | mac [32] | sections
//! The MAC is keyed BLAKE3 (key derived from the shared `view.key`) over every
//! byte of the file except the MAC itself. Sections 0..8 are the state tables
//! in `state::TABLES` order and are decoded only when first used; section 8 is
//! the request index, section 9 the post -> recipients index and section 10
//! the summary-post index.

use crate::journal::Position;
use crate::json::{Map, Value};
use crate::state::{Entry, Raw, State, Table, T};
use std::collections::HashMap;
use std::rc::Rc;

include!(concat!(env!("OUT_DIR"), "/reducer.rs"));

pub const MAGIC: &[u8; 8] = b"COLLABRS";
pub const FORMAT: u32 = 5;
const SECTIONS: usize = 11;

/// A saved view's bytes: the file mapped read-only, or a copy in memory.
///
/// Views are replaced by rename and never written in place, so a mapping
/// keeps reading the file it opened.
pub enum Bytes {
    Owned(Vec<u8>),
    Mapped(*const u8, usize),
}

impl std::ops::Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Bytes::Owned(v) => v,
            Bytes::Mapped(p, n) => unsafe { std::slice::from_raw_parts(*p, *n) },
        }
    }
}

impl Drop for Bytes {
    fn drop(&mut self) {
        if let Bytes::Mapped(p, n) = *self {
            unsafe { libc::munmap(p as *mut libc::c_void, n) };
        }
    }
}

impl Bytes {
    /// Map a regular file, or read it when mapping is not possible.
    pub fn read(path: &std::path::Path) -> std::io::Result<Bytes> {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        let len = meta.len() as usize;
        if meta.is_file() && len > 0 {
            #[cfg(target_os = "linux")]
            let flags = libc::MAP_PRIVATE | libc::MAP_POPULATE;
            #[cfg(not(target_os = "linux"))]
            let flags = libc::MAP_PRIVATE;
            let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, flags, file.as_raw_fd(), 0) };
            if p != libc::MAP_FAILED {
                return Ok(Bytes::Mapped(p as *const u8, len));
            }
        }
        let mut data = Vec::with_capacity(len);
        std::io::Read::read_to_end(&mut &file, &mut data)?;
        Ok(Bytes::Owned(data))
    }
}

/// The request index: request ID to the `[seq, start, end]` of its record.
///
/// A loaded view keeps the saved index as bytes, sorted by ID behind an offset
/// table, and looks IDs up by binary search; entries added since loading live
/// in a hash map. Nothing is decoded unless the view is saved again.
#[derive(Clone, Default)]
pub struct Requests {
    base: Option<Raw>,
    base_len: usize,
    added: HashMap<String, [u64; 3], crate::pyfmt::FxBuild>,
}

impl Requests {
    fn base_entry(raw: &[u8], n: usize, i: usize) -> Option<(&[u8], [u64; 3])> {
        let at = 4 + 4 * i;
        let off = u32::from_le_bytes(raw.get(at..at + 4)?.try_into().ok()?) as usize;
        let mut r = Reader { b: raw, i: 4 + 4 * n + off };
        let len = r.varint()? as usize;
        let key = r.take(len)?;
        Some((key, [r.varint()?, r.varint()?, r.varint()?]))
    }
    fn base_get(&self, id: &str) -> Option<[u64; 3]> {
        let raw = self.base.as_ref()?.bytes();
        let (mut lo, mut hi) = (0usize, self.base_len);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let (key, v) = Self::base_entry(raw, self.base_len, mid).expect("saved view section passed its MAC but does not decode");
            match key.cmp(id.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(v),
            }
        }
        None
    }
    pub fn get(&self, id: &str) -> Option<[u64; 3]> {
        self.added.get(id).copied().or_else(|| self.base_get(id))
    }
    pub fn contains_key(&self, id: &str) -> bool {
        self.get(id).is_some()
    }
    pub fn insert(&mut self, id: String, at: [u64; 3]) {
        self.added.insert(id, at);
    }
    /// Every entry, in no particular order.
    pub fn entries(&self) -> Vec<(&str, [u64; 3])> {
        let mut out: Vec<(&str, [u64; 3])> = Vec::with_capacity(self.base_len + self.added.len());
        if let Some(raw) = &self.base {
            let raw = raw.bytes();
            for i in 0..self.base_len {
                let (key, v) = Self::base_entry(raw, self.base_len, i).expect("saved view section passed its MAC but does not decode");
                let key = std::str::from_utf8(key).expect("saved request ID is text");
                if !self.added.contains_key(key) {
                    out.push((key, v));
                }
            }
        }
        out.extend(self.added.iter().map(|(k, v)| (k.as_str(), *v)));
        out
    }
}

fn mac_key(view_key: &[u8]) -> [u8; 32] {
    blake3::derive_key("agent-collab rust view.rsnap v1", view_key)
}

// ---------------------------------------------------------------- encoding

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn bytes(out: &mut Vec<u8>, b: &[u8]) {
    varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

pub fn encode_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(0),
        Value::Bool(false) => out.push(1),
        Value::Bool(true) => out.push(2),
        Value::Int(i) => {
            out.push(3);
            varint(out, ((*i << 1) ^ (*i >> 63)) as u64);
        }
        Value::Big(s) => {
            out.push(4);
            bytes(out, s.as_bytes());
        }
        Value::Float(f) => {
            out.push(5);
            out.extend_from_slice(&f.to_le_bytes());
        }
        Value::Str(s) => {
            out.push(6);
            bytes(out, s.as_bytes());
        }
        Value::Arr(a) => {
            out.push(7);
            varint(out, a.len() as u64);
            for item in a {
                encode_value(out, item);
            }
        }
        Value::Obj(m) => {
            out.push(8);
            varint(out, m.len() as u64);
            for (k, item) in m.iter() {
                bytes(out, k.as_bytes());
                encode_value(out, item);
            }
        }
    }
}

/// count, then per entity: key, encoded length, encoding. The lengths let a
/// load index the keys without decoding any entity.
fn encode_table(out: &mut Vec<u8>, table: &Table) {
    varint(out, table.len() as u64);
    let mut scratch = Vec::new();
    for (k, entry) in table.entries() {
        bytes(out, k.as_bytes());
        match entry.raw() {
            Some(raw) => bytes(out, raw),
            None => {
                scratch.clear();
                encode_value(&mut scratch, entry.value());
                bytes(out, &scratch);
            }
        }
    }
}

fn encode_requests(out: &mut Vec<u8>, requests: &Requests) {
    let mut entries = requests.entries();
    entries.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut body = Vec::new();
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (k, [seq, start, end]) in &entries {
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes(&mut body, k.as_bytes());
        varint(&mut body, *seq);
        varint(&mut body, *start);
        varint(&mut body, *end);
    }
    out.extend_from_slice(&body);
}

fn encode_recipients(out: &mut Vec<u8>, index: &crate::state::Recipients) {
    let mut keys: Vec<&String> = index.keys().collect();
    keys.sort();
    varint(out, keys.len() as u64);
    for k in keys {
        bytes(out, k.as_bytes());
        let actors = &index[k];
        varint(out, actors.len() as u64);
        for a in actors {
            bytes(out, a.as_bytes());
        }
    }
}

fn encode_summaries(out: &mut Vec<u8>, index: &crate::state::Summaries) {
    varint(out, index.len() as u64);
    for (k, refs) in index {
        bytes(out, k.as_bytes());
        varint(out, refs.len() as u64);
        for r in refs {
            bytes(out, r.as_bytes());
        }
    }
}

pub fn decode_summaries(b: &[u8]) -> Option<crate::state::Summaries> {
    let mut r = Reader { b, i: 0 };
    let n = r.varint()? as usize;
    let mut out = crate::state::Summaries::with_capacity_and_hasher(n.min(1 << 20), Default::default());
    for _ in 0..n {
        let k = r.str()?;
        let m = r.varint()? as usize;
        let mut refs = Vec::with_capacity(m.min(1 << 16));
        for _ in 0..m {
            refs.push(r.str()?);
        }
        out.insert(k, refs);
    }
    (r.i == b.len()).then_some(out)
}

pub fn decode_recipients(b: &[u8]) -> Option<crate::state::Recipients> {
    let mut r = Reader { b, i: 0 };
    let n = r.varint()? as usize;
    let mut out = crate::state::Recipients::with_capacity_and_hasher(n, Default::default());
    for _ in 0..n {
        let k = r.str()?;
        let m = r.varint()? as usize;
        let mut set = std::collections::BTreeSet::new();
        for _ in 0..m {
            set.insert(r.str()?);
        }
        out.insert(k, set);
    }
    (r.i == b.len()).then_some(out)
}

pub fn encode(view_key: &[u8], position: &Position, state: &mut State, requests: &Requests) -> Vec<u8> {
    let mut sections: Vec<Vec<u8>> = Vec::with_capacity(SECTIONS);
    for t in T::ALL {
        let mut s = Vec::new();
        encode_table(&mut s, state.table(t));
        sections.push(s);
    }
    let mut r = Vec::new();
    encode_requests(&mut r, requests);
    sections.push(r);
    let mut r = Vec::new();
    encode_recipients(&mut r, state.recipients_for_snapshot());
    sections.push(r);
    let mut r = Vec::new();
    encode_summaries(&mut r, state.summaries());
    sections.push(r);

    let mut head = Vec::with_capacity(256);
    head.extend_from_slice(MAGIC);
    head.extend_from_slice(&FORMAT.to_le_bytes());
    head.extend_from_slice(&REDUCER);
    bytes(&mut head, position.stream.as_bytes());
    head.extend_from_slice(&position.seq.to_le_bytes());
    bytes(&mut head, position.sha256.as_bytes());
    head.extend_from_slice(&position.start.to_le_bytes());
    head.extend_from_slice(&position.end.to_le_bytes());
    let table_at = head.len();
    let body_start = table_at + SECTIONS * 16 + 32;
    let mut offset = body_start as u64;
    for s in &sections {
        head.extend_from_slice(&offset.to_le_bytes());
        head.extend_from_slice(&(s.len() as u64).to_le_bytes());
        offset += s.len() as u64;
    }
    let mut out = head;
    out.extend_from_slice(&[0u8; 32]);
    for s in sections {
        out.extend_from_slice(&s);
    }
    let mac_at = table_at + SECTIONS * 16;
    let tag = mac(view_key, &out[..mac_at], &out[mac_at + 32..]);
    out[mac_at..mac_at + 32].copy_from_slice(&tag);
    out
}

/// The view's MAC: a keyed hash of the header and of the hashes of the body's
/// fixed parts. The parts are hashed side by side on large views.
fn mac(view_key: &[u8], head: &[u8], body: &[u8]) -> [u8; 32] {
    const PARTS: usize = 4;
    let size = body.len().div_ceil(PARTS).max(1);
    let parts: Vec<&[u8]> = (0..PARTS).map(|i| &body[(i * size).min(body.len())..((i + 1) * size).min(body.len())]).collect();
    let digests: Vec<blake3::Hash> = if body.len() > (1 << 20) {
        std::thread::scope(|scope| {
            let handles: Vec<_> = parts[1..].iter().map(|p| scope.spawn(move || blake3::hash(p))).collect();
            let mut out = vec![blake3::hash(parts[0])];
            out.extend(handles.into_iter().map(|h| h.join().expect("hash thread")));
            out
        })
    } else {
        parts.iter().map(|p| blake3::hash(p)).collect()
    };
    let mut hasher = blake3::Hasher::new_keyed(&mac_key(view_key));
    hasher.update(head);
    hasher.update(&(body.len() as u64).to_le_bytes());
    for d in &digests {
        hasher.update(d.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

// ---------------------------------------------------------------- decoding

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.i + n > self.b.len() {
            return None;
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.i)?;
        self.i += 1;
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            v |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                return Some(v);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }
    fn str(&mut self) -> Option<String> {
        let n = self.varint()? as usize;
        let b = self.take(n)?;
        std::str::from_utf8(b).ok().map(str::to_string)
    }
    fn value(&mut self, depth: usize) -> Option<Value> {
        if depth > 2000 {
            return None;
        }
        Some(match self.u8()? {
            0 => Value::Null,
            1 => Value::Bool(false),
            2 => Value::Bool(true),
            3 => {
                let z = self.varint()?;
                Value::Int(((z >> 1) as i64) ^ -((z & 1) as i64))
            }
            4 => Value::Big(self.str()?.into()),
            5 => Value::Float(f64::from_le_bytes(self.take(8)?.try_into().ok()?)),
            6 => Value::Str(self.str()?),
            7 => {
                let n = self.varint()? as usize;
                let mut a = Vec::with_capacity(n.min(1 << 16));
                for _ in 0..n {
                    a.push(self.value(depth + 1)?);
                }
                Value::Arr(a)
            }
            8 => {
                let n = self.varint()? as usize;
                let mut m = Map::with_capacity(n.min(1 << 16));
                for _ in 0..n {
                    let k = self.str()?;
                    let v = self.value(depth + 1)?;
                    m.push(k, v);
                }
                Value::Obj(m)
            }
            _ => return None,
        })
    }
}

impl Reader<'_> {
    /// Step over one encoded value without building it.
    fn skip(&mut self, depth: usize) -> Option<()> {
        if depth > 2000 {
            return None;
        }
        match self.u8()? {
            0..=2 => {}
            3 => {
                self.varint()?;
            }
            4 | 6 => {
                let n = self.varint()? as usize;
                self.take(n)?;
            }
            5 => {
                self.take(8)?;
            }
            7 => {
                for _ in 0..self.varint()? {
                    self.skip(depth + 1)?;
                }
            }
            8 => {
                for _ in 0..self.varint()? {
                    let n = self.varint()? as usize;
                    self.take(n)?;
                    self.skip(depth + 1)?;
                }
            }
            _ => return None,
        }
        Some(())
    }
}

/// One top-level field of an encoded object, decoded alone: `Some(None)`
/// when the object has no such key, `None` when the bytes are not an object.
pub fn peek_field(b: &[u8], key: &str) -> Option<Option<Value>> {
    let mut r = Reader { b, i: 0 };
    if r.u8()? != 8 {
        return None;
    }
    let mut found = None;
    for _ in 0..r.varint()? {
        let n = r.varint()? as usize;
        let k = r.take(n)?;
        if k == key.as_bytes() {
            // A later duplicate wins, as when the object is built.
            found = Some(r.value(1)?);
        } else {
            r.skip(1)?;
        }
    }
    Some(found)
}

pub fn decode_table(raw: &Raw) -> Option<Table> {
    let b = raw.bytes();
    let mut r = Reader { b, i: 0 };
    let n = r.varint()? as usize;
    let mut table = Table::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        let k = r.str()?;
        let len = r.varint()? as usize;
        let at = r.i;
        r.take(len)?;
        table.push_entry(k, Entry::from_raw(Raw { data: raw.data.clone(), start: raw.start + at, end: raw.start + at + len }));
    }
    (r.i == b.len()).then_some(table)
}

pub fn decode_value(b: &[u8]) -> Option<Value> {
    let mut r = Reader { b, i: 0 };
    let v = r.value(0)?;
    (r.i == b.len()).then_some(v)
}

/// Check the request index's framing and order once, so lookups can trust it.
fn decode_requests(raw: Raw) -> Option<Requests> {
    let b = raw.bytes();
    let n = u32::from_le_bytes(b.get(..4)?.try_into().ok()?) as usize;
    let body = 4usize.checked_add(n.checked_mul(4)?)?;
    if body > b.len() {
        return None;
    }
    let mut r = Reader { b, i: body };
    let mut previous: Option<&[u8]> = None;
    for i in 0..n {
        let off = u32::from_le_bytes(b[4 + 4 * i..8 + 4 * i].try_into().ok()?) as usize;
        if body + off != r.i {
            return None;
        }
        let len = r.varint()? as usize;
        let key = r.take(len)?;
        std::str::from_utf8(key).ok()?;
        if previous.is_some_and(|p| p >= key) {
            return None;
        }
        previous = Some(key);
        r.varint()?;
        r.varint()?;
        r.varint()?;
    }
    (r.i == b.len()).then_some(Requests { base: Some(raw), base_len: n, added: HashMap::default() })
}

pub struct Loaded {
    pub position: Position,
    pub state: State,
    pub requests: Requests,
}

/// The snapshot in `data`, or None unless it is one this exact build wrote under `view_key`.
pub fn decode(view_key: &[u8], data: Bytes) -> Option<Loaded> {
    let mut r = Reader { b: &data, i: 0 };
    if r.take(8)? != MAGIC || r.u32()? != FORMAT || r.take(32)? != REDUCER {
        return None;
    }
    let stream = r.str()?;
    let seq = r.u64()?;
    let sha256 = r.str()?;
    let start = r.u64()?;
    let end = r.u64()?;
    let mut ranges = Vec::with_capacity(SECTIONS);
    for _ in 0..SECTIONS {
        let off = r.u64()? as usize;
        let len = r.u64()? as usize;
        if off.checked_add(len)? > data.len() {
            return None;
        }
        ranges.push((off, off + len));
    }
    let mac_at = r.i;
    let mac = r.take(32)?;
    if !constant_time_eq(&self::mac(view_key, &data[..mac_at], &data[mac_at + 32..]), mac) {
        return None;
    }
    let body_start = mac_at + 32;
    let mut expect = body_start;
    for (a, b) in &ranges {
        if *a != expect {
            return None;
        }
        expect = *b;
    }
    if expect != data.len() {
        return None;
    }
    let data = Rc::new(data);
    let requests = decode_requests(Raw { data: data.clone(), start: ranges[8].0, end: ranges[8].1 })?;
    let raws: Vec<Raw> = ranges[..8].iter().map(|(a, b)| Raw { data: data.clone(), start: *a, end: *b }).collect();
    let recipients = Raw { data: data.clone(), start: ranges[9].0, end: ranges[9].1 };
    let summaries = Raw { data: data.clone(), start: ranges[10].0, end: ranges[10].1 };
    Some(Loaded { position: Position { stream, seq, sha256, start, end }, state: State::from_raw(raws, recipients, summaries), requests })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(bytes: Vec<u8>) -> Raw {
        let end = bytes.len();
        Raw { data: Rc::new(Bytes::Owned(bytes)), start: 0, end }
    }

    #[test]
    fn request_index_round_trip() {
        let mut requests = Requests::default();
        for i in 0..500u64 {
            requests.insert(format!("req-{:04}", (i * 7919) % 500), [i + 1, i * 10, i * 10 + 9]);
        }
        let mut out = Vec::new();
        encode_requests(&mut out, &requests);
        let mut loaded = decode_requests(raw(out)).expect("decodes");
        for i in 0..500u64 {
            assert_eq!(loaded.get(&format!("req-{:04}", (i * 7919) % 500)), Some([i + 1, i * 10, i * 10 + 9]));
        }
        assert_eq!(loaded.get("req-9999"), None);
        assert!(!loaded.contains_key(""));
        loaded.insert("new".into(), [501, 1, 2]);
        loaded.insert("req-0000".into(), [9, 9, 9]);
        assert_eq!(loaded.get("new"), Some([501, 1, 2]));
        assert_eq!(loaded.get("req-0000"), Some([9, 9, 9]));
        let entries = loaded.entries();
        assert_eq!(entries.len(), 501);
        assert_eq!(entries.iter().filter(|(k, _)| *k == "req-0000").count(), 1);
    }

    #[test]
    fn request_index_rejects_disorder() {
        let mut out = Vec::new();
        out.extend_from_slice(&2u32.to_le_bytes());
        let mut body = Vec::new();
        out.extend_from_slice(&0u32.to_le_bytes());
        bytes(&mut body, b"b");
        body.extend_from_slice(&[1, 1, 1]);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes(&mut body, b"a");
        body.extend_from_slice(&[1, 1, 1]);
        out.extend_from_slice(&body);
        assert!(decode_requests(raw(out)).is_none());
    }

    #[test]
    fn peek_matches_full_decode() {
        let v = crate::json::parse(br#"{"a":[1,{"x":null}],"recipient":"me","n":-5,"f":1.5,"big":123456789012345678901234567890,"t":true}"#, crate::json::LENIENT).unwrap();
        let mut out = Vec::new();
        encode_value(&mut out, &v);
        for key in ["a", "recipient", "n", "f", "big", "t"] {
            let peeked = peek_field(&out, key).unwrap().unwrap();
            assert!(peeked.py_eq(v.at(key)), "{key}");
        }
        assert!(peek_field(&out, "missing").unwrap().is_none());
        let mut arr = Vec::new();
        encode_value(&mut arr, &Value::Arr(vec![]));
        assert!(peek_field(&arr, "a").is_none());
    }

    #[test]
    fn mac_covers_header_and_body() {
        let key = [7u8; 64];
        let body: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let tag = mac(&key, b"head", &body);
        assert_eq!(tag, mac(&key, b"head", &body));
        assert_ne!(tag, mac(&key, b"hea", &body));
        let mut changed = body.clone();
        changed[2_999_999] ^= 1;
        assert_ne!(tag, mac(&key, b"head", &changed));
        assert_ne!(tag, mac(&[8u8; 64], b"head", &body));
        assert_ne!(mac(&key, b"", b"ab"), mac(&key, b"a", b"b"));
    }
}
