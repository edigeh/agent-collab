//! Python standard-library behaviour the board depends on: `datetime` ISO
//! parsing and formatting, `str` whitespace rules, `fnmatch.fnmatchcase`,
//! urlsafe base64, `os.path.realpath`, `uuid.uuid4` and `OSError` texts.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------- time

/// An aware datetime: the instant in UTC microseconds plus the offset it was written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ts {
    pub utc_us: i64,
    pub offset_us: i64,
}

impl PartialOrd for Ts {
    fn partial_cmp(&self, other: &Ts) -> Option<std::cmp::Ordering> {
        Some(self.utc_us.cmp(&other.utc_us))
    }
}

const US: i64 = 1_000_000;
const DAY_US: i64 = 86_400 * US;

pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

impl Ts {
    pub fn now() -> Ts {
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        Ts { utc_us: d.as_secs() as i64 * US + d.subsec_micros() as i64, offset_us: 0 }
    }

    /// `datetime.fromisoformat(value.replace('Z', '+00:00'))`, requiring a timezone.
    pub fn parse(value: &str) -> Option<Ts> {
        let replaced = value.replace('Z', "+00:00");
        parse_iso(&replaced)
    }

    /// `datetime.isoformat()`: microseconds only when nonzero, offset as ±HH:MM[:SS[.ffffff]].
    pub fn isoformat(&self) -> String {
        let local = self.utc_us + self.offset_us;
        let days = local.div_euclid(DAY_US);
        let rem = local.rem_euclid(DAY_US);
        let (y, m, d) = civil_from_days(days);
        let (h, mi, s, us) = (rem / 3_600_000_000, rem / 60_000_000 % 60, rem / US % 60, rem % US);
        let mut out = format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}");
        if us != 0 {
            out.push_str(&format!(".{us:06}"));
        }
        let off = self.offset_us;
        let sign = if off < 0 { '-' } else { '+' };
        let a = off.abs();
        let (oh, om, os, ous) = (a / 3_600_000_000, a / 60_000_000 % 60, a / US % 60, a % US);
        out.push_str(&format!("{sign}{oh:02}:{om:02}"));
        if os != 0 || ous != 0 {
            out.push_str(&format!(":{os:02}"));
            if ous != 0 {
                out.push_str(&format!(".{ous:06}"));
            }
        }
        out
    }

    /// `datetime.timestamp()` (seconds as a float).
    pub fn seconds(&self) -> f64 {
        self.utc_us as f64 / 1e6
    }

    pub fn add_seconds(&self, s: i64) -> Ts {
        Ts { utc_us: self.utc_us + s * US, offset_us: self.offset_us }
    }

    /// `(self - other).total_seconds()`.
    pub fn diff_seconds(&self, other: &Ts) -> f64 {
        (self.utc_us - other.utc_us) as f64 / 1e6
    }
}

pub fn utcnow() -> String {
    Ts::now().isoformat()
}

struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn digits(&mut self, n: usize) -> Option<i64> {
        if self.i + n > self.b.len() {
            return None;
        }
        let mut v = 0i64;
        for k in 0..n {
            let c = self.b[self.i + k];
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        self.i += n;
        Some(v)
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn done(&self) -> bool {
        self.i >= self.b.len()
    }
}

/// Parses HH[:MM[:SS[.f+]]] (extended) or HH[MM[SS[.f+]]] (basic). Returns microseconds.
#[allow(unused_assignments)]
fn parse_clock(c: &mut Cursor) -> Option<i64> {
    let h = c.digits(2)?;
    let mut extended = None;
    let mut m = 0;
    let mut s = 0;
    let mut us = 0;
    let at_tz = |c: &Cursor| matches!(c.peek(), None | Some(b'+') | Some(b'-'));
    if !at_tz(c) {
        if c.eat(b':') {
            extended = Some(true);
        } else {
            extended = Some(false);
        }
        m = c.digits(2)?;
        if !at_tz(c) {
            if extended == Some(true) {
                if !c.eat(b':') {
                    return None;
                }
            }
            s = c.digits(2)?;
            if c.peek() == Some(b'.') || c.peek() == Some(b',') {
                c.i += 1;
                let start = c.i;
                while c.peek().is_some_and(|d| d.is_ascii_digit()) {
                    c.i += 1;
                }
                let frac = &c.b[start..c.i];
                if frac.is_empty() {
                    return None;
                }
                let mut v = 0i64;
                for k in 0..6 {
                    v = v * 10 + frac.get(k).map(|d| (d - b'0') as i64).unwrap_or(0);
                }
                us = v;
            }
        }
    }
    if h > 23 || m > 59 || s > 59 {
        return None;
    }
    Some(((h * 60 + m) * 60 + s) * US + us)
}

fn parse_iso(text: &str) -> Option<Ts> {
    if !text.is_ascii() {
        // Only the separator may be non-ASCII; normalise it if so.
        let chars: Vec<char> = text.chars().collect();
        for sep_at in [10usize, 8] {
            if chars.len() > sep_at && chars[..sep_at].iter().all(|c| c.is_ascii()) && chars[sep_at + 1..].iter().all(|c| c.is_ascii()) {
                let mut s: String = chars[..sep_at].iter().collect();
                s.push('T');
                s.extend(chars[sep_at + 1..].iter());
                return parse_iso(&s);
            }
        }
        return None;
    }
    let b = text.as_bytes();
    let mut c = Cursor { b, i: 0 };
    let y = c.digits(4)?;
    let (mo, d);
    let basic;
    if c.eat(b'-') {
        basic = false;
        mo = c.digits(2)?;
        if !c.eat(b'-') {
            return None;
        }
        d = c.digits(2)?;
    } else {
        basic = true;
        mo = c.digits(2)?;
        d = c.digits(2)?;
    }
    let _ = basic;
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || y < 1 {
        return None;
    }
    let date_us = days_from_civil(y, mo, d) * DAY_US;
    if c.done() {
        return None; // a date without time has no timezone
    }
    c.i += 1; // any single separator character
    let clock = parse_clock(&mut c)?;
    if c.done() {
        return None; // naive
    }
    let sign = match c.peek() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return None,
    };
    c.i += 1;
    let oh = c.digits(2)?;
    let mut off = oh * 3_600 * US;
    if !c.done() {
        let ext = c.eat(b':');
        let om = c.digits(2)?;
        off += om * 60 * US;
        if !c.done() {
            if ext && !c.eat(b':') {
                return None;
            }
            let os = c.digits(2)?;
            off += os * US;
            if c.eat(b'.') || c.eat(b',') {
                let start = c.i;
                while c.peek().is_some_and(|d| d.is_ascii_digit()) {
                    c.i += 1;
                }
                let frac = &b[start..c.i];
                if frac.is_empty() {
                    return None;
                }
                let mut v = 0i64;
                for k in 0..6 {
                    v = v * 10 + frac.get(k).map(|d| (d - b'0') as i64).unwrap_or(0);
                }
                off += v;
            }
            if om > 59 || os > 59 {
                return None;
            }
        } else if om > 59 {
            return None;
        }
    }
    if !c.done() || off >= 24 * 3_600 * US {
        return None;
    }
    let offset_us = sign * off;
    Some(Ts { utc_us: date_us + clock - offset_us, offset_us })
}

// ---------------------------------------------------------------- str

/// `str.isspace()` for one character.
pub fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `bool(value.strip())`.
pub fn has_text(s: &str) -> bool {
    s.chars().any(|c| !py_isspace(c))
}

/// `' '.join(s.split())`.
pub fn split_join(s: &str) -> String {
    s.split(py_isspace).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
}

/// `s.split()`.
pub fn py_split(s: &str) -> Vec<&str> {
    s.split(py_isspace).filter(|p| !p.is_empty()).collect()
}

