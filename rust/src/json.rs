//! JSON exactly as the Python build reads and writes it.
//!
//! Hashes and recorded results depend on byte-exact encodings, so this module
//! reproduces three `json.dumps` configurations (canonical, default, CLI), a
//! strict `json.loads`, Python's float `repr`, and the equality and truthiness
//! rules the reducer relies on.

use std::fmt::Write as _;

#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    /// An integer outside i64, kept as its exact decimal text.
    Big(Box<str>),
    Float(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Map),
}

pub static NULL: Value = Value::Null;

/// An insertion-ordered object, like a Python dict (including after `pop`).
#[derive(Clone, Debug, Default)]
pub struct Map(pub Vec<(String, Value)>);

impl Map {
    pub fn new() -> Map {
        Map(Vec::new())
    }
    pub fn with_capacity(n: usize) -> Map {
        Map(Vec::with_capacity(n))
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.0.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn contains_key(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }
    /// `d[key] = value`: replaces in place, or appends a new key.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        let key = key.into();
        let value = value.into();
        if let Some(slot) = self.get_mut(&key) {
            *slot = value;
        } else {
            self.0.push((key, value));
        }
    }
    /// Appends without checking for an existing key (caller guarantees uniqueness).
    pub fn push(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        self.0.push((key.into(), value.into()));
    }
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let index = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(index).1)
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter().map(|(k, v)| (k, v))
    }
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.iter().map(|(k, _)| k)
    }
}

impl Value {
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(s.into())
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(m) => m.get(key),
            _ => None,
        }
    }
    /// `d.get(key)` with Python's None default.
    pub fn at(&self, key: &str) -> &Value {
        self.get(key).unwrap_or(&NULL)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        match self {
            Value::Obj(m) => m.get_mut(key),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_obj(&self) -> Option<&Map> {
        match self {
            Value::Obj(m) => Some(m),
            _ => None,
        }
    }
    pub fn as_obj_mut(&mut self) -> Option<&mut Map> {
        match self {
            Value::Obj(m) => Some(m),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
    /// `type(x) is int` (bool excluded).
    pub fn is_int(&self) -> bool {
        matches!(self, Value::Int(_) | Value::Big(_))
    }
    pub fn is_obj(&self) -> bool {
        matches!(self, Value::Obj(_))
    }
    pub fn is_arr(&self) -> bool {
        matches!(self, Value::Arr(_))
    }
    pub fn is_str(&self) -> bool {
        matches!(self, Value::Str(_))
    }
    /// Python truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Big(_) => true,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Arr(a) => !a.is_empty(),
            Value::Obj(m) => !m.is_empty(),
        }
    }
    /// Python `==` between JSON values (`1 == 1.0 == True`, dicts ignore order).
    pub fn py_eq(&self, other: &Value) -> bool {
        use Value::*;
        match (self, other) {
            (Null, Null) => true,
            (Str(a), Str(b)) => a == b,
            (Arr(a), Arr(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.py_eq(y)),
            (Obj(a), Obj(b)) => {
                a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| v.py_eq(w)))
            }
            (Big(a), Big(b)) => a == b,
            _ => match (self.number(), other.number()) {
                (Some(a), Some(b)) => a.eq_num(&b),
                _ => false,
            },
        }
    }
    fn number(&self) -> Option<Num> {
        match self {
            Value::Bool(b) => Some(Num::I(*b as i64)),
            Value::Int(i) => Some(Num::I(*i)),
            Value::Float(f) => Some(Num::F(*f)),
            _ => None,
        }
    }
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NoneType",
            Value::Bool(_) => "bool",
            Value::Int(_) | Value::Big(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "str",
            Value::Arr(_) => "list",
            Value::Obj(_) => "dict",
        }
    }
}

enum Num {
    I(i64),
    F(f64),
}

impl Num {
    fn eq_num(&self, other: &Num) -> bool {
        match (self, other) {
            (Num::I(a), Num::I(b)) => a == b,
            (Num::F(a), Num::F(b)) => a == b,
            (Num::I(i), Num::F(f)) | (Num::F(f), Num::I(i)) => {
                f.fract() == 0.0 && *f >= -9.3e18 && *f <= 9.3e18 && (*f as i128) == (*i as i128)
            }
        }
    }
}

