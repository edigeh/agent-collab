//! Deterministic board transitions, ported line by line from `state.py`.
//!
//! The state is eight tables of JSON entities. Tables are shared copy-on-write
//! (`Rc`) so cloning a state is cheap, and each table is decoded from the saved
//! snapshot only when first touched.

use crate::error::{reject, require, Error, Result};
use crate::json::{self, Map, Value, NULL};
use crate::pyfmt::{self, Ts};
use indexmap::IndexMap;
use std::cell::OnceCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

pub const TABLES: [&str; 8] = ["projects", "sessions", "posts", "tasks", "deliveries", "notices", "leases", "artifact_removals"];
pub const HARNESSES: [&str; 5] = ["codex", "claude", "pi", "omp", "human"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum T {
    Projects = 0,
    Sessions = 1,
    Posts = 2,
    Tasks = 3,
    Deliveries = 4,
    Notices = 5,
    Leases = 6,
    ArtifactRemovals = 7,
}

impl T {
    pub const ALL: [T; 8] = [T::Projects, T::Sessions, T::Posts, T::Tasks, T::Deliveries, T::Notices, T::Leases, T::ArtifactRemovals];
    pub fn name(self) -> &'static str {
        TABLES[self as usize]
    }
    pub fn from_kind(kind: &str) -> Option<T> {
        match kind {
            "post" => Some(T::Posts),
            "task" => Some(T::Tasks),
            "notice" => Some(T::Notices),
            _ => None,
        }
    }
}

/// Encoded bytes of one table (or one entity) inside a loaded snapshot.
#[derive(Clone)]
pub struct Raw {
    pub data: Rc<crate::snapshot::Bytes>,
    pub start: usize,
    pub end: usize,
}

impl Raw {
    pub fn bytes(&self) -> &[u8] {
        &self.data[self.start..self.end]
    }
}

/// One entity: decoded from its snapshot bytes on first use. While `raw` is
/// set the entity is unchanged since loading and those bytes are its encoding.
#[derive(Clone)]
pub struct Entry {
    cell: OnceCell<Rc<Value>>,
    raw: Option<Raw>,
}

impl Entry {
    pub fn new(value: Value) -> Entry {
        Entry { cell: OnceCell::from(Rc::new(value)), raw: None }
    }
    pub fn from_raw(raw: Raw) -> Entry {
        Entry { cell: OnceCell::new(), raw: Some(raw) }
    }
    pub fn value(&self) -> &Value {
        self.cell.get_or_init(|| {
            let raw = self.raw.as_ref().expect("entity has neither value nor bytes");
            Rc::new(crate::snapshot::decode_value(raw.bytes()).expect("saved view entity passed its MAC but does not decode"))
        })
    }
    pub fn value_mut(&mut self) -> &mut Value {
        self.value();
        self.raw = None;
        Rc::make_mut(self.cell.get_mut().unwrap())
    }
    /// One top-level field, read from the saved bytes without decoding the
    /// rest of the entity when it has not been decoded yet.
    pub fn field(&self, key: &str) -> Value {
        if let (None, Some(raw)) = (self.cell.get(), &self.raw) {
            if let Some(found) = crate::snapshot::peek_field(raw.bytes(), key) {
                return found.unwrap_or(Value::Null);
            }
        }
        self.value().get(key).cloned().unwrap_or(Value::Null)
    }
    /// The snapshot encoding, when the entity is unchanged since loading.
    pub fn raw(&self) -> Option<&[u8]> {
        self.raw.as_ref().map(Raw::bytes)
    }
}

/// A table of entities keyed by ID, in insertion order (like a Python dict).
#[derive(Clone, Default)]
pub struct Table {
    map: IndexMap<String, Entry, pyfmt::FxBuild>,
}

impl Table {
    pub fn with_capacity(n: usize) -> Table {
        Table { map: IndexMap::with_capacity_and_hasher(n, Default::default()) }
    }
    pub fn push_entry(&mut self, key: String, entry: Entry) {
        self.map.insert(key, entry);
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.map.get(key).map(Entry::value)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.map.get_mut(key).map(Entry::value_mut)
    }
    pub fn contains_key(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }
    pub fn insert(&mut self, key: String, value: Value) {
        self.map.insert(key, Entry::new(value));
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.map.keys()
    }
    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.map.values().map(Entry::value)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.map.iter().map(|(k, e)| (k, e.value()))
    }
    pub fn entries(&self) -> impl Iterator<Item = (&String, &Entry)> {
        self.map.iter()
    }
}

impl std::ops::Index<&str> for Table {
    type Output = Value;
    fn index(&self, key: &str) -> &Value {
        self.get(key).expect("no such entity")
    }
}

#[derive(Clone, Default)]
struct Slot {
    cell: OnceCell<Rc<Table>>,
    raw: Option<Raw>,
}

#[derive(Clone)]
pub struct State {
    slots: [Slot; 8],
    /// post id -> actors it was delivered to; derived from deliveries on first use.
    recipients: Option<Rc<HashMap<String, BTreeSet<String>, pyfmt::FxBuild>>>,
    /// The saved index, decoded on first use.
    recipients_raw: Option<Raw>,
    /// Summary post id -> the post ids it references (kind and refs never
    /// change after a post is created), so `invalidate` need not scan posts.
    summaries: OnceCell<Rc<Summaries>>,
    summaries_raw: Option<Raw>,
    /// The reverse of `summaries`: post id -> summary posts that reference it.
    cited: OnceCell<Rc<HashMap<String, Vec<String>, pyfmt::FxBuild>>>,
}

pub type Summaries = IndexMap<String, Vec<String>, pyfmt::FxBuild>;

pub type Recipients = HashMap<String, BTreeSet<String>, pyfmt::FxBuild>;

impl Drop for State {
    fn drop(&mut self) {
        // A short-lived CLI process leaves its tables for the OS to reclaim.
        if crate::forgetting() {
            for slot in self.slots.iter_mut() {
                if let Some(table) = slot.cell.take() {
                    std::mem::forget(table);
                }
                if let Some(raw) = slot.raw.take() {
                    std::mem::forget(raw);
                }
            }
            if let Some(index) = self.recipients.take() {
                std::mem::forget(index);
            }
            if let Some(index) = self.summaries.take() {
                std::mem::forget(index);
            }
            if let Some(index) = self.cited.take() {
                std::mem::forget(index);
            }
        }
    }
}

impl Default for State {
    fn default() -> Self {
        State::empty()
    }
}

impl State {
    pub fn empty() -> State {
        let mut s = State { slots: Default::default(), recipients: None, recipients_raw: None, summaries: OnceCell::from(Rc::new(Summaries::default())), summaries_raw: None, cited: OnceCell::new() };
        for slot in s.slots.iter_mut() {
            let _ = slot.cell.set(Rc::new(Table::default()));
        }
        s.insert(T::Projects, "global", crate::obj! {"id" => "global", "name" => "Shared system", "roots" => Vec::<Value>::new(), "revision" => 1i64});
        s
    }

    pub fn from_raw(raws: Vec<Raw>, recipients: Raw, summaries: Raw) -> State {
        let mut s = State { slots: Default::default(), recipients: None, recipients_raw: Some(recipients), summaries: OnceCell::new(), summaries_raw: Some(summaries), cited: OnceCell::new() };
        for (slot, raw) in s.slots.iter_mut().zip(raws) {
            slot.raw = Some(raw);
        }
        s
    }

    /// The post -> recipients index as the snapshot stores it.
    pub fn recipients_for_snapshot(&mut self) -> &Recipients {
        self.recipient_index()
    }