/// `s[:n]` by code points.
pub fn prefix_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// `s.strip()`.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

// ---------------------------------------------------------------- fnmatch

/// `fnmatch.fnmatchcase(name, pattern)`: `*` and `?` cross `/`, `[...]`/`[!...]` classes.
pub fn fnmatchcase(name: &str, pattern: &str) -> bool {
    if !pattern.contains(['*', '?', '[']) {
        return name == pattern;
    }
    let name: Vec<char> = name.chars().collect();
    let pat: Vec<char> = pattern.chars().collect();
    let tokens = compile(&pat);
    matches(&tokens, &name)
}

enum Tok {
    Any,
    Star,
    Lit(char),
    Class { negate: bool, items: Vec<(char, char)> },
}

fn compile(p: &[char]) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < p.len() {
        let c = p[i];
        i += 1;
        match c {
            '*' => {
                if !matches!(out.last(), Some(Tok::Star)) {
                    out.push(Tok::Star);
                }
            }
            '?' => out.push(Tok::Any),
            '[' => {
                let mut j = i;
                if j < p.len() && p[j] == '!' {
                    j += 1;
                }
                if j < p.len() && p[j] == ']' {
                    j += 1;
                }
                while j < p.len() && p[j] != ']' {
                    j += 1;
                }
                if j >= p.len() {
                    out.push(Tok::Lit('['));
                    continue;
                }
                let mut body = &p[i..j];
                i = j + 1;
                let negate = !body.is_empty() && body[0] == '!';
                if negate {
                    body = &body[1..];
                }
                // Split into chunks on '-' the way fnmatch.translate does.
                let mut items = Vec::new();
                let mut k = 0;
                while k < body.len() {
                    if k + 2 < body.len() && body[k + 1] == '-' {
                        let (lo, hi) = (body[k], body[k + 2]);
                        if lo <= hi {
                            items.push((lo, hi));
                        }
                        k += 3;
                    } else {
                        items.push((body[k], body[k]));
                        k += 1;
                    }
                }
                out.push(Tok::Class { negate, items });
            }
            c => out.push(Tok::Lit(c)),
        }
    }
    out
}

fn one(t: &Tok, c: char) -> bool {
    match t {
        Tok::Any => true,
        Tok::Lit(l) => *l == c,
        Tok::Class { negate, items } => {
            if items.is_empty() {
                return *negate;
            }
            items.iter().any(|(lo, hi)| *lo <= c && c <= *hi) != *negate
        }
        Tok::Star => unreachable!(),
    }
}

fn matches(tokens: &[Tok], name: &[char]) -> bool {
    let (mut t, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if t < tokens.len() {
            if let Tok::Star = tokens[t] {
                star = Some((t, n));
                t += 1;
                continue;
            }
            if one(&tokens[t], name[n]) {
                t += 1;
                n += 1;
                continue;
            }
        }
        if let Some((st, sn)) = star {
            t = st + 1;
            n = sn + 1;
            star = Some((st, sn + 1));
            continue;
        }
        return false;
    }
    while t < tokens.len() && matches!(tokens[t], Tok::Star) {
        t += 1;
    }
    t == tokens.len()
}

// ---------------------------------------------------------------- base64

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// `base64.urlsafe_b64encode`.
pub fn b64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// `base64.urlsafe_b64decode` (non-validating: characters outside the alphabet are dropped).
pub fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    let mut vals = Vec::new();
    let mut pad = 0;
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => {
                pad += 1;
                // binascii stops at the padding that completes a quantum.
                if (vals.len() + pad) % 4 == 0 {
                    break;
                }
                continue;
            }
            _ => continue,
        };
        if pad > 0 {
            // Data after padding: binascii treats a '=' inside a quantum as excess.
            return None;
        }
        vals.push(v);
    }
    match vals.len() % 4 {
        1 => return None,
        2 if pad < 2 => return None,
        3 if pad < 1 => return None,
        _ => {}
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        let mut n = 0u32;
        for (k, v) in chunk.iter().enumerate() {
            n |= (*v as u32) << (18 - 6 * k);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------- paths

/// `os.path.abspath`-style lexical normalisation of an absolute path.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for comp in path.components() {
        match comp {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(p) => out.push(p),
        }
    }
    out
}

