//! Durable, checksummed, append-only newline-delimited JSON journal.
//!
//! Byte-compatible with `collab_core/journal.py`: the same envelope, hash
//! chain, record cap, sibling lock file and tail recovery.

use crate::error::{Error, Result};
use crate::json::{self, Map, Value};
use crate::pyfmt;
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const ZERO_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
pub const MAX_RECORD: usize = 1_048_576;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    pub stream: String,
    pub seq: u64,
    pub sha256: String,
    pub start: u64,
    pub end: u64,
}

impl Position {
    pub fn to_value(&self) -> Value {
        crate::obj! {
            "stream" => self.stream.clone(), "seq" => self.seq, "sha256" => self.sha256.clone(),
            "start" => self.start, "end" => self.end,
        }
    }
}

/// One committed record. `payload` is the envelope's payload object.
#[derive(Clone, Debug)]
pub struct Record {
    pub stream: String,
    pub seq: u64,
    pub prev: String,
    pub sha256: String,
    pub payload: Value,
}

impl Record {
    pub fn envelope(&self) -> Value {
        crate::obj! {
            "payload" => self.payload.clone(), "prev" => self.prev.clone(), "seq" => self.seq,
            "sha256" => self.sha256.clone(), "stream" => self.stream.clone(), "version" => 1i64,
        }
    }
}

fn cpath(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap_or_else(|_| CString::new("\u{1}").unwrap())
}

fn last_os_error(path: &Path) -> std::io::Error {
    pyfmt::with_path(std::io::Error::last_os_error(), path)
}

fn journal_failed(err: std::io::Error) -> Error {
    Error::Journal(format!("journal operation failed: {err}"))
}

pub fn fsync_directory(path: &Path) -> std::io::Result<()> {
    let c = cpath(path);
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(last_os_error(path));
    }
    let rc = unsafe { libc::fsync(fd) };
    let err = if rc != 0 { Some(last_os_error(path)) } else { None };
    unsafe { libc::close(fd) };
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

pub fn ensure_directory(path: &Path) -> Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(Error::Journal(format!("journal parent is not a directory: {}", path.display())));
            }
            return Ok(());
        }
        Err(_) => {}
    }
    let parent = match path.parent() {
        Some(p) if p != path => p,
        _ => return Err(Error::Journal(format!("cannot create journal directory: {}", path.display()))),
    };
    ensure_directory(parent)?;
    let c = cpath(path);
    if unsafe { libc::mkdir(c.as_ptr(), 0o700) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EEXIST) {
            if !path.is_dir() {
                return Err(Error::Journal(format!("journal parent is not a directory: {}", path.display())));
            }
            return Ok(());
        }
        return Err(journal_failed(pyfmt::with_path(err, path)));
    }
    fsync_directory(parent).map_err(journal_failed)?;
    Ok(())
}

pub struct Fd(pub i32);

impl Drop for Fd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe { libc::close(self.0) };
        }
    }
}

fn fstat(fd: i32) -> std::io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(st)
}

fn file_size(fd: i32) -> std::io::Result<u64> {
    Ok(fstat(fd)?.st_size as u64)
}

fn open_private_regular(path: &Path, flags: i32) -> Result<(Fd, bool)> {
    let common = libc::O_CLOEXEC | libc::O_NOFOLLOW;
    let c = cpath(path);
    let mut created = false;
    let mut fd = unsafe { libc::open(c.as_ptr(), flags | common | libc::O_CREAT | libc::O_EXCL, 0o600 as libc::c_uint) };
    if fd >= 0 {
        created = true;
    } else {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EEXIST) {
            return Err(journal_failed(pyfmt::with_path(err, path)));
        }
        fd = unsafe { libc::open(c.as_ptr(), flags | common) };
        if fd < 0 {
            return Err(journal_failed(last_os_error(path)));
        }
    }
    let fd = Fd(fd);
    let st = fstat(fd.0).map_err(journal_failed)?;
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(Error::Journal(format!("journal path is not a regular file: {}", path.display())));
    }
    unsafe { libc::fchmod(fd.0, 0o600) };
    if created {
        unsafe { libc::fsync(fd.0) };
        fsync_directory(path.parent().unwrap_or(Path::new("/"))).map_err(journal_failed)?;
    }
    Ok((fd, created))
}