impl From<&String> for Value {
    fn from(s: &String) -> Value {
        Value::Str(s.clone())
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Str(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Str(s)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Value {
        Value::Int(i)
    }
}
impl From<u64> for Value {
    fn from(i: u64) -> Value {
        Value::Int(i as i64)
    }
}
impl From<usize> for Value {
    fn from(i: usize) -> Value {
        Value::Int(i as i64)
    }
}
impl From<Map> for Value {
    fn from(m: Map) -> Value {
        Value::Obj(m)
    }
}
impl From<Vec<Value>> for Value {
    fn from(a: Vec<Value>) -> Value {
        Value::Arr(a)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(o: Option<T>) -> Value {
        match o {
            Some(v) => v.into(),
            None => Value::Null,
        }
    }
}

/// Build an object from key/value pairs in order.
#[macro_export]
macro_rules! obj {
    ($($k:expr => $v:expr),* $(,)?) => {{
        let mut m = $crate::json::Map::new();
        $( m.push($k, $crate::json::Value::from($v)); )*
        $crate::json::Value::Obj(m)
    }};
}

// ---------------------------------------------------------------- parsing

#[derive(Debug)]
pub struct ParseError(pub String);

#[derive(Clone, Copy)]
pub struct ParseOptions {
    /// Reject duplicate keys and NaN/Infinity (the journal's strict hooks).
    pub strict: bool,
}

pub const STRICT: ParseOptions = ParseOptions { strict: true };
pub const LENIENT: ParseOptions = ParseOptions { strict: false };

const MAX_DEPTH: usize = 990;

pub fn parse(data: &[u8], options: ParseOptions) -> Result<Value, ParseError> {
    parse_checked(data, options).map(|(v, _)| v)
}

/// Parse, also reporting whether `data` is exactly the canonical encoding of the result.
pub fn parse_checked(data: &[u8], options: ParseOptions) -> Result<(Value, bool), ParseError> {
    let text = std::str::from_utf8(data).map_err(|e| ParseError(format!("invalid UTF-8: {e}")))?;
    let mut p = Parser { b: text.as_bytes(), i: 0, opt: options, depth: 0, canonical: true };
    p.ws();
    let value = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(p.err("Extra data"));
    }
    Ok((value, p.canonical))
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    opt: ParseOptions,
    depth: usize,
    /// Still byte-identical to the canonical encoding of what was parsed.
    canonical: bool,
}

impl<'a> Parser<'a> {
    fn err(&self, msg: &str) -> ParseError {
        ParseError(format!("{msg}: char {}", self.i))
    }
    #[inline]
    fn ws(&mut self) {
        let start = self.i;
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
        if self.i != start {
            self.canonical = false;
        }
    }
    fn value(&mut self) -> Result<Value, ParseError> {
        if self.i >= self.b.len() {
            return Err(self.err("Expecting value"));
        }
        match self.b[self.i] {
            b'"' => Ok(Value::Str(self.string()?)),
            b'{' => self.object(),
            b'[' => self.array(),
            b't' if self.b[self.i..].starts_with(b"true") => {
                self.i += 4;
                Ok(Value::Bool(true))
            }
            b'f' if self.b[self.i..].starts_with(b"false") => {
                self.i += 5;
                Ok(Value::Bool(false))
            }
            b'n' if self.b[self.i..].starts_with(b"null") => {
                self.i += 4;
                Ok(Value::Null)
            }
            b'N' if self.b[self.i..].starts_with(b"NaN") => {
                self.i += 3;
                self.constant(f64::NAN, "NaN")
            }
            b'I' if self.b[self.i..].starts_with(b"Infinity") => {
                self.i += 8;
                self.constant(f64::INFINITY, "Infinity")
            }
            b'-' if self.b[self.i..].starts_with(b"-Infinity") => {
                self.i += 9;
                self.constant(f64::NEG_INFINITY, "-Infinity")
            }
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(self.err("Expecting value")),
        }
    }
    fn constant(&self, v: f64, name: &str) -> Result<Value, ParseError> {
        if self.opt.strict {
            Err(ParseError(format!("non-finite JSON number: {name}")))
        } else {
            Ok(Value::Float(v))
        }
    }
    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.i;
        let b = self.b;
        if b[self.i] == b'-' {
            self.i += 1;
        }
        if self.i >= b.len() || !b[self.i].is_ascii_digit() {
            return Err(ParseError(format!("Expecting value: char {start}")));
        }
        if b[self.i] == b'0' {
            self.i += 1;
        } else {
            while self.i < b.len() && b[self.i].is_ascii_digit() {
                self.i += 1;
            }
        }
        let mut float = false;
        if self.i + 1 < b.len() && b[self.i] == b'.' && b[self.i + 1].is_ascii_digit() {
            float = true;
            self.i += 1;
            while self.i < b.len() && b[self.i].is_ascii_digit() {
                self.i += 1;
            }
        }
        if self.i < b.len() && (b[self.i] == b'e' || b[self.i] == b'E') {
            let mut j = self.i + 1;
            if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                j += 1;
            }
            if j < b.len() && b[j].is_ascii_digit() {
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                float = true;
                self.i = j;
            }
        }
        let text = std::str::from_utf8(&b[start..self.i]).unwrap();
        if float {
            let f: f64 = text.parse().map_err(|_| self.err("invalid number"))?;
            if !f.is_finite() && self.opt.strict {
                return Err(ParseError("non-finite JSON number".into()));
            }
            if self.canonical && float_repr(f) != text {
                self.canonical = false;
            }
            return Ok(Value::Float(f));
        }
        if text == "-0" {
            self.canonical = false;
        }
        match text.parse::<i64>() {
            Ok(i) => Ok(Value::Int(i)),
            Err(_) => Ok(Value::Big(text.into())),
        }
    }
    fn string(&mut self) -> Result<String, ParseError> {
        let b = self.b;
        self.i += 1; // opening quote
        let start = self.i;
        // Fast path: no escapes.
        while self.i < b.len() {
            let c = b[self.i];
            if c == b'"' {
                let s = unsafe { std::str::from_utf8_unchecked(&b[start..self.i]) }.to_string();
                self.i += 1;
                return Ok(s);
            }
            if c == b'\\' {
                break;
            }
            if c < 0x20 {
                return Err(self.err("Invalid control character"));
            }
            self.i += 1;
        }
        let mut out = String::with_capacity(self.i - start + 16);
        out.push_str(unsafe { std::str::from_utf8_unchecked(&b[start..self.i]) });
        loop {
            if self.i >= b.len() {
                return Err(self.err("Unterminated string"));
            }
            let c = b[self.i];
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    if self.i >= b.len() {
                        return Err(self.err("Unterminated string"));
                    }
                    let e = b[self.i];
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => {
                            self.canonical = false;
                            out.push('/')
                        }
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex_at = self.i;
                            let first = self.hex4()?;
                            // Canonical output escapes only controls without a short form, in lowercase.
                            if first >= 0x20 || matches!(first, 0x08 | 0x09 | 0x0a | 0x0c | 0x0d) || self.b[hex_at..hex_at + 4].iter().any(|c| c.is_ascii_uppercase()) {
                                self.canonical = false;
                            }
                            if (0xd800..0xdc00).contains(&first) {
                                if b[self.i..].starts_with(b"\\u") {
                                    let save = self.i;
                                    self.i += 2;
                                    let second = self.hex4()?;
                                    if (0xdc00..0xe000).contains(&second) {
                                        let code = 0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00);
                                        out.push(char::from_u32(code).unwrap());
                                        continue;
                                    }
                                    self.i = save;
                                }
                                return Err(ParseError("lone surrogate in string".into()));
                            }
                            if (0xdc00..0xe000).contains(&first) {
                                return Err(ParseError("lone surrogate in string".into()));
                            }
                            out.push(char::from_u32(first).unwrap());
                        }
                        _ => return Err(self.err("Invalid \\escape")),
                    }
                }
                c if c < 0x20 => return Err(self.err("Invalid control character")),
                _ => {
                    let run = self.i;
                    while self.i < b.len() && b[self.i] != b'"' && b[self.i] != b'\\' && b[self.i] >= 0x20 {
                        self.i += 1;
                    }
                    out.push_str(unsafe { std::str::from_utf8_unchecked(&b[run..self.i]) });
                }
            }
        }
    }
    fn hex4(&mut self) -> Result<u32, ParseError> {
        if self.i + 4 > self.b.len() {
            return Err(self.err("Invalid \\uXXXX escape"));
        }
        let s = std::str::from_utf8(&self.b[self.i..self.i + 4]).map_err(|_| self.err("Invalid \\uXXXX escape"))?;
        if !s.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(self.err("Invalid \\uXXXX escape"));
        }
        self.i += 4;
        Ok(u32::from_str_radix(s, 16).unwrap())
    }
    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ParseError("JSON nesting is too deep".into()));
        }
        Ok(())
    }
    fn object(&mut self) -> Result<Value, ParseError> {
        self.enter()?;
        self.i += 1;
        let mut map = Map::new();
        let mut seen: Option<std::collections::HashSet<String>> = None;
        self.ws();
        if self.i < self.b.len() && self.b[self.i] == b'}' {
            self.i += 1;
            self.depth -= 1;
            return Ok(Value::Obj(map));
        }
        loop {
            if self.i >= self.b.len() || self.b[self.i] != b'"' {
                return Err(self.err("Expecting property name enclosed in double quotes"));
            }
            let key = self.string()?;
            self.ws();
            if self.i >= self.b.len() || self.b[self.i] != b':' {
                return Err(self.err("Expecting ':' delimiter"));
            }
            self.i += 1;
            self.ws();
            let value = self.value()?;
            // Duplicate detection: linear for small objects, a set for large ones.
            let duplicate = if map.len() < 16 {
                map.contains_key(&key)
            } else {
                let set = seen.get_or_insert_with(|| map.keys().cloned().collect());
                !set.insert(key.clone())
            };
            if duplicate {
                if self.opt.strict {
                    return Err(ParseError(format!("duplicate JSON key: {key:?}")));
                }
                map.insert(key, value);
            } else {
                if map.len() >= 16 && seen.is_none() {
                    let mut set: std::collections::HashSet<String> = map.keys().cloned().collect();
                    set.insert(key.clone());
                    seen = Some(set);
                }
                if self.canonical && map.0.last().is_some_and(|(k, _)| k.as_bytes() >= key.as_bytes()) {
                    self.canonical = false;
                }
                map.push(key, value);
            }
            self.ws();
            if self.i >= self.b.len() {
                return Err(self.err("Expecting ',' delimiter"));
            }
            match self.b[self.i] {
                b',' => {
                    self.i += 1;
                    self.ws();
                }
                b'}' => {
                    self.i += 1;
                    self.depth -= 1;
                    return Ok(Value::Obj(map));
                }
                _ => return Err(self.err("Expecting ',' delimiter")),
            }
        }
    }
    fn array(&mut self) -> Result<Value, ParseError> {
        self.enter()?;
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.i < self.b.len() && self.b[self.i] == b']' {
            self.i += 1;
            self.depth -= 1;
            return Ok(Value::Arr(items));
        }
        loop {
            self.ws();
            items.push(self.value()?);
            self.ws();
            if self.i >= self.b.len() {
                return Err(self.err("Expecting ',' delimiter"));
            }
            match self.b[self.i] {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    self.depth -= 1;
                    return Ok(Value::Arr(items));
                }
                _ => return Err(self.err("Expecting ',' delimiter")),
            }
        }
    }
}