pub fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize(path)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        normalize(&cwd.join(path))
    }
}

/// `Path.expanduser()`.
pub fn expanduser(path: &str) -> PathBuf {
    if path == "~" || path.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut p = PathBuf::from(home);
            if path.len() > 2 {
                p.push(&path[2..]);
            }
            return p;
        }
    }
    PathBuf::from(path)
}

/// `Path(path).absolute()` without normalising `..` (pathlib keeps it), but collapsing `.` and `//`.
pub fn pathlib_absolute(path: &Path) -> PathBuf {
    let base = if path.is_absolute() { PathBuf::new() } else { std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")) };
    let mut out = base;
    for comp in path.components() {
        match comp {
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => out.push(".."),
            Component::Normal(p) => out.push(p),
            Component::Prefix(_) => {}
        }
    }
    out
}

/// `os.path.realpath(path, strict=False)` (what `Path.resolve()` does).
pub fn resolve(path: &Path) -> PathBuf {
    let start = pathlib_absolute(path);
    let mut rest: Vec<OsString> = start
        .components()
        .filter_map(|c| match c {
            Component::Normal(p) => Some(p.to_os_string()),
            Component::ParentDir => Some(OsString::from("..")),
            _ => None,
        })
        .collect();
    rest.reverse();
    let mut current = PathBuf::from("/");
    let mut links = 0;
    while let Some(name) = rest.pop() {
        if name == ".." {
            current.pop();
            continue;
        }
        let candidate = current.join(&name);
        match std::fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_symlink() => {
                links += 1;
                if links > 40 {
                    // A loop: realpath(strict=False) returns the rest unresolved.
                    let mut out = candidate;
                    while let Some(n) = rest.pop() {
                        out.push(n);
                    }
                    return normalize(&out);
                }
                match std::fs::read_link(&candidate) {
                    Ok(target) => {
                        let mut parts: Vec<OsString> = target
                            .components()
                            .filter_map(|c| match c {
                                Component::Normal(p) => Some(p.to_os_string()),
                                Component::ParentDir => Some(OsString::from("..")),
                                _ => None,
                            })
                            .collect();
                        if target.is_absolute() {
                            current = PathBuf::from("/");
                        }
                        parts.reverse();
                        rest.extend(parts);
                    }
                    Err(_) => current = candidate,
                }
            }
            _ => current = candidate,
        }
    }
    current
}

/// `Path.resolve(strict=True)`: resolve and require existence.
pub fn resolve_strict(path: &Path) -> std::io::Result<PathBuf> {
    let p = std::fs::canonicalize(pathlib_absolute(path)).map_err(|e| with_path(e, path))?;
    Ok(p)
}

/// `PurePath.is_relative_to`.
pub fn is_relative_to(path: &Path, base: &Path) -> bool {
    path.starts_with(base)
}

pub fn path_str(p: &Path) -> String {
    String::from_utf8_lossy(p.as_os_str().as_bytes()).into_owned()
}

pub fn path_from_bytes(b: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(b))
}

// ---------------------------------------------------------------- errors

/// An io::Error whose Display matches Python's `str(OSError)`.
pub fn with_path(err: std::io::Error, path: &Path) -> std::io::Error {
    let code = err.raw_os_error();
    let msg = match code {
        Some(c) => format!("[Errno {c}] {}: '{}'", strerror(c), path_str(path)),
        None => format!("{err}: '{}'", path_str(path)),
    };
    std::io::Error::new(err.kind(), msg)
}