pub fn pread_exact(fd: i32, len: usize, offset: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut done = 0;
    while done < len {
        let n = unsafe {
            libc::pread(fd, buf[done..].as_mut_ptr() as *mut libc::c_void, len - done, (offset + done as u64) as libc::off_t)
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            break;
        }
        done += n as usize;
    }
    buf.truncate(done);
    Ok(buf)
}

fn write_all(fd: i32, data: &[u8]) -> std::io::Result<()> {
    let mut done = 0;
    while done < data.len() {
        let n = unsafe { libc::write(fd, data[done..].as_ptr() as *const libc::c_void, data.len() - done) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            return Err(std::io::Error::other("journal write made no progress"));
        }
        done += n as usize;
    }
    Ok(())
}

fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// (`uuid.UUID(s)` parses, `str(uuid.UUID(s)) == s`).
fn uuid_status(s: &str) -> (bool, bool) {
    let mut t = s.trim_start_matches("urn:").trim_start_matches("uuid:");
    t = t.trim_matches(|c| c == '{' || c == '}');
    let compact: String = t.chars().filter(|c| *c != '-').collect();
    let parses = compact.len() == 32 && compact.bytes().all(|c| c.is_ascii_hexdigit());
    let canonical = s.len() == 36
        && s.bytes().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == b'-' } else { matches!(c, b'0'..=b'9' | b'a'..=b'f') });
    (parses, canonical)
}

/// SHA-256 of the canonical envelope without `sha256` (keys sorted: payload, prev, seq, stream, version).
pub fn hash_envelope(version: &Value, stream: &str, seq: &Value, prev: &str, payload: &Value) -> Result<String> {
    let mut buf = Vec::with_capacity(512);
    buf.extend_from_slice(b"{\"payload\":");
    json::encode_into(payload, json::CANONICAL, &mut buf)
        .map_err(|e| Error::Journal(format!("value cannot be encoded as canonical JSON: {}", e.0)))?;
    buf.extend_from_slice(b",\"prev\":");
    json::encode_str(prev, false, &mut buf);
    buf.extend_from_slice(b",\"seq\":");
    json::encode_into(seq, json::CANONICAL, &mut buf).unwrap();
    buf.extend_from_slice(b",\"stream\":");
    json::encode_str(stream, false, &mut buf);
    buf.extend_from_slice(b",\"version\":");
    json::encode_into(version, json::CANONICAL, &mut buf).unwrap();
    buf.push(b'}');
    Ok(pyfmt::hex(&Sha256::digest(&buf)))
}

const FIELDS: [&str; 6] = ["payload", "prev", "seq", "sha256", "stream", "version"];