    pub fn table(&self, t: T) -> &Table {
        let slot = &self.slots[t as usize];
        slot.cell.get_or_init(|| {
            let raw = slot.raw.as_ref().expect("table has neither data nor bytes");
            Rc::new(crate::snapshot::decode_table(raw).expect("saved view section passed its MAC but does not decode"))
        })
    }

    pub fn table_mut(&mut self, t: T) -> &mut Table {
        self.table(t);
        let slot = &mut self.slots[t as usize];
        Rc::make_mut(slot.cell.get_mut().unwrap())
    }

    pub fn get(&self, t: T, key: &str) -> Option<&Value> {
        self.table(t).get(key)
    }

    pub fn contains(&self, t: T, key: &str) -> bool {
        self.table(t).contains_key(key)
    }

    pub fn obj_mut(&mut self, t: T, key: &str) -> Option<&mut Map> {
        self.table_mut(t).get_mut(key)?.as_obj_mut()
    }

    pub fn insert(&mut self, t: T, key: &str, value: Value) {
        if t == T::Posts {
            self.summaries();
            let index = Rc::make_mut(self.summaries.get_mut().unwrap());
            let old = if value.at("kind").as_str() == Some("summary") {
                let refs: Vec<String> = value.at("refs").as_arr().map(|a| a.iter().filter_map(|r| r.at("id").as_str().map(str::to_string)).collect()).unwrap_or_default();
                let old = index.insert(key.to_string(), refs.clone());
                Some((old, refs))
            } else {
                index.shift_remove(key).map(|old| (Some(old), Vec::new()))
            };
            if let (Some((old, new)), Some(cited)) = (old, self.cited.get_mut()) {
                let cited = Rc::make_mut(cited);
                for r in old.unwrap_or_default() {
                    if let Some(by) = cited.get_mut(&r) {
                        by.retain(|id| id != key);
                    }
                }
                for r in new {
                    cited.entry(r).or_default().push(key.to_string());
                }
            }
        }
        self.table_mut(t).insert(key.to_string(), value);
    }

    /// Summary posts that reference post `id`.
    pub fn cited_by(&self, id: &str) -> &[String] {
        let cited = self.cited.get_or_init(|| {
            let mut cited: HashMap<String, Vec<String>, pyfmt::FxBuild> = HashMap::default();
            for (summary, refs) in self.summaries() {
                for r in refs {
                    cited.entry(r.clone()).or_default().push(summary.clone());
                }
            }
            Rc::new(cited)
        });
        cited.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn summaries(&self) -> &Summaries {
        self.summaries.get_or_init(|| {
            let raw = self.summaries_raw.as_ref().expect("summary index has neither data nor bytes");
            Rc::new(crate::snapshot::decode_summaries(raw.bytes()).expect("saved view section passed its MAC but does not decode"))
        })
    }

    pub fn values(&self, t: T) -> impl Iterator<Item = &Value> {
        self.table(t).values()
    }

    /// The entities whose `field` satisfies `keep`, decoding only those.
    pub fn values_where<'a>(&'a self, t: T, field: &'a str, keep: impl Fn(&Value) -> bool + 'a) -> impl Iterator<Item = &'a Value> + 'a {
        self.table(t).entries().filter(move |(_, e)| keep(&e.field(field))).map(|(_, e)| e.value())
    }

    pub fn len(&self, t: T) -> usize {
        self.table(t).len()
    }

    /// The whole state as one JSON object (tables in Python's order).
    pub fn to_value(&self) -> Value {
        let mut m = Map::with_capacity(8);
        for t in T::ALL {
            let mut table = Map::with_capacity(self.len(t));
            for (k, v) in self.table(t).iter() {
                table.push(k.clone(), v.clone());
            }
            m.push(t.name(), Value::Obj(table));
        }
        Value::Obj(m)
    }

    pub fn table_value(&self, t: T) -> Value {
        let mut table = Map::with_capacity(self.len(t));
        for (k, v) in self.table(t).iter() {
            table.push(k.clone(), v.clone());
        }
        Value::Obj(table)
    }

    /// `canonical(state)` without materialising one big value.
    pub fn encode_canonical(&self, out: &mut Vec<u8>) {
        let mut names: Vec<T> = T::ALL.to_vec();
        names.sort_by_key(|t| t.name());
        out.push(b'{');
        for (n, t) in names.into_iter().enumerate() {
            if n > 0 {
                out.push(b',');
            }
            json::encode_str(t.name(), false, out);
            out.push(b':');
            out.push(b'{');
            let table = self.table(t);
            let mut keys: Vec<&String> = table.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                json::encode_str(k, false, out);
                out.push(b':');
                json::encode_into(&table[k.as_str()], json::CANONICAL, out).expect("finite state values");
            }
            out.push(b'}');
        }
        out.push(b'}');
    }

    fn recipient_index(&mut self) -> &HashMap<String, BTreeSet<String>, pyfmt::FxBuild> {
        if self.recipients.is_none() {
            if let Some(raw) = self.recipients_raw.take() {
                let index = crate::snapshot::decode_recipients(&raw.data[raw.start..raw.end]).expect("saved view section passed its MAC but does not decode");
                self.recipients = Some(Rc::new(index));
                return self.recipients.as_ref().unwrap();
            }
            let mut index: HashMap<String, BTreeSet<String>, pyfmt::FxBuild> = HashMap::default();
            for d in self.values(T::Deliveries) {
                let actor = d.at("actor").as_str().unwrap_or("").to_string();
                for item in d.at("items").as_arr().map(|a| a.as_slice()).unwrap_or(&[]) {
                    if item.at("kind").as_str() == Some("post") {
                        if let Some(id) = item.at("id").as_str() {
                            index.entry(id.to_string()).or_default().insert(actor.clone());
                        }
                    }
                }
            }
            self.recipients = Some(Rc::new(index));
        }
        self.recipients.as_ref().unwrap()
    }

    fn note_delivery(&mut self, actor: &str, items: &[Value]) {
        if let Some(index) = self.recipients.as_mut() {
            let index = Rc::make_mut(index);
            for item in items {
                if item.at("kind").as_str() == Some("post") {
                    if let Some(id) = item.at("id").as_str() {
                        index.entry(id.to_string()).or_default().insert(actor.to_string());
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- helpers

pub fn is_identifier(s: &str) -> bool {
    !s.is_empty() && s.len() <= 160 && s.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b':' | b'-'))
}

pub fn identifier(value: &Value) -> Result<&str> {
    match value {
        Value::Str(s) if is_identifier(s) => Ok(s),
        _ => reject("invalid identifier"),
    }
}

pub fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn text<'a>(value: &'a Value, field: &str, limit: usize) -> Result<&'a str> {
    match value {
        Value::Str(s) if pyfmt::has_text(s) && s.len() <= limit => Ok(s),
        _ => Err(Error::Rejected(format!("{field} must be nonempty text of at most {limit} bytes"))),
    }
}

pub fn text_default(value: &Value) -> Result<&str> {
    text(value, "text", 32000)
}

pub fn timestamp(value: &Value) -> Result<Ts> {
    match value {
        Value::Str(s) => Ts::parse(s).ok_or_else(|| Error::Rejected("invalid timestamp".into())),
        _ => reject("invalid timestamp"),
    }
}

pub fn timestamp_str(value: &str) -> Result<Ts> {
    Ts::parse(value).ok_or_else(|| Error::Rejected("invalid timestamp".into()))
}

/// `entity(state, table, key)`: the key, once it names an existing entity.
pub fn entity<'a>(state: &State, t: T, key: &'a Value) -> Result<&'a str> {
    let Value::Str(k) = key else {
        return Err(Error::Rejected(format!("{} identifier must be text", t.name())));
    };
    if !state.contains(t, k) {
        return Err(Error::Rejected(format!("unknown {}: {}", t.name(), k)));
    }
    Ok(k)
}