// ---------------------------------------------------------------- encoding

#[derive(Clone, Copy)]
pub struct Style {
    pub sort_keys: bool,
    pub ensure_ascii: bool,
    /// `(',', ':')` when true, else `(', ', ': ')`.
    pub compact: bool,
    pub allow_nan: bool,
}

/// `json.dumps(sort_keys=True, separators=(',',':'), ensure_ascii=False, allow_nan=False)`.
pub const CANONICAL: Style = Style { sort_keys: true, ensure_ascii: false, compact: true, allow_nan: false };
/// `json.dumps(value)` with every default.
pub const DEFAULT: Style = Style { sort_keys: false, ensure_ascii: true, compact: false, allow_nan: true };
/// `json.dumps(value, sort_keys=True, ensure_ascii=False)` (CLI output).
pub const CLI: Style = Style { sort_keys: true, ensure_ascii: false, compact: false, allow_nan: true };
/// `json.dumps(value, ensure_ascii=False)` (viewer output).
pub const VIEWER: Style = Style { sort_keys: false, ensure_ascii: false, compact: false, allow_nan: true };

#[derive(Debug)]
pub struct EncodeError(pub String);

pub fn canonical(v: &Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    encode_into(v, CANONICAL, &mut out).expect("canonical encoding of a validated value");
    out
}