pub fn validate_record(
    raw: &[u8],
    expected_sequence: Option<u64>,
    expected_stream: Option<&str>,
    expected_previous: Option<&str>,
    offset: u64,
) -> Result<Record> {
    let wrap = |msg: String| Error::Corrupt(format!("corrupt journal record at byte {offset}: {msg}"));
    let (value, is_canonical) = json::parse_checked(raw, json::STRICT).map_err(|e| {
        if e.0.contains("too deep") {
            wrap("JSON nesting is too deep".into())
        } else {
            wrap(format!("invalid JSON record: {}", e.0))
        }
    })?;
    let mut map = match value {
        Value::Obj(m) if m.len() == 6 && FIELDS.iter().all(|f| m.contains_key(f)) => m,
        _ => return Err(wrap("record must be an object with the exact envelope fields".into())),
    };
    let version = map.get("version").unwrap();
    if !matches!(version, Value::Int(1)) {
        return Err(wrap("unsupported journal record version".into()));
    }
    let stream = match map.get("stream").unwrap() {
        Value::Str(s) => s.clone(),
        _ => return Err(wrap("record stream must be a UUID string".into())),
    };
    let (parses, canonical) = uuid_status(&stream);
    if !parses {
        return Err(wrap("record stream is not a UUID".into()));
    }
    if !canonical {
        return Err(wrap("record stream UUID is not canonical".into()));
    }
    if let Some(expected) = expected_stream {
        if stream != expected {
            return Err(wrap("record stream changed within the journal".into()));
        }
    }
    let seq_value = map.get("seq").unwrap();
    let seq = match (seq_value, expected_sequence) {
        (Value::Int(i), Some(e)) if *i >= 0 && *i as u64 == e => *i as u64,
        // Unchecked (parallel pass): any int; the ordered pass compares it.
        (Value::Int(i), None) => *i as u64,
        (other, _) => {
            let shown = match other {
                Value::Str(s) => format!("'{s}'"),
                Value::Int(i) => i.to_string(),
                v => json::dumps(v, json::DEFAULT),
            };
            let expected = expected_sequence.map(|e| e.to_string()).unwrap_or_else(|| "an integer".into());
            return Err(wrap(format!("record sequence is {shown}; expected {expected}")));
        }
    };
    let prev = match map.get("prev").unwrap() {
        Value::Str(s) if is_hash(s) => s.clone(),
        _ => return Err(wrap("record previous hash is malformed".into())),
    };
    if let Some(expected) = expected_previous {
        if prev != expected {
            return Err(wrap("record previous hash does not match committed history".into()));
        }
    }
    if !map.get("payload").unwrap().is_obj() {
        return Err(wrap("record payload must be a JSON object".into()));
    }
    if !json::all_finite(map.get("payload").unwrap()) {
        return Err(wrap("payload contains a non-finite number".into()));
    }
    let sha = match map.get("sha256").unwrap() {
        Value::Str(s) if is_hash(s) => s.clone(),
        _ => return Err(wrap("record checksum is malformed".into())),
    };
    // A canonical line hashes as itself minus its `,"sha256":"…"` pair, which in
    // sorted order sits right before the fixed-size stream and version fields.
    const TAIL: usize = 76 + 12 + 36 + 13; // ,"sha256":"<64>" ,"stream":"<36>" ,"version":1}
    if is_canonical && raw.len() > TAIL && raw[raw.len() - TAIL..].starts_with(b",\"sha256\":\"") {
        let cut = raw.len() - TAIL;
        let mut hasher = Sha256::new();
        hasher.update(&raw[..cut]);
        hasher.update(&raw[cut + 76..]);
        if pyfmt::hex(&hasher.finalize()) != sha {
            return Err(wrap("record checksum does not match its contents".into()));
        }
        let payload = map.remove("payload").unwrap();
        return Ok(Record { stream, seq, prev, sha256: sha, payload });
    }
    let calculated = hash_envelope(map.get("version").unwrap(), &stream, map.get("seq").unwrap(), &prev, map.get("payload").unwrap())
        .map_err(|e| wrap(e.message()))?;
    if sha != calculated {
        return Err(wrap("record checksum does not match its contents".into()));
    }
    let payload = map.remove("payload").unwrap();
    Ok(Record { stream, seq, prev, sha256: sha, payload })
}

pub struct Journal {
    pub path: PathBuf,
    pub lock_path: PathBuf,
    pub timeout: f64,
    pub max_record: usize,
    /// Records this process already validated, for `locked_all`.
    seen: std::cell::RefCell<Option<(Vec<Record>, Vec<Position>)>>,
}

pub struct ReadResult {
    pub records: Vec<Record>,
    pub positions: Vec<Position>,
    pub incomplete_at: Option<u64>,
}