pub fn os_error_text(err: &std::io::Error) -> String {
    if let Some(c) = err.raw_os_error() {
        return format!("[Errno {c}] {}", strerror(c));
    }
    err.to_string()
}

pub fn strerror(code: i32) -> String {
    unsafe {
        let p = libc::strerror(code);
        if p.is_null() {
            return format!("Unknown error {code}");
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

// ---------------------------------------------------------------- randomness

pub fn random_bytes(buf: &mut [u8]) {
    let mut filled = 0;
    while filled < buf.len() {
        #[cfg(target_os = "linux")]
        let n = unsafe { libc::getrandom(buf[filled..].as_mut_ptr() as *mut libc::c_void, buf.len() - filled, 0) };
        #[cfg(not(target_os = "linux"))]
        let n = unsafe {
            let chunk = (buf.len() - filled).min(256);
            if libc::getentropy(buf[filled..].as_mut_ptr() as *mut libc::c_void, chunk) == 0 {
                chunk as isize
            } else {
                -1
            }
        };
        if n <= 0 {
            let mut f = std::fs::File::open("/dev/urandom").expect("no randomness source");
            std::io::Read::read_exact(&mut f, &mut buf[filled..]).expect("no randomness source");
            return;
        }
        filled += n as usize;
    }
}

pub fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

/// `str(uuid.uuid4())`.
pub fn uuid4() -> String {
    let mut b = [0u8; 16];
    random_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trip() {
        let t = Ts::parse("2026-10-17T02:09:40.123456+00:00").unwrap();
        assert_eq!(t.isoformat(), "2026-10-17T02:09:40.123456+00:00");
        let t = Ts::parse("2026-10-17T02:09:40Z").unwrap();
        assert_eq!(t.isoformat(), "2026-10-17T02:09:40+00:00");
        assert_eq!(t.add_seconds(600).isoformat(), "2026-10-17T02:19:40+00:00");
        let t = Ts::parse("2026-10-17T02:09:40.5-05:30").unwrap();
        assert_eq!(t.isoformat(), "2026-10-17T02:09:40.500000-05:30");
        assert!(Ts::parse("2026-10-17T02:09:40").is_none());
        assert!(Ts::parse("2026-10-17").is_none());
        assert!(Ts::parse("2026-02-30T00:00:00+00:00").is_none());
    }

    #[test]
    fn fnmatch_cases() {
        assert!(fnmatchcase("/a/b/c.py", "/a/*"));
        assert!(fnmatchcase("abc", "a?c"));
        assert!(fnmatchcase("abc", "a[a-c]c"));
        assert!(!fnmatchcase("abc", "a[!a-c]c"));
        assert!(fnmatchcase("a[c", "a[c"));
        assert!(fnmatchcase("a]c", "a[]]c"));
        assert!(!fnmatchcase("abc", "ab"));
        assert!(fnmatchcase("", "*"));
    }

    #[test]
    fn base64_round_trip() {
        for s in [&b""[..], b"a", b"ab", b"abc", b"abcd\xff\xfe"] {
            assert_eq!(b64url_decode(&b64url_encode(s)).unwrap(), s);
        }
        assert!(b64url_decode("abc").is_none());
    }

    #[test]
    fn whitespace() {
        assert!(!has_text(" \u{1c}\t\u{a0}"));
        assert_eq!(split_join("  a \u{1f} b  "), "a b");
    }
}

/// FxHash: a fast non-cryptographic hasher for in-memory tables keyed by IDs.
#[derive(Default, Clone, Copy)]
pub struct FxHasher {
    hash: u64,
}

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut buf = [0u8; 8];
            buf[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(buf) ^ (rest.len() as u64) << 56);
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.hash = (self.hash.rotate_left(5) ^ w).wrapping_mul(SEED);
    }
}

pub type FxBuild = std::hash::BuildHasherDefault<FxHasher>;