pub fn try_canonical(v: &Value) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::with_capacity(256);
    encode_into(v, CANONICAL, &mut out)?;
    Ok(out)
}

pub fn dumps(v: &Value, style: Style) -> String {
    let mut out = Vec::with_capacity(256);
    encode_into(v, style, &mut out).expect("encoding");
    String::from_utf8(out).unwrap()
}

pub fn encode_into(v: &Value, style: Style, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Int(i) => {
            let mut buf = itoa_buf(*i);
            out.extend_from_slice(buf.as_bytes_mut());
        }
        Value::Big(s) => out.extend_from_slice(s.as_bytes()),
        Value::Float(f) => {
            if !f.is_finite() {
                if !style.allow_nan {
                    return Err(EncodeError(format!("Out of range float values are not JSON compliant: {}", float_repr(*f))));
                }
                out.extend_from_slice(if f.is_nan() { b"NaN" as &[u8] } else if *f > 0.0 { b"Infinity" } else { b"-Infinity" });
            } else {
                out.extend_from_slice(float_repr(*f).as_bytes());
            }
        }
        Value::Str(s) => encode_str(s, style.ensure_ascii, out),
        Value::Arr(a) => {
            out.push(b'[');
            for (n, item) in a.iter().enumerate() {
                if n > 0 {
                    out.extend_from_slice(if style.compact { b"," } else { b", " });
                }
                encode_into(item, style, out)?;
            }
            out.push(b']');
        }
        Value::Obj(m) => {
            out.push(b'{');
            let write = |n: usize, k: &String, val: &Value, out: &mut Vec<u8>| -> Result<(), EncodeError> {
                if n > 0 {
                    out.extend_from_slice(if style.compact { b"," } else { b", " });
                }
                encode_str(k, style.ensure_ascii, out);
                out.extend_from_slice(if style.compact { b":" } else { b": " });
                encode_into(val, style, out)
            };
            if style.sort_keys && m.len() > 1 {
                let mut entries: Vec<&(String, Value)> = m.0.iter().collect();
                entries.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                for (n, (k, val)) in entries.into_iter().enumerate() {
                    write(n, k, val, out)?;
                }
            } else {
                for (n, (k, val)) in m.0.iter().enumerate() {
                    write(n, k, val, out)?;
                }
            }
            out.push(b'}');
        }
    }
    Ok(())
}