impl Journal {
    pub fn new(path: PathBuf, timeout: f64) -> Journal {
        let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".lock");
        let lock_path = path.with_file_name(name);
        Journal { path, lock_path, timeout, max_record: MAX_RECORD, seen: Default::default() }
    }

    /// Lock, validate, and expose the journal until the returned guard drops.
    pub fn locked(&self, after: Option<&Position>) -> Result<LockedJournal<'_>> {
        self.locked_with(after, &mut |_, _| Ok(()))
    }

    /// `locked`, showing each record read to `each` (in order) as it is validated.
    pub fn locked_with(&self, after: Option<&Position>, each: &mut dyn FnMut(&Record, &Position) -> Result<()>) -> Result<LockedJournal<'_>> {
        ensure_directory(self.path.parent().unwrap_or(Path::new("/")))?;
        let (lock_fd, _) = open_private_regular(&self.lock_path, libc::O_RDWR)?;
        self.acquire(lock_fd.0)?;
        let lock = LockGuard(lock_fd);
        let (journal_fd, _) = open_private_regular(&self.path, libc::O_RDWR | libc::O_APPEND)?;
        let base = match after {
            Some(p) if self.holds(journal_fd.0, p).map_err(journal_failed)? => Some(p.clone()),
            _ => None,
        };
        let mut read = ReadResult { records: Vec::new(), positions: Vec::new(), incomplete_at: None };
        read.incomplete_at = self.read_each(journal_fd.0, base.as_ref(), None, &mut |record, position| {
            each(&record, &position)?;
            read.records.push(record);
            read.positions.push(position);
            Ok(())
        })?;
        if let Some(at) = read.incomplete_at {
            self.recover_tail(journal_fd.0, at)?;
        }
        let head = read.positions.last().cloned().or_else(|| base.clone());
        Ok(LockedJournal { journal: self, fd: journal_fd, _lock: lock, records: read.records, positions: read.positions, base, head, remember: false })
    }

    /// Like `locked(None)`: every committed record. Records this process read
    /// under an earlier lock are reused once the journal confirms the last of
    /// them is still in place; only later records are read and validated.
    pub fn locked_all(&self) -> Result<LockedJournal<'_>> {
        let seen = self.seen.borrow_mut().take();
        let after = seen.as_ref().and_then(|(_, p)| p.last().cloned());
        let mut log = self.locked(after.as_ref())?;
        if let (Some(_), Some((mut records, mut positions))) = (&log.base, seen) {
            records.append(&mut log.records);
            positions.append(&mut log.positions);
            log.records = records;
            log.positions = positions;
            log.base = None;
        }
        log.remember = true;
        Ok(log)
    }

    /// Every committed record, read without the lock (an unfinished tail is ignored).
    pub fn committed(&self) -> Result<(Vec<Record>, Vec<Position>)> {
        let (mut records, mut positions) = (Vec::new(), Vec::new());
        self.committed_each(&mut |record, position| {
            records.push(record);
            positions.push(position);
            Ok(())
        })?;
        Ok((records, positions))
    }

    /// The committed records, read without the lock, handed to `each` in order.
    pub fn committed_each(&self, each: &mut dyn FnMut(Record, Position) -> Result<()>) -> Result<()> {
        let c = cpath(&self.path);
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW) };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                return Ok(());
            }
            return Err(journal_failed(pyfmt::with_path(err, &self.path)));
        }
        let fd = Fd(fd);
        let st = fstat(fd.0).map_err(journal_failed)?;
        if st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(Error::Journal(format!("journal path is not a regular file: {}", self.path.display())));
        }
        self.read_each(fd.0, None, None, each)?;
        Ok(())
    }

    fn acquire(&self, fd: i32) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs_f64(self.timeout);
        loop {
            if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            let code = err.raw_os_error().unwrap_or(0);
            if code != libc::EACCES && code != libc::EAGAIN && code != libc::EWOULDBLOCK && code != libc::EINTR {
                return Err(Error::Journal(format!("could not lock journal: {}", pyfmt::os_error_text(&err))));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Busy(format!("journal remained locked for {:.3} seconds", self.timeout)));
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
        }
    }

    fn holds(&self, fd: i32, position: &Position) -> std::io::Result<bool> {
        let length = position.end.saturating_sub(position.start);
        if !(position.start < position.end && position.end <= file_size(fd)?) || length as usize > self.max_record + 1 {
            return Ok(false);
        }
        let line = pread_exact(fd, length as usize, position.start)?;
        if line.len() as u64 != length || line.last() != Some(&b'\n') {
            return Ok(false);
        }
        match validate_record(&line[..line.len() - 1], Some(position.seq), Some(&position.stream), None, position.start) {
            Ok(record) => Ok(record.sha256 == position.sha256),
            Err(_) => Ok(false),
        }
    }

    pub fn read_and_validate(&self, fd: i32, base: Option<&Position>, stop: Option<u64>) -> Result<ReadResult> {
        let mut records = Vec::new();
        let mut positions = Vec::new();
        let incomplete_at = self.read_each(fd, base, stop, &mut |record, position| {
            records.push(record);
            positions.push(position);
            Ok(())
        })?;
        Ok(ReadResult { records, positions, incomplete_at })
    }

    /// Read and validate the records after `base` (up to `stop`), handing each
    /// to `each` in journal order, and return where an incomplete final line
    /// starts, if any.
    ///
    /// Large reads frame and validate records on worker threads, a slice of
    /// whole lines at a time, while this thread checks sequence numbers and the
    /// hash chain in order and passes records on; so `each` (a replay) runs
    /// while later records are still being validated. A record that failed or
    /// does not chain is validated again in order, so the error reported is
    /// exactly the one a sequential read meets first.
    pub fn read_each(&self, fd: i32, base: Option<&Position>, stop: Option<u64>, each: &mut dyn FnMut(Record, Position) -> Result<()>) -> Result<Option<u64>> {
        let size = match stop {
            Some(s) => s,
            None => file_size(fd).map_err(journal_failed)?,
        };
        let start = base.map(|b| b.end).unwrap_or(0);
        if start >= size {
            return Ok(None);
        }
        let data = pread_exact(fd, (size - start) as usize, start).map_err(journal_failed)?;
        let mut expected_stream: Option<String> = base.map(|b| b.stream.clone());
        let mut expected_previous: String = base.map(|b| b.sha256.clone()).unwrap_or_else(|| ZERO_HASH.to_string());
        let mut next_sequence = base.map(|b| b.seq + 1).unwrap_or(1);

        type Framed = (usize, usize, Option<Result<Record>>);
        let max = self.max_record;
        let data = &data[..];
        let frame = |lo: usize, hi: usize, validate: bool| -> (Vec<Framed>, Option<usize>) {
            let mut out = Vec::new();
            let mut at = lo;
            while at < hi {
                let Some(nl) = memchr(b'\n', &data[at..hi]) else {
                    return (out, Some(at));
                };
                let end = at + nl;
                let checked = (validate && end - at <= max).then(|| validate_record(&data[at..end], None, None, None, start + at as u64));
                out.push((at, end, checked));
                at = end + 1;
            }
            (out, None)
        };
        // The ordered pass over one slice's lines; `Some(at)` once the
        // incomplete final line is reached.
        let mut ordered = |lines: Vec<Framed>, tail: Option<usize>| -> Result<Option<u64>> {
            for (a, b, checked) in lines {
                let offset = start + a as u64;
                if b - a > self.max_record {
                    return Err(Error::Corrupt(format!("committed record at byte {offset} exceeds {} bytes", self.max_record)));
                }
                let seq = next_sequence;
                let record = match checked {
                    Some(Ok(r)) if r.seq == seq && expected_stream.as_deref().is_none_or(|s| s == r.stream) && r.prev == expected_previous => r,
                    _ => validate_record(&data[a..b], Some(seq), expected_stream.as_deref(), Some(&expected_previous), offset)?,
                };
                if expected_stream.is_none() {
                    expected_stream = Some(record.stream.clone());
                }
                expected_previous.clone_from(&record.sha256);
                next_sequence += 1;
                let position = Position { stream: record.stream.clone(), seq: record.seq, sha256: record.sha256.clone(), start: offset, end: start + b as u64 + 1 };
                each(record, position)?;
            }
            Ok(tail.map(|t| start + t as u64))
        };

        if data.len() <= (1 << 20) {
            let (lines, tail) = frame(0, data.len(), false);
            return ordered(lines, tail);
        }
        // Slices of whole lines, about 256 KiB each.
        let mut cuts = vec![0usize];
        let step = 1 << 18;
        while data.len() - *cuts.last().unwrap() > step {
            let guess = *cuts.last().unwrap() + step;
            match memchr(b'\n', &data[guess..]) {
                Some(n) if guess + n + 1 < data.len() => cuts.push(guess + n + 1),
                _ => break,
            }
        }
        cuts.push(data.len());
        let slices: Vec<(usize, usize)> = cuts.windows(2).map(|w| (w[0], w[1])).collect();
        let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 8);
        let next = std::sync::atomic::AtomicUsize::new(0);
        let stop_flag = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel::<(usize, (Vec<Framed>, Option<usize>))>();
            for _ in 0..workers {
                let tx = tx.clone();
                let (next, stop_flag, slices, frame) = (&next, &stop_flag, &slices, &frame);
                scope.spawn(move || loop {
                    if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&(lo, hi)) = slices.get(i) else { break };
                    if tx.send((i, frame(lo, hi, true))).is_err() {
                        break;
                    }
                });
            }
            drop(tx);
            let mut waiting: Vec<Option<(Vec<Framed>, Option<usize>)>> = (0..slices.len()).map(|_| None).collect();
            let mut due = 0;
            let outcome = (|| {
                while due < slices.len() {
                    while waiting[due].is_none() {
                        let (i, part) = rx.recv().expect("validation worker stopped");
                        waiting[i] = Some(part);
                    }
                    let (lines, tail) = waiting[due].take().unwrap();
                    due += 1;
                    if let Some(at) = ordered(lines, tail)? {
                        return Ok(Some(at));
                    }
                }
                Ok(None)
            })();
            stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            drop(rx);
            outcome
        })
    }

    fn recover_tail(&self, fd: i32, tail_at: u64) -> Result<()> {
        let recoveries = self.path.parent().unwrap_or(Path::new("/")).join("recoveries");
        ensure_directory(&recoveries)?;
        let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let mut rnd = [0u8; 16];
        pyfmt::random_bytes(&mut rnd);
        let target = recoveries.join(format!("{name}.tail-{nanos}-{}-{}.bin", std::process::id(), pyfmt::hex(&rnd)));
        let c = cpath(&target);
        let out = unsafe { libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_CREAT | libc::O_EXCL, 0o600 as libc::c_uint) };
        if out < 0 {
            return Err(journal_failed(last_os_error(&target)));
        }
        let out = Fd(out);
        let size = file_size(fd).map_err(journal_failed)?;
        let tail = pread_exact(fd, (size - tail_at) as usize, tail_at).map_err(journal_failed)?;
        if (tail.len() as u64) < size - tail_at {
            return Err(Error::Journal("journal tail changed while it was being recovered".into()));
        }
        write_all(out.0, &tail).map_err(journal_failed)?;
        unsafe { libc::fsync(out.0) };
        drop(out);
        fsync_directory(&recoveries).map_err(journal_failed)?;
        if unsafe { libc::ftruncate(fd, tail_at as libc::off_t) } != 0 {
            return Err(journal_failed(std::io::Error::last_os_error()));
        }
        unsafe { libc::fsync(fd) };
        Ok(())
    }
}