fn revision(item: &Value) -> &Value {
    item.at("revision")
}

pub fn expected(item: &Value, data: &Value) -> Result<()> {
    let ok = data.at("expected").is_int() && revision(item).py_eq(data.at("expected"));
    if ok {
        Ok(())
    } else {
        Err(Error::Rejected(format!("revision conflict: current revision is {}", py_str(revision(item)))))
    }
}

/// `str(value)` for scalars as Python prints them.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Int(i) => i.to_string(),
        Value::Big(s) => s.to_string(),
        Value::Float(f) => json::float_repr(*f),
        Value::Str(s) => s.clone(),
        other => py_repr(other),
    }
}

/// `repr(value)` for JSON values.
pub fn py_repr(v: &Value) -> String {
    match v {
        Value::Str(s) => {
            let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
            let mut out = String::new();
            out.push(quote);
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c == quote => {
                        out.push('\\');
                        out.push(c);
                    }
                    c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
                    c => out.push(c),
                }
            }
            out.push(quote);
            out
        }
        Value::Arr(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Obj(m) => format!("{{{}}}", m.iter().map(|(k, v)| format!("{}: {}", py_repr(&Value::Str(k.clone())), py_repr(v))).collect::<Vec<_>>().join(", ")),
        other => py_str(other),
    }
}

pub fn evidence(value: &Value) -> Result<Value> {
    let ok_list = matches!(value, Value::Arr(a) if !a.is_empty() && a.len() <= 32);
    require(ok_list, "captured evidence is required")?;
    for r in value.as_arr().unwrap() {
        let ok = r.is_obj() && is_hash(&py_str(r.get("sha256").unwrap_or(&Value::Str(String::new())))) && r.at("source").is_str();
        require(ok, "evidence requires source and SHA-256")?;
    }
    Ok(value.clone())
}

pub fn artifact_hashes(value: &Value, found: &mut HashSet<String>) {
    match value {
        Value::Obj(m) => {
            if let Some(Value::Str(s)) = m.get("sha256") {
                found.insert(s.clone());
            }
            for (_, v) in m.iter() {
                artifact_hashes(v, found);
            }
        }
        Value::Arr(a) => {
            for v in a {
                artifact_hashes(v, found);
            }
        }
        _ => {}
    }
}

fn contains_hash(value: &Value, sha: &str) -> bool {
    match value {
        Value::Obj(m) => {
            if let Some(Value::Str(s)) = m.get("sha256") {
                if s == sha {
                    return true;
                }
            }
            m.iter().any(|(_, v)| contains_hash(v, sha))
        }
        Value::Arr(a) => a.iter().any(|v| contains_hash(v, sha)),
        _ => false,
    }
}

fn rstrip_slash(s: &str) -> &str {
    s.trim_end_matches('/')
}

pub fn overlaps<'a>(left: impl IntoIterator<Item = &'a str> + Clone, right: impl IntoIterator<Item = &'a str> + Clone) -> bool {
    for a in left {
        for b in right.clone() {
            if a == b || pyfmt::fnmatchcase(a, b) || pyfmt::fnmatchcase(b, a) {
                return true;
            }
            let (ra, rb) = (rstrip_slash(a), rstrip_slash(b));
            if (ra.starts_with(rb) && ra[rb.len()..].starts_with('/')) || (rb.starts_with(ra) && rb[ra.len()..].starts_with('/')) {
                return true;
            }
        }
    }
    false
}

fn strs(v: &Value) -> Vec<&str> {
    v.as_arr().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default()
}

pub fn scope(value: &Value) -> Result<Vec<String>> {
    let ok = matches!(value, Value::Arr(a) if a.len() <= 64);
    require(ok, "scope must be a list of at most 64 paths/resources")?;
    let mut set = BTreeSet::new();
    for x in value.as_arr().unwrap() {
        set.insert(text(x, "scope", 1024)?.to_string());
    }
    Ok(set.into_iter().collect())
}

fn str_list(items: Vec<String>) -> Value {
    Value::Arr(items.into_iter().map(Value::Str).collect())
}

/// A declared presence: what the session is doing, a summary, and shared resources it holds.
pub fn presence_status(value: &Value, recorded_at: &str) -> Result<Value> {
    let ok = matches!(value, Value::Obj(m) if m.keys().all(|k| k == "doing" || k == "summary" || k == "uses"));
    require(ok, "status takes doing, summary and optional uses")?;
    let empty = Value::Arr(Vec::new());
    let uses = value.get("uses").unwrap_or(&empty);
    require(matches!(uses, Value::Arr(a) if a.len() <= 4), "uses lists at most 4 resources")?;
    let doing = text(value.at("doing"), "doing", 80)?;
    let summary = text(value.at("summary"), "summary", 280)?;
    let mut item = Map::new();
    item.push("doing", doing);
    item.push("summary", summary);
    let mut names = BTreeSet::new();
    for u in uses.as_arr().unwrap() {
        names.insert(pyfmt::split_join(&text(u, "resource", 48)?.to_lowercase()));
    }
    if !names.is_empty() {
        item.push("uses", str_list(names.into_iter().collect()));
    }
    item.push("at", recorded_at);
    Ok(Value::Obj(item))
}

pub fn presence_host(value: &Value, recorded_at: &str) -> Result<Value> {
    let ok = matches!(value.get("pid"), Some(Value::Int(p)) if *p > 1 && *p < (1i64 << 31));
    require(value.is_obj() && ok, "host needs a process ID")?;
    let started = text(value.at("started"), "started", 64)?;
    Ok(crate::obj! {"pid" => value.at("pid").clone(), "started" => started, "at" => recorded_at})
}

pub fn may_manage(state: &State, actor: &str, owner: &Value) -> bool {
    let mut owner = owner.clone();
    let mut steps = 0;
    while owner.truthy() {
        if let Value::Str(o) = &owner {
            if o == actor {
                return true;
            }
            owner = state.get(T::Sessions, o).map(|s| s.at("parent").clone()).unwrap_or(Value::Null);
        } else {
            return false;
        }
        steps += 1;
        if steps > 100_000 {
            return false;
        }
    }
    false
}

pub fn notice_id(nid: &str, recipient: &Value, project: &Value, kind: &str, target: &Value) -> String {
    let list = Value::Arr(vec![Value::str(nid), recipient.clone(), project.clone(), Value::str(kind), target.clone()]);
    "n-".to_string() + &pyfmt::sha256_hex(json::dumps(&list, json::DEFAULT).as_bytes())
}

pub fn notice(state: &mut State, nid: &str, recipient: &Value, project: &Value, kind: &str, target: &Value, message: String) -> Result<()> {
    let id = notice_id(nid, recipient, project, kind, target);
    require(!state.contains(T::Notices, &id), "derived notice ID collision")?;
    let item = crate::obj! {
        "id" => id.clone(), "recipient" => recipient.clone(), "project" => project.clone(), "kind" => kind,
        "target" => target.clone(), "text" => message, "acked" => false, "revision" => 1i64,
    };
    state.insert(T::Notices, &id, item);
    Ok(())
}

fn bump(m: &mut Map, key: &str) {
    if let Some(Value::Int(i)) = m.get_mut(key) {
        *i += 1;
    }
}