struct ItoaBuf {
    buf: [u8; 24],
    start: usize,
}
impl ItoaBuf {
    fn as_bytes_mut(&mut self) -> &[u8] {
        &self.buf[self.start..]
    }
}
fn itoa_buf(i: i64) -> ItoaBuf {
    let mut buf = [0u8; 24];
    let mut pos = 24;
    let neg = i < 0;
    let mut n = (i as i128).unsigned_abs();
    loop {
        pos -= 1;
        buf[pos] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    if neg {
        pos -= 1;
        buf[pos] = b'-';
    }
    ItoaBuf { buf, start: pos }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

pub fn encode_str(s: &str, ensure_ascii: bool, out: &mut Vec<u8>) {
    out.push(b'"');
    let bytes = s.as_bytes();
    let mut run = 0;
    if !ensure_ascii {
        for (i, &c) in bytes.iter().enumerate() {
            let esc: &[u8] = match c {
                b'"' => b"\\\"",
                b'\\' => b"\\\\",
                b'\n' => b"\\n",
                b'\r' => b"\\r",
                b'\t' => b"\\t",
                0x08 => b"\\b",
                0x0c => b"\\f",
                c if c < 0x20 => {
                    out.extend_from_slice(&bytes[run..i]);
                    out.extend_from_slice(&[b'\\', b'u', b'0', b'0', HEX[(c >> 4) as usize], HEX[(c & 15) as usize]]);
                    run = i + 1;
                    continue;
                }
                _ => continue,
            };
            out.extend_from_slice(&bytes[run..i]);
            out.extend_from_slice(esc);
            run = i + 1;
        }
        out.extend_from_slice(&bytes[run..]);
    } else {
        for ch in s.chars() {
            match ch {
                '"' => out.extend_from_slice(b"\\\""),
                '\\' => out.extend_from_slice(b"\\\\"),
                '\n' => out.extend_from_slice(b"\\n"),
                '\r' => out.extend_from_slice(b"\\r"),
                '\t' => out.extend_from_slice(b"\\t"),
                '\u{8}' => out.extend_from_slice(b"\\b"),
                '\u{c}' => out.extend_from_slice(b"\\f"),
                ' '..='~' => out.push(ch as u8),
                _ => {
                    let mut units = [0u16; 2];
                    for unit in ch.encode_utf16(&mut units) {
                        let u = *unit;
                        out.extend_from_slice(b"\\u");
                        for shift in [12, 8, 4, 0] {
                            out.push(HEX[((u >> shift) & 15) as usize]);
                        }
                    }
                }
            }
        }
    }
    out.push(b'"');
}

/// Python's `repr(float)` for finite values.
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                out.extend(std::iter::repeat('0').take(int_len - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            out.push_str("0.");
            out.extend(std::iter::repeat('0').take((-exp - 1) as usize));
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    out
}

/// `_validate_json_value`: payloads may not hold non-finite numbers.
pub fn all_finite(v: &Value) -> bool {
    match v {
        Value::Float(f) => f.is_finite(),
        Value::Arr(a) => a.iter().all(all_finite),
        Value::Obj(m) => m.iter().all(|(_, v)| all_finite(v)),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_match_python_repr() {
        let cases = [
            (1e16, "1e+16"),
            (1.5e-5, "1.5e-05"),
            (123.0, "123.0"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (0.1, "0.1"),
            (-2.5, "-2.5"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (123456789.123, "123456789.123"),
        ];
        for (x, want) in cases {
            assert_eq!(float_repr(x), want);
        }
    }

    #[test]
    fn canonical_escapes() {
        let v = Value::Str("a\"\\\n\u{1}\u{7f}\u{2028}é😀".into());
        assert_eq!(String::from_utf8(canonical(&v)).unwrap(), "\"a\\\"\\\\\\n\\u0001\u{7f}\u{2028}é😀\"");
        assert_eq!(dumps(&v, DEFAULT), "\"a\\\"\\\\\\n\\u0001\\u007f\\u2028\\u00e9\\ud83d\\ude00\"");
    }

    #[test]
    fn strict_parse() {
        assert!(parse(b"{\"a\":1,\"a\":2}", STRICT).is_err());
        assert!(parse(b"NaN", STRICT).is_err());
        assert!(parse(b"\"\\ud800\"", STRICT).is_err());
        assert!(matches!(parse(b"-0", STRICT).unwrap(), Value::Int(0)));
        assert!(matches!(parse(b"123456789012345678901234", STRICT).unwrap(), Value::Big(_)));
        let v = parse(b" {\"b\": [1, 2.5, \"\\ud83d\\ude00\"], \"a\": null} ", STRICT).unwrap();
        assert_eq!(String::from_utf8(canonical(&v)).unwrap(), "{\"a\":null,\"b\":[1,2.5,\"😀\"]}");
    }
}