/// Position of the first `needle` byte, eight bytes at a time.
pub fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let pattern = LO * needle as u64;
    let mut i = 0;
    while i + 8 <= hay.len() {
        let w = u64::from_le_bytes(hay[i..i + 8].try_into().unwrap()) ^ pattern;
        let found = w.wrapping_sub(LO) & !w & HI;
        if found != 0 {
            return Some(i + (found.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    hay[i..].iter().position(|&b| b == needle).map(|p| p + i)
}

struct LockGuard(Fd);

impl Drop for LockGuard {
    fn drop(&mut self) {
        unsafe { libc::flock((self.0).0, libc::LOCK_UN) };
    }
}

/// A validated journal held under its exclusive lock.
pub struct LockedJournal<'a> {
    journal: &'a Journal,
    fd: Fd,
    // Dropped after `fd` (field order), so the journal closes before unlocking.
    _lock: LockGuard,
    pub records: Vec<Record>,
    pub positions: Vec<Position>,
    pub base: Option<Position>,
    pub head: Option<Position>,
    remember: bool,
}

impl Drop for LockedJournal<'_> {
    fn drop(&mut self) {
        // Still under the lock: these records are exactly the journal's contents.
        if self.remember && self.base.is_none() {
            *self.journal.seen.borrow_mut() = Some((std::mem::take(&mut self.records), std::mem::take(&mut self.positions)));
        }
    }
}

impl<'a> LockedJournal<'a> {
    /// Every committed record, reading and validating the prefix before `base`.
    pub fn history(&self) -> Result<Vec<Record>> {
        let Some(base) = &self.base else {
            return Ok(self.records.clone());
        };
        let prefix = self.journal.read_and_validate(self.fd.0, None, Some(base.end))?;
        if prefix.positions.last() != Some(base) {
            return Err(Error::Corrupt("journal prefix does not end at its verified position".into()));
        }
        let mut all = prefix.records;
        all.extend(self.records.iter().cloned());
        Ok(all)
    }

    /// Committed record `seq`, read from bytes [start, end) and validated on its own.
    pub fn record_at(&self, seq: u64, start: u64, end: u64) -> Result<Record> {
        let line = if start < end { pread_exact(self.fd.0, (end - start) as usize, start).map_err(journal_failed)? } else { Vec::new() };
        if line.len() as u64 != end.saturating_sub(start) || line.last() != Some(&b'\n') || start >= end {
            return Err(Error::Corrupt(format!("record {seq} is not at bytes {start}..{end}")));
        }
        validate_record(&line[..line.len() - 1], Some(seq), self.head.as_ref().map(|h| h.stream.as_str()), None, start)
    }

    /// Durably append one payload and return its record.
    pub fn append(&mut self, payload: Value) -> Result<Record> {
        if !payload.is_obj() {
            return Err(Error::Journal("journal payload must be a JSON object".into()));
        }
        if !json::all_finite(&payload) {
            return Err(Error::Journal("payload contains a non-finite number".into()));
        }
        let (stream, seq, prev) = match &self.head {
            Some(h) => (h.stream.clone(), h.seq + 1, h.sha256.clone()),
            None => (pyfmt::uuid4(), 1, ZERO_HASH.to_string()),
        };
        let seq_value = Value::Int(seq as i64);
        let version = Value::Int(1);
        let sha = hash_envelope(&version, &stream, &seq_value, &prev, &payload)?;
        let mut envelope = Map::with_capacity(6);
        envelope.push("version", version);
        envelope.push("stream", Value::Str(stream.clone()));
        envelope.push("seq", seq_value);
        envelope.push("prev", Value::Str(prev.clone()));
        envelope.push("payload", payload);
        envelope.push("sha256", Value::Str(sha.clone()));
        let envelope = Value::Obj(envelope);
        let mut encoded = json::canonical(&envelope);
        if encoded.len() > self.journal.max_record {
            return Err(Error::Journal(format!(
                "encoded journal record is {} bytes; maximum is {}",
                encoded.len(),
                self.journal.max_record
            )));
        }
        let start = file_size(self.fd.0).map_err(|e| Error::Journal(format!("could not durably append journal record: {}", pyfmt::os_error_text(&e))))?;
        encoded.push(b'\n');
        write_all(self.fd.0, &encoded)
            .and_then(|_| if unsafe { libc::fsync(self.fd.0) } != 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) })
            .map_err(|e| Error::Journal(format!("could not durably append journal record: {}", pyfmt::os_error_text(&e))))?;
        let Value::Obj(mut envelope) = envelope else { unreachable!() };
        let payload = envelope.remove("payload").unwrap();
        let record = Record { stream: stream.clone(), seq, prev, sha256: sha.clone(), payload };
        let position = Position { stream, seq, sha256: sha, start, end: start + encoded.len() as u64 };
        self.records.push(record.clone());
        self.positions.push(position.clone());
        self.head = Some(position);
        Ok(record)
    }
}