pub fn invalidate(state: &mut State, changed: &BTreeSet<String>, event_id: &str) -> Result<BTreeSet<String>> {
    // Every summary that references an affected post, transitively.
    let mut affected: BTreeSet<String> = changed.clone();
    let mut pending: Vec<String> = changed.iter().cloned().collect();
    while let Some(id) = pending.pop() {
        for summary in state.cited_by(&id) {
            if affected.insert(summary.clone()) {
                pending.push(summary.clone());
            }
        }
    }
    for pid in affected.difference(changed) {
        if let Some(p) = state.obj_mut(T::Posts, pid) {
            p.insert("stale", Value::Bool(true));
            bump(p, "revision");
        }
    }
    for pid in &affected {
        let project = state.get(T::Posts, pid).map(|p| p.at("project").clone()).unwrap_or(Value::Null);
        let actors: Vec<String> = state.recipient_index().get(pid.as_str()).map(|s| s.iter().cloned().collect()).unwrap_or_default();
        for actor in actors {
            notice(
                state,
                &format!("{event_id}:{pid}:{actor}"),
                &Value::Str(actor.clone()),
                &project,
                "correction",
                &Value::Str(pid.clone()),
                format!("Information changed: {pid}; read its current revision before relying on it."),
            )?;
        }
    }
    Ok(affected)
}

const LIST_FIELDS: [&str; 6] = ["items", "refs", "posts", "outcomes", "evidence", "source_fixes"];

pub fn validate_request(request: &Value) -> Result<()> {
    require(request.is_obj(), "request must be an object")?;
    identifier(request.at("id"))?;
    identifier(request.at("actor"))?;
    require(request.at("op").is_str(), "operation must be text")?;
    let d = request.at("data");
    require(d.is_obj(), "data must be an object")?;
    let op = request.at("op").as_str().unwrap();
    if op == "artifact.remove.plan" {
        require(matches!(d.at("sha256"), Value::Str(s) if is_hash(s)), "invalid artifact hash")?;
        require(matches!(d.at("token"), Value::Str(s) if is_hash(s)), "cleanup requires a preview token")?;
        require(d.at("preview").is_obj(), "cleanup requires a preview object")?;
    } else if op == "artifact.remove.finish" {
        identifier(d.at("removal"))?;
    }
    if let Some(observed) = request.get("observed_at") {
        timestamp(observed)?;
    }
    for field in LIST_FIELDS {
        if let Some(v) = d.get(field) {
            require(v.is_arr(), format!("{field} must be a list"))?;
            require(v.as_arr().unwrap().iter().all(|x| x.is_obj()), format!("{field} entries must be objects"))?;
        }
    }
    for field in ["roots", "scope", "dependencies"] {
        if let Some(v) = d.get(field) {
            let ok = matches!(v, Value::Arr(a) if a.iter().all(|x| x.is_str()));
            require(ok, format!("{field} must contain strings"))?;
        }
    }
    if let Some(Value::Arr(outcomes)) = d.get("outcomes") {
        for outcome in outcomes {
            identifier(outcome.at("post"))?;
            for field in ["evidence", "source_fixes"] {
                if let Some(v) = outcome.get(field) {
                    let ok = matches!(v, Value::Arr(a) if a.iter().all(|x| x.is_obj()));
                    require(ok, format!("outcome {field} must contain objects"))?;
                }
            }
        }
    }
    for field in ["posts", "refs", "items"] {
        if let Some(Value::Arr(list)) = d.get(field) {
            for r in list {
                let ok = r.at("id").is_str() && matches!(r.at("revision"), Value::Int(i) if *i > 0) || matches!(r.at("revision"), Value::Big(b) if !b.starts_with('-')) && r.at("id").is_str();
                require(ok, "references need id and positive revision")?;
            }
        }
    }
    if let Some(name) = d.get("name") {
        text(name, "name", 200)?;
    }
    Ok(())
}

pub fn record_rejection(state: &mut State, request: &Value, reason: &str) -> Result<()> {
    let actor = request.at("actor");
    let project = match actor.as_str().and_then(|a| state.get(T::Sessions, a)) {
        Some(s) => s.get("project").cloned().unwrap_or(Value::str("global")),
        None => Value::str("global"),
    };
    let id = py_str(request.at("id"));
    notice(
        state,
        &format!("rejection:{id}"),
        actor,
        &project,
        "rejection",
        request.at("id"),
        format!("Request {id} was rejected: {reason}. Reconcile the intended work explicitly."),
    )
}

fn data_or<'a>(d: &'a Value, key: &str, default: &'a Value) -> &'a Value {
    d.get(key).unwrap_or(default)
}

fn closed(status: &Value) -> bool {
    matches!(status.as_str(), Some("done") | Some("cancelled"))
}

fn empty_arr() -> Value {
    Value::Arr(Vec::new())
}

fn empty_obj() -> Value {
    Value::Obj(Map::new())
}

/// Apply one request to `s` in place and return its result. On `Err` the state
/// may be partly changed; callers apply to a copy when the outcome is unknown.
pub fn apply(s: &mut State, request: &Value, recorded_at: &str) -> Result<Value> {
    validate_request(request)?;
    let rid_v = request.at("id").clone();
    let rid = identifier(&rid_v)?.to_string();
    let actor_v = request.at("actor").clone();
    let actor = identifier(&actor_v)?.to_string();
    let op = request.at("op").as_str().unwrap_or("").to_string();
    let d = request.at("data");
    require(d.is_obj(), "data must be an object")?;
    let now = timestamp_str(recorded_at)?;
    if op != "session.register" {
        entity(s, T::Sessions, &actor_v)?;
        let here = s.obj_mut(T::Sessions, &actor).unwrap();
        here.insert("seen_at", Value::str(recorded_at));
        here.remove("left");
    }
    let mut result = Map::new();
    result.push("id", rid.clone());
    if !op.starts_with("artifact.") {
        let removing: Vec<String> = s
            .values(T::ArtifactRemovals)
            .filter(|r| r.at("status").as_str() == Some("planned"))
            .filter_map(|r| r.at("sha256").as_str().map(String::from))
            .collect();
        if !removing.is_empty() {
            let mut found = HashSet::new();
            artifact_hashes(d, &mut found);
            require(!removing.iter().any(|h| found.contains(h)), "artifact removal is pending; evidence cannot be newly referenced")?;
        }
    }
    let observed_at = request.get("observed_at").cloned().unwrap_or_else(|| Value::str(recorded_at));

    match op.as_str() {
        "session.register" => {
            require(d.at("harness").as_str().is_some_and(|h| HARNESSES.contains(&h)), "unknown harness")?;
            let prior = s.get(T::Sessions, &actor).cloned();
            let prior_project = prior.as_ref().and_then(|p| p.get("project").cloned()).unwrap_or(Value::str("global"));
            let project = data_or(d, "project", &prior_project).clone();
            entity(s, T::Projects, &project)?;
            let prior_parent = prior.as_ref().and_then(|p| p.get("parent").cloned()).unwrap_or(Value::Null);
            let parent = data_or(d, "parent", &prior_parent).clone();
            if parent.truthy() {
                entity(s, T::Sessions, &parent)?;
                require(!parent.py_eq(&actor_v), "session cannot parent itself")?;
            }
            let declared = match d.get("status") {
                Some(st) => Some(presence_status(st, recorded_at)?),
                None => None,
            };
            let process = match d.get("host") {
                Some(h) => Some(presence_host(h, recorded_at)?),
                None => None,
            };
            if let Some(item) = prior {
                require(
                    item.at("harness").py_eq(d.at("harness")) && item.at("parent").py_eq(&parent),
                    "session identity already registered with a different harness/parent",
                )?;
                let item = s.obj_mut(T::Sessions, &actor).unwrap();
                item.insert("seen_at", Value::str(recorded_at));
                item.insert("project", project);
                item.remove("left");
            } else {
                let name = data_or(d, "name", &actor_v).clone();
                s.insert(
                    T::Sessions,
                    &actor,
                    crate::obj! {
                        "id" => actor.clone(), "harness" => d.at("harness").clone(), "parent" => parent,
                        "project" => project, "name" => name, "seen_at" => recorded_at,
                        "started_at" => recorded_at, "revision" => 1i64,
                    },
                );
            }
            let item = s.obj_mut(T::Sessions, &actor).unwrap();
            if let Some(declared) = declared {
                item.insert("status", declared);
            }
            if let Some(process) = process {
                item.insert("host", process);
            }
            result.push("session", actor.clone());
        }
        "session.touch" => {
            if let Some(project) = d.get("project") {
                entity(s, T::Projects, project)?;
                s.obj_mut(T::Sessions, &actor).unwrap().insert("project", project.clone());
            }
            if let Some(st) = d.get("status") {
                let status = presence_status(st, recorded_at)?;
                s.obj_mut(T::Sessions, &actor).unwrap().insert("status", status);
            }
            require(!d.as_obj().unwrap().contains_key("summary") || matches!(d.at("leave"), Value::Bool(true)), "a touch summary is a hand-off note for leave")?;
            if let Some(leave) = d.get("leave") {
                require(matches!(leave, Value::Bool(true)), "leave must be true")?;
                let mut left = Map::new();
                left.push("at", recorded_at);
                if let Some(summary) = d.get("summary") {
                    left.push("summary", text(summary, "summary", 280)?);
                }
                s.obj_mut(T::Sessions, &actor).unwrap().insert("left", Value::Obj(left));
            }
            result.push("session", actor.clone());
        }
        "artifact.remove.plan" => {
            let sha = match d.at("sha256") {
                Value::Str(h) if is_hash(h) => h.clone(),
                _ => return reject("invalid artifact hash"),
            };
            require(
                !s.values(T::ArtifactRemovals).any(|r| r.at("sha256").as_str() == Some(&sha) && r.at("status").as_str() == Some("planned")),
                "artifact removal already pending",
            )?;
            s.insert(
                T::ArtifactRemovals,
                &rid,
                crate::obj! {
                    "id" => rid.clone(), "actor" => actor.clone(), "sha256" => sha.clone(), "status" => "planned",
                    "preview" => d.at("preview").clone(), "created_at" => recorded_at,
                },
            );
            let hit: Vec<String> = s
                .table(T::Posts)
                .iter()
                .filter(|(_, p)| contains_hash(p, &sha))
                .map(|(k, _)| k.clone())
                .collect();
            let mut changed = BTreeSet::new();
            for key in hit {
                let p = s.obj_mut(T::Posts, &key).unwrap();
                p.insert("due", Value::Bool(true));
                p.insert("urgent", Value::Bool(true));
                p.insert("status", Value::str("disputed"));
                bump(p, "revision");
                let report = crate::obj! {
                    "id" => rid.clone(), "actor" => actor.clone(),
                    "reason" => format!("Preserved evidence explicitly scheduled for removal: {sha}"),
                    "evidence" => empty_arr(), "observed_at" => recorded_at, "ingested_at" => recorded_at,
                };
                if let Some(Value::Arr(reports)) = p.get_mut("reports") {
                    reports.push(report);
                }
                changed.insert(p.get("id").and_then(|v| v.as_str()).unwrap_or(&key).to_string());
            }
            invalidate(s, &changed, &rid)?;
            result.push("removal", rid.clone());
        }
        "artifact.remove.finish" => {
            let key = entity(s, T::ArtifactRemovals, d.at("removal"))?.to_string();
            let removal = s.get(T::ArtifactRemovals, &key).unwrap();
            let ok = may_manage(s, &actor, removal.at("actor")) || s.get(T::Sessions, &actor).unwrap().at("harness").as_str() == Some("human");
            require(ok, "only responsible agent or user can resume removal")?;
            let removal = s.obj_mut(T::ArtifactRemovals, &key).unwrap();
            removal.insert("status", Value::str("removed"));
            removal.insert("finished_at", Value::str(recorded_at));
            let id = removal.get("id").cloned().unwrap_or(Value::Null);
            result.push("removal", id);
        }
        "project.create" => {
            let pid_v = data_or(d, "id", &rid_v).clone();
            let pid = identifier(&pid_v)?.to_string();
            require(!s.contains(T::Projects, &pid), "project already exists")?;
            let roots_default = empty_arr();
            let roots = data_or(d, "roots", &roots_default);
            require(roots.is_arr(), "roots must be a list")?;
            let mut set = BTreeSet::new();
            for root in roots.as_arr().unwrap() {
                let r = text(root, "root", 32000)?;
                require(r.starts_with('/'), "repository roots must be absolute")?;
                require(!s.values(T::Projects).any(|p| p.at("roots").as_arr().is_some_and(|a| a.iter().any(|x| x.py_eq(root)))), "repository already attached")?;
                set.insert(r.to_string());
            }
            let name = text(d.at("name"), "name", 200)?.to_string();
            s.insert(T::Projects, &pid, crate::obj! {"id" => pid.clone(), "name" => name, "roots" => str_list(set.into_iter().collect()), "revision" => 1i64});
            result.push("project", pid);
        }
        "project.attach" => {
            let key = entity(s, T::Projects, d.at("project"))?.to_string();
            let item = s.get(T::Projects, &key).unwrap();
            expected(item, d)?;
            let root = text(d.at("root"), "root", 32000)?.to_string();
            require(root.starts_with('/'), "root must be absolute")?;
            let item_id = item.at("id").clone();
            require(
                !s.values(T::Projects).any(|p| p.at("roots").as_arr().is_some_and(|a| a.iter().any(|x| x.as_str() == Some(&root))) && !p.at("id").py_eq(&item_id)),
                "repository already attached elsewhere",
            )?;
            let item = s.obj_mut(T::Projects, &key).unwrap();
            let mut roots: BTreeSet<String> = strs(item.get("roots").unwrap_or(&NULL)).into_iter().map(String::from).collect();
            roots.insert(root);
            item.insert("roots", str_list(roots.into_iter().collect()));
            bump(item, "revision");
            result.push("project", item_id);
        }
        "post.create" => {
            let session_project = s.get(T::Sessions, &actor).unwrap().at("project").clone();
            let project = data_or(d, "project", &session_project).clone();
            entity(s, T::Projects, &project)?;
            let message = Value::str("message");
            let kind = data_or(d, "kind", &message).clone();
            require(kind.as_str().is_some_and(|k| ["message", "finding", "request", "summary", "work"].contains(&k)), "invalid post kind")?;
            let kind = kind.as_str().unwrap().to_string();
            let recipient = d.at("to").clone();
            if recipient.truthy() {
                entity(s, T::Sessions, &recipient)?;
            }
            let refs_default = empty_arr();
            let refs = data_or(d, "refs", &refs_default).clone();
            require(refs.is_arr(), "refs must be a list")?;
            for r in refs.as_arr().unwrap() {
                let key = entity(s, T::Posts, r.at("id"))?;
                let source = s.get(T::Posts, key).unwrap();
                require(source.at("revision").py_eq(r.at("revision")) && !source.at("stale").truthy(), "summary source is stale")?;
            }
            if kind == "summary" {
                require(refs.truthy(), "summary requires source revisions")?;
            }
            require(!s.contains(T::Posts, &rid), "post already exists")?;
            let proof = if d.at("evidence").truthy() { evidence(d.at("evidence"))? } else { empty_arr() };
            let body = text_default(d.at("text"))?.to_string();
            let context = data_or(d, "context", &empty_obj()).clone();
            s.insert(
                T::Posts,
                &rid,
                crate::obj! {
                    "id" => rid.clone(), "project" => project.clone(), "actor" => actor.clone(), "kind" => kind.clone(),
                    "text" => body.clone(), "to" => recipient.clone(), "revision" => 1i64, "status" => "unverified",
                    "refs" => refs.clone(), "evidence" => proof, "created_at" => recorded_at,
                    "observed_at" => observed_at.clone(), "ingested_at" => recorded_at, "reports" => empty_arr(),
                    "due" => true, "urgent" => false, "stale" => false,
                    "moderation_generation" => 0i64, "context" => context.clone(),
                },
            );
            if kind == "request" {
                let tid = format!("{rid}:task");
                require(!s.contains(T::Tasks, &tid), "derived request task ID collision")?;
                s.insert(
                    T::Tasks,
                    &tid,
                    crate::obj! {
                        "id" => tid.clone(), "project" => project.clone(), "title" => pyfmt::prefix_chars(&body, 200),
                        "owner" => recipient.clone(), "creator" => actor.clone(), "scope" => empty_arr(), "status" => "open",
                        "revision" => 1i64, "priority" => "normal", "evidence" => empty_arr(), "source_post" => rid.clone(),
                        "correction" => false, "created_at" => recorded_at, "dependencies" => empty_arr(),
                        "context" => context,
                    },
                );
                result.push("task", tid);
            }
            if kind == "work" {
                let scope_default = empty_arr();
                let worked = scope(data_or(d, "scope", &scope_default))?;
                s.obj_mut(T::Posts, &rid).unwrap().insert("scope", str_list(worked.clone()));
                let mut pending = Vec::new();
                for task in s.values(T::Tasks) {
                    if task.at("project").py_eq(&project)
                        && !closed(task.at("status"))
                        && !task.at("owner").py_eq(&actor_v)
                        && overlaps(worked.iter().map(|x| x.as_str()), strs(task.at("scope")))
                    {
                        let owner = task.at("owner").clone();
                        let mut who: Vec<Value> = vec![actor_v.clone()];
                        if !owner.is_null() && !owner.py_eq(&actor_v) {
                            who.push(owner);
                        }
                        if who.iter().any(|w| !w.is_str()) {
                            return reject("task owner must be text");
                        }
                        who.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
                        pending.push((task.at("id").clone(), who));
                    }
                }
                for (task_id, who) in pending {
                    let tid = py_str(&task_id);
                    for recipient in who {
                        notice(
                            s,
                            &format!("{rid}:{tid}:{}", py_str(&recipient)),
                            &recipient,
                            &project,
                            "overlap",
                            &task_id,
                            format!("Reconcile work {rid} with assignment {tid}; no prior coordination is implied."),
                        )?;
                    }
                }
            }
            result.push("post", rid.clone());
        }
        "post.observe" => {
            let key = entity(s, T::Posts, d.at("post"))?.to_string();
            let p = s.get(T::Posts, &key).unwrap();
            expected(p, d)?;
            let observations = d.at("observations");
            require(matches!(observations, Value::Obj(m) if !m.is_empty() && m.len() <= 32), "source observations must be a bounded object")?;
            let proof = if d.at("evidence").truthy() { evidence(d.at("evidence"))? } else { empty_arr() };
            let moderation_evidence = p.get("moderation").and_then(|m| m.get("evidence")).filter(|e| e.truthy());
            let mut baseline = Map::new();
            if let Some(refs) = moderation_evidence.or_else(|| p.get("evidence")).and_then(|e| e.as_arr()) {
                for r in refs {
                    baseline.insert(py_str(r.at("source")), r.at("sha256").clone());
                }
            }
            let mut prior = baseline.clone();
            let prior_obs = p.get("source_observations").cloned().unwrap_or_else(empty_obj);
            if let Some(m) = prior_obs.as_obj() {
                for (k, v) in m.iter() {
                    prior.insert(k.clone(), v.clone());
                }
            }
            let obs = observations.as_obj().unwrap();
            for (source, sha) in obs.iter() {
                require(baseline.contains_key(source), "observation must reference a known source")?;
                require(sha.is_null() || matches!(sha, Value::Str(h) if is_hash(h)), "invalid observed hash")?;
                require(
                    sha.is_null()
                        || proof.as_arr().unwrap().iter().any(|r| r.at("source").as_str() == Some(source.as_str()) && r.at("sha256").py_eq(sha)),
                    "observed bytes require captured evidence",
                )?;
            }
            let mut changes: Vec<String> = obs.iter().filter(|(src, sha)| !prior.get(src).unwrap_or(&NULL).py_eq(sha)).map(|(src, _)| src.clone()).collect();
            changes.sort();
            let post = s.obj_mut(T::Posts, &key).unwrap();
            post.insert("source_checked_at", Value::str(recorded_at));
            let mut merged = prior_obs.as_obj().cloned().unwrap_or_default();
            for (k, v) in obs.iter() {
                merged.insert(k.clone(), v.clone());
            }
            post.insert("source_observations", Value::Obj(merged));
            let post_id = post.get("id").cloned().unwrap_or(Value::Null);
            if !changes.is_empty() {
                post.insert("due", Value::Bool(true));
                post.insert("urgent", Value::Bool(true));
                bump(post, "revision");
                post.insert("status", Value::str("disputed"));
                let report = crate::obj! {
                    "id" => rid.clone(), "actor" => actor.clone(),
                    "reason" => format!("Source changed or became unavailable; recheck claim: {}", changes.join(", ")),
                    "evidence" => proof, "observed_at" => observed_at.clone(), "ingested_at" => recorded_at,
                };
                if let Some(Value::Arr(reports)) = post.get_mut("reports") {
                    reports.push(report);
                }
                let mut changed = BTreeSet::new();
                changed.insert(py_str(&post_id));
                invalidate(s, &changed, &rid)?;
            }
            result.push("post", post_id);
            result.push("changed", !changes.is_empty());
        }
        "post.report" => {
            let key = entity(s, T::Posts, d.at("post"))?.to_string();
            expected(s.get(T::Posts, &key).unwrap(), d)?;
            let reason = text_default(d.at("reason"))?.to_string();
            let proof = if d.at("evidence").truthy() { evidence(d.at("evidence"))? } else { empty_arr() };
            let p = s.obj_mut(T::Posts, &key).unwrap();
            p.insert("due", Value::Bool(true));
            p.insert("urgent", Value::Bool(true));
            bump(p, "revision");
            let report = crate::obj! {
                "id" => rid.clone(), "actor" => actor.clone(), "reason" => reason, "evidence" => proof,
                "observed_at" => observed_at.clone(), "ingested_at" => recorded_at,
            };
            if let Some(Value::Arr(reports)) = p.get_mut("reports") {
                reports.push(report);
            }
            p.insert("status", Value::str("disputed"));
            let post_id = p.get("id").cloned().unwrap_or(Value::Null);
            let mut changed = BTreeSet::new();
            changed.insert(py_str(&post_id));
            invalidate(s, &changed, &rid)?;
            result.push("post", post_id);
        }
        "task.create" => {
            let session_project = s.get(T::Sessions, &actor).unwrap().at("project").clone();
            let project = data_or(d, "project", &session_project).clone();
            entity(s, T::Projects, &project)?;
            let owner = data_or(d, "owner", &actor_v).clone();
            if owner.truthy() {
                entity(s, T::Sessions, &owner)?;
            }
            let scope_default = empty_arr();
            let sc = scope(data_or(d, "scope", &scope_default))?;
            let dependencies = data_or(d, "dependencies", &scope_default).clone();
            require(dependencies.is_arr(), "dependencies must be a list")?;
            for dep in dependencies.as_arr().unwrap() {
                entity(s, T::Tasks, dep)?;
            }
            let conflicts: Vec<String> = s
                .values(T::Tasks)
                .filter(|t| {
                    t.at("project").py_eq(&project)
                        && !closed(t.at("status"))
                        && t.at("owner").truthy()
                        && owner.truthy()
                        && overlaps(sc.iter().map(|x| x.as_str()), strs(t.at("scope")))
                })
                .map(|t| py_str(t.at("id")))
                .collect();
            require(conflicts.is_empty(), format!("scope conflict with {}", conflicts.join(", ")))?;
            require(!s.contains(T::Tasks, &rid), "task already exists")?;
            let normal = Value::str("normal");
            let priority = data_or(d, "priority", &normal).clone();
            require(matches!(priority.as_str(), Some("normal") | Some("urgent")), "invalid priority")?;
            let title = text(d.at("title"), "title", 300)?.to_string();
            let context = data_or(d, "context", &empty_obj()).clone();
            s.insert(
                T::Tasks,
                &rid,
                crate::obj! {
                    "id" => rid.clone(), "project" => project, "title" => title, "owner" => owner,
                    "creator" => actor.clone(), "scope" => str_list(sc), "status" => "open", "revision" => 1i64,
                    "priority" => priority, "evidence" => empty_arr(), "correction" => false,
                    "created_at" => recorded_at, "dependencies" => dependencies, "context" => context,
                },
            );
            result.push("task", rid.clone());
        }
        "task.claim" | "task.finish" | "task.block" | "task.cancel" => {
            let key = entity(s, T::Tasks, d.at("task"))?.to_string();
            let task = s.get(T::Tasks, &key).unwrap().clone();
            expected(&task, d)?;
            require(!closed(task.at("status")), "task already closed")?;
            if op == "task.claim" {
                let old = task.at("owner").clone();
                let mut takeover_evidence = None;
                if old.truthy() && !old.py_eq(&actor_v) {
                    require(matches!(d.at("takeover"), Value::Bool(true)), "owned task requires explicit takeover")?;
                    takeover_evidence = Some(evidence(d.at("evidence"))?);
                    notice(
                        s,
                        &format!("{rid}:takeover"),
                        &old,
                        task.at("project"),
                        "ownership",
                        task.at("id"),
                        format!("{actor} took over {}; reconcile before resuming.", py_str(task.at("id"))),
                    )?;
                }
                for other in s.values(T::Tasks) {
                    if !other.at("id").py_eq(task.at("id"))
                        && other.at("project").py_eq(task.at("project"))
                        && other.at("owner").truthy()
                        && !closed(other.at("status"))
                    {
                        require(!overlaps(strs(task.at("scope")), strs(other.at("scope"))), format!("scope conflict with {}", py_str(other.at("id"))))?;
                    }
                }
                let t = s.obj_mut(T::Tasks, &key).unwrap();
                if let Some(ev) = takeover_evidence {
                    t.insert("takeover_evidence", ev);
                }
                t.insert("owner", actor.clone());
                t.insert("status", Value::str("open"));
            } else {
                let ok = may_manage(s, &actor, task.at("owner")) || (!task.at("owner").truthy() && task.at("creator").py_eq(&actor_v));
                require(ok, "only owner or parent can change this task")?;
                if op == "task.finish" {
                    let deps_done = task.at("dependencies").as_arr().map(|a| a.as_slice()).unwrap_or(&[]).iter().all(|dep| {
                        dep.as_str().and_then(|k| s.get(T::Tasks, k)).is_some_and(|t| t.at("status").as_str() == Some("done"))
                    });
                    require(deps_done, "unfinished dependencies")?;
                    let ev = evidence(d.at("evidence"))?;
                    let t = s.obj_mut(T::Tasks, &key).unwrap();
                    t.insert("evidence", ev);
                    if task.at("correction").truthy() {
                        require(matches!(d.at("resolution").as_str(), Some("corrected") | Some("mistaken")), "correction requires corrected/mistaken resolution")?;
                        t.insert("resolution", d.at("resolution").clone());
                    }
                    t.insert("status", Value::str("done"));
                } else if op == "task.cancel" {
                    require(!task.at("correction").truthy(), "urgent correction requires evidence-backed resolution")?;
                    let t = s.obj_mut(T::Tasks, &key).unwrap();
                    t.insert("status", Value::str("cancelled"));
                    let reason = text_default(d.at("reason"))?.to_string();
                    t.insert("reason", Value::Str(reason));
                } else {
                    let t = s.obj_mut(T::Tasks, &key).unwrap();
                    t.insert("status", Value::str("blocked"));
                    let reason = text_default(d.at("reason"))?.to_string();
                    t.insert("reason", Value::Str(reason));
                }
            }
            let t = s.obj_mut(T::Tasks, &key).unwrap();
            bump(t, "revision");
            result.push("task", t.get("id").cloned().unwrap_or(Value::Null));
        }
        "delivery.prepare" => {
            let items = d.at("items");
            require(matches!(items, Value::Arr(a) if a.len() <= 50), "invalid delivery size")?;
            for r in items.as_arr().unwrap() {
                let t = r.at("kind").as_str().and_then(T::from_kind);
                require(t.is_some(), "invalid delivery reference")?;
                let t = t.unwrap();
                let key = entity(s, t, r.at("id"))?;
                let item = s.get(t, key).unwrap();
                require(item.at("revision").py_eq(r.at("revision")), "delivery source changed")?;
                if t == T::Notices {
                    require(item.at("recipient").py_eq(&actor_v), "notice belongs to another session")?;
                }
            }
            let items = items.as_arr().unwrap().clone();
            s.note_delivery(&actor, &items);
            s.insert(
                T::Deliveries,
                &rid,
                crate::obj! {
                    "id" => rid.clone(), "actor" => actor.clone(), "items" => items, "acked" => false,
                    "created_at" => recorded_at, "revision" => 1i64,
                },
            );
            result.push("delivery", rid.clone());
        }
        "delivery.ack" => {
            let key = entity(s, T::Deliveries, d.at("delivery"))?.to_string();
            let item = s.get(T::Deliveries, &key).unwrap();
            require(item.at("actor").py_eq(&actor_v), "delivery belongs to another session")?;
            let notices: Vec<String> = item
                .at("items")
                .as_arr()
                .map(|a| a.as_slice())
                .unwrap_or(&[])
                .iter()
                .filter(|r| r.at("kind").as_str() == Some("notice"))
                .map(|r| py_str(r.at("id")))
                .collect();
            let id = item.at("id").clone();
            s.obj_mut(T::Deliveries, &key).unwrap().insert("acked", Value::Bool(true));
            for n in notices {
                match s.obj_mut(T::Notices, &n) {
                    Some(notice) => notice.insert("acked", Value::Bool(true)),
                    None => return reject(format!("unknown notices: {n}")),
                }
            }
            result.push("delivery", id);
        }
        "moderation.claim" => {
            let refs = d.at("posts");
            require(matches!(refs, Value::Arr(a) if !a.is_empty() && a.len() <= 8), "moderation batch must contain 1..8 posts")?;
            let refs = refs.as_arr().unwrap().clone();
            let ids: HashSet<String> = refs.iter().map(|r| py_str(r.at("id"))).collect();
            require(ids.len() == refs.len(), "duplicate moderation target")?;
            let seconds = match d.get("seconds") {
                None => 600,
                Some(Value::Int(i)) => *i,
                Some(_) => i64::MIN,
            };
            require((30..=1800).contains(&seconds), "invalid lease duration")?;
            let mut generations = Map::new();
            for r in &refs {
                let key = entity(s, T::Posts, r.at("id"))?.to_string();
                let p = s.get(T::Posts, &key).unwrap();
                require(p.at("revision").py_eq(r.at("revision")), "moderation target changed")?;
                require(p.at("due").truthy(), "post not due for moderation")?;
                let pid = py_str(p.at("id"));
                for lease in s.values(T::Leases) {
                    if lease.at("status").as_str() == Some("active") && timestamp(lease.at("expires"))? > now {
                        require(!lease.at("generations").as_obj().is_some_and(|g| g.contains_key(&pid)), "moderation already assigned")?;
                    }
                }
                let p = s.obj_mut(T::Posts, &key).unwrap();
                bump(p, "moderation_generation");
                let generation = p.get("moderation_generation").cloned().unwrap_or(Value::Null);
                generations.insert(pid, generation);
            }
            s.insert(
                T::Leases,
                &rid,
                crate::obj! {
                    "id" => rid.clone(), "actor" => actor.clone(), "posts" => refs, "generations" => Value::Obj(generations),
                    "expires" => now.add_seconds(seconds).isoformat(), "status" => "active", "revision" => 1i64,
                },
            );
            result.push("lease", rid.clone());
        }
        "moderation.apply" | "moderation.release" => {
            let key = entity(s, T::Leases, d.at("lease"))?.to_string();
            let lease = s.get(T::Leases, &key).unwrap().clone();
            require(lease.at("actor").py_eq(&actor_v) && lease.at("status").as_str() == Some("active"), "invalid moderation owner/state")?;
            if op == "moderation.release" {
                let reason = text_default(d.at("reason"))?.to_string();
                let l = s.obj_mut(T::Leases, &key).unwrap();
                l.insert("status", Value::str("released"));
                l.insert("reason", Value::Str(reason));
                l.insert("released_at", Value::str(recorded_at));
            } else {
                require(timestamp(lease.at("expires"))? > now, "moderation lease expired")?;
                let outcomes = d.at("outcomes");
                require(outcomes.is_arr(), "outcomes must be a list")?;
                let outcomes = outcomes.as_arr().unwrap();
                let mut wanted: Map = Map::new();
                for r in lease.at("posts").as_arr().map(|a| a.as_slice()).unwrap_or(&[]) {
                    wanted.insert(py_str(r.at("id")), r.at("revision").clone());
                }
                let got: BTreeSet<String> = outcomes.iter().map(|r| py_str(r.at("post"))).collect();
                let want: BTreeSet<String> = wanted.keys().cloned().collect();
                require(outcomes.len() == wanted.len() && got == want, "outcomes must cover batch exactly")?;
                let mut changed = BTreeSet::new();
                for out in outcomes {
                    let pkey = entity(s, T::Posts, out.at("post"))?.to_string();
                    let p = s.get(T::Posts, &pkey).unwrap();
                    let pid = py_str(p.at("id"));
                    let generation = lease.at("generations").get(&pid).unwrap_or(&NULL);
                    require(
                        wanted.get(&pid).is_some_and(|w| p.at("revision").py_eq(w)) && p.at("moderation_generation").py_eq(generation),
                        "stale moderation result",
                    )?;
                    let status = out.at("status").as_str().unwrap_or("");
                    require(["supported", "unsupported", "disputed", "corrected", "superseded"].contains(&status) && out.at("status").is_str(), "invalid moderation classification")?;
                    let status = status.to_string();
                    let proof = if ["supported", "corrected", "superseded"].contains(&status.as_str()) {
                        evidence(out.at("evidence"))?
                    } else if out.at("evidence").truthy() {
                        evidence(out.at("evidence"))?
                    } else {
                        empty_arr()
                    };
                    let reason = text_default(out.at("reason"))?.to_string();
                    let corrected_text = if status == "corrected" || status == "superseded" {
                        Some(text(out.at("text"), "corrected text", 32000)?.to_string())
                    } else {
                        None
                    };
                    let p = s.obj_mut(T::Posts, &pkey).unwrap();
                    if let Some(t) = corrected_text {
                        p.insert("text", Value::Str(t));
                    }
                    p.insert("status", Value::Str(status.clone()));
                    bump(p, "revision");
                    p.insert("due", Value::Bool(false));
                    p.insert("urgent", Value::Bool(false));
                    p.insert("moderation", crate::obj! {"actor" => actor.clone(), "reason" => reason, "evidence" => proof.clone(), "event" => rid.clone(), "time" => recorded_at});
                    let post_project = p.get("project").cloned().unwrap_or(Value::Null);
                    changed.insert(pid.clone());
                    let fixes = out.get("source_fixes").cloned().unwrap_or_else(empty_arr);
                    require(fixes.is_arr(), "source_fixes must be a list")?;
                    let fixes = fixes.as_arr().unwrap();
                    require(fixes.is_empty() || status == "corrected" || status == "superseded", "source fixes require an evidence-backed correction")?;
                    for (n, fix) in fixes.iter().enumerate() {
                        let project = data_or(fix, "project", &post_project).clone();
                        entity(s, T::Projects, &project)?;
                        let source = text(fix.at("source"), "source", 32000)?.to_string();
                        require(proof.as_arr().unwrap().iter().any(|r| r.at("source").as_str() == Some(&source)), "source fix requires evidence for that source")?;
                        let justification = text_default(fix.at("reason"))?.to_string();
                        let mut shas: Vec<String> = proof.as_arr().unwrap().iter().map(|x| py_str(x.at("sha256"))).collect();
                        shas.sort();
                        let correction_key = format!("{source}:{}", shas.join("|"));
                        let existing = s
                            .table(T::Tasks)
                            .iter()
                            .find(|(_, t)| t.at("correction_key").as_str() == Some(&correction_key) && t.at("project").py_eq(&project) && t.at("status").as_str() != Some("done"))
                            .map(|(k, _)| k.clone());
                        if let Some(tk) = existing {
                            let t = s.obj_mut(T::Tasks, &tk).unwrap();
                            let mut related: BTreeSet<String> = strs(t.get("related_posts").unwrap_or(&NULL)).into_iter().map(String::from).collect();
                            related.insert(pid.clone());
                            t.insert("related_posts", str_list(related.into_iter().collect()));
                            bump(t, "revision");
                        } else {
                            let tid = format!("{rid}:fix:{pid}:{n}");
                            require(!s.contains(T::Tasks, &tid), "derived correction task ID collision")?;
                            s.insert(
                                T::Tasks,
                                &tid,
                                crate::obj! {
                                    "id" => tid.clone(), "project" => project, "title" => pyfmt::prefix_chars(&justification, 300),
                                    "owner" => Value::Null, "creator" => actor.clone(), "scope" => str_list(vec![source.clone()]),
                                    "status" => "open", "priority" => "urgent", "correction" => true,
                                    "correction_key" => correction_key, "related_posts" => str_list(vec![pid.clone()]),
                                    "evidence" => proof.clone(), "revision" => 1i64, "created_at" => recorded_at,
                                    "dependencies" => empty_arr(), "source" => source,
                                },
                            );
                        }
                    }
                }
                invalidate(s, &changed, &rid)?;
                s.obj_mut(T::Leases, &key).unwrap().insert("status", Value::str("applied"));
            }
            result.push("lease", lease.at("id").clone());
        }
        _ => return Err(Error::Rejected(format!("unknown operation: {op}"))),
    }
    Ok(Value::Obj(result))
}
