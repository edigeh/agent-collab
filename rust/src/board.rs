//! Durable receipts, deterministic replay, artifacts, and bounded board reads
//! (`board.py`). The write path, its locks and its fsyncs are unchanged; only
//! the saved view differs (see `snapshot.rs`).

use crate::error::{reject, require, Error, Result};
use crate::journal::{self, Journal, LockedJournal, Record};
use crate::json::{self, Map, Value, NULL};
use crate::presence;
use crate::pyfmt::{self, utcnow, Ts};
use crate::snapshot;
use crate::state::{self, State, T};
use crate::view::View;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const BRIEF_VIEWS: [&str; 2] = ["compact", "full"];
pub const DEFAULT_BRIEF_BUDGET: i64 = 12_000;
pub const COMPACT_PREVIEW_LIMIT: usize = 600;
pub const COMPACT_SCOPE_LIMIT: usize = 8;
/// A command that had to replay this many records saves its view for the next ones.
pub const SAVE_AFTER: usize = 256;
const STALE_TEXT: &str = "[STALE SUMMARY: retrieve the current source claims before relying on this summary.]";
const INSTRUCTION: &str = "Acknowledge this delivery after reading the returned items. Use next_cursor for continuation without skipping items; start a new checkpoint without a cursor. Do not repeat inbox or status just to reread this page. Use read ITEM --kind post|task|notice only when full detail is needed. Requests and open commitments remain unresolved until explicitly handled.";

pub fn digest(data: &[u8]) -> String {
    pyfmt::sha256_hex(data)
}

fn preview(value: &Value, limit: usize) -> Value {
    match value {
        Value::Str(s) if pyfmt::char_len(s) > limit => Value::Str(format!("{}… [read full item]", pyfmt::prefix_chars(s, limit))),
        other => other.clone(),
    }
}

/// `list(value or [])`, cut to `limit`, and the full count.
fn bounded_values(value: &Value, limit: usize) -> (Vec<Value>, usize) {
    let values: Vec<Value> = match value {
        Value::Arr(a) => a.clone(),
        Value::Obj(m) => m.keys().map(|k| Value::Str(k.clone())).collect(),
        Value::Str(s) => s.chars().map(|c| Value::Str(c.to_string())).collect(),
        _ => Vec::new(),
    };
    let n = values.len();
    (values.into_iter().take(limit).collect(), n)
}

fn len_of(v: Option<&Value>) -> usize {
    match v {
        Some(Value::Arr(a)) => a.len(),
        Some(Value::Obj(m)) => m.len(),
        Some(Value::Str(s)) => pyfmt::char_len(s),
        _ => 0,
    }
}

/// Render a delivery item without exposing bulky fields by default.
pub fn render_item(kind: &str, item: &Value, view: &str, preview_limit: usize) -> Result<Value> {
    require(BRIEF_VIEWS.contains(&view), "brief view must be compact or full")?;
    if view == "full" {
        let mut shown = item.as_obj().cloned().unwrap_or_default();
        if kind == "post" {
            if item.at("stale").truthy() {
                shown.insert("text", STALE_TEXT);
            }
            let k = shown.remove("kind").unwrap_or(Value::Null);
            shown.push("post_kind", k);
        } else if kind == "notice" {
            let k = shown.remove("kind").unwrap_or(Value::Null);
            shown.push("notice_kind", k);
        }
        shown.insert("kind", kind);
        return Ok(Value::Obj(shown));
    }
    let mut r = Map::with_capacity(20);
    match kind {
        "post" => {
            let (scope, scope_count) = bounded_values(item.at("scope"), COMPACT_SCOPE_LIMIT);
            let text = if item.at("stale").truthy() { Value::str(STALE_TEXT) } else { item.get("text").cloned().unwrap_or(Value::str("")) };
            r.push("id", item.at("id").clone());
            r.push("kind", "post");
            r.push("project", item.at("project").clone());
            r.push("actor", item.at("actor").clone());
            r.push("to", item.at("to").clone());
            r.push("revision", item.at("revision").clone());
            r.push("status", item.at("status").clone());
            r.push("urgent", item.at("urgent").truthy());
            r.push("stale", item.at("stale").truthy());
            r.push("due", item.at("due").truthy());
            r.push("created_at", item.at("created_at").clone());
            r.push("post_kind", item.at("kind").clone());
            r.push("text", preview(&text, preview_limit));
            r.push("evidence_count", len_of(item.get("evidence")));
            r.push("refs_count", len_of(item.get("refs")));
            r.push("reports_count", len_of(item.get("reports")));
            r.push("detail_available", true);
            if !item.at("scope").is_null() {
                r.push("scope", Value::Arr(scope.clone()));
                r.push("scope_count", scope_count);
                r.push("scope_truncated", scope_count > scope.len());
            }
        }
        "task" => {
            let (scope, scope_count) = bounded_values(item.at("scope"), COMPACT_SCOPE_LIMIT);
            r.push("id", item.at("id").clone());
            r.push("kind", "task");
            r.push("project", item.at("project").clone());
            r.push("title", preview(item.get("title").unwrap_or(&Value::str("")), preview_limit.min(300)));
            r.push("owner", item.at("owner").clone());
            r.push("creator", item.at("creator").clone());
            r.push("revision", item.at("revision").clone());
            r.push("status", item.at("status").clone());
            r.push("priority", item.at("priority").clone());
            r.push("correction", item.at("correction").truthy());
            let shown = scope.len();
            r.push("scope", Value::Arr(scope));
            r.push("scope_count", scope_count);
            r.push("scope_truncated", scope_count > shown);
            r.push("dependencies_count", len_of(item.get("dependencies")));
            r.push("evidence_count", len_of(item.get("evidence")));
            r.push("source_post", item.at("source_post").clone());
            r.push("created_at", item.at("created_at").clone());
            r.push("detail_available", true);
            if let Some(reason) = item.get("reason") {
                r.push("reason", preview(reason, preview_limit));
            }
            if let Some(resolution) = item.get("resolution") {
                r.push("resolution", resolution.clone());
            }
        }
        "notice" => {
            let nk = item.at("kind").clone();
            let urgent = matches!(nk.as_str(), Some("correction") | Some("ownership") | Some("rejection") | Some("overlap"));
            r.push("id", item.at("id").clone());
            r.push("kind", "notice");
            r.push("notice_kind", nk);
            r.push("project", item.at("project").clone());
            r.push("recipient", item.at("recipient").clone());
            r.push("target", item.at("target").clone());
            r.push("revision", item.at("revision").clone());
            r.push("urgent", urgent);
            r.push("text", preview(item.get("text").unwrap_or(&Value::str("")), preview_limit));
            r.push("detail_available", true);
        }
        _ => return reject("invalid delivery item kind"),
    }
    Ok(Value::Obj(r))
}

fn outcome(env: &Record) -> Value {
    let event = &env.payload;
    let mut m = Map::new();
    m.push("status", if event.at("accepted").truthy() { "accepted" } else { "rejected" });
    m.push("request_id", event.at("request").at("id").clone());
    m.push("seq", env.seq);
    if let Some(result) = event.at("result").as_obj() {
        for (k, v) in result.iter() {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Obj(m)
}

pub fn durable_directory(path: &Path) -> std::io::Result<()> {
    journal::fsync_directory(path)
}

/// Write `data` to `path` atomically: temp file, fsync, rename, directory fsync.
pub fn atomic_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    write_file(path, data, true)
}

/// Replace `path` by rename. With `durable` false the data is not forced to
/// disk: for files that are only a cache checked on load, where a crash that
/// loses them costs a rebuild, not information.
fn write_file(path: &Path, data: &[u8], durable: bool) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("/"));
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(|e| pyfmt::with_path(e, parent))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut rnd = [0u8; 6];
    let (tmp, mut file) = loop {
        pyfmt::random_bytes(&mut rnd);
        let suffix: String = rnd.iter().map(|b| (b"abcdefghijklmnopqrstuvwxyz0123456789_"[(*b as usize) % 37]) as char).collect();
        let tmp = parent.join(format!(".{name}.{suffix}"));
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(pyfmt::with_path(e, &tmp)),
        }
    };
    let result = (|| {
        file.write_all(data)?;
        if !durable {
            drop(file);
            return std::fs::rename(&tmp, path);
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        durable_directory(parent)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn os(e: std::io::Error) -> Error {
    Error::from(e)
}

pub struct BriefArgs<'a> {
    pub project: Option<String>,
    pub limit: i64,
    pub offset: i64,
    pub post_id: Option<String>,
    pub cursor: Option<&'a str>,
    pub view: String,
    pub budget: Option<i64>,
    pub item_id: Option<String>,
    pub item_kind: Option<String>,
}

impl Default for BriefArgs<'_> {
    fn default() -> Self {
        BriefArgs { project: None, limit: 12, offset: 0, post_id: None, cursor: None, view: "compact".into(), budget: None, item_id: None, item_kind: None }
    }
}

pub struct Board {
    pub root: PathBuf,
    pub receipt_root: PathBuf,
    pub timeout: f64,
    pub journal: Journal,
    pub snap_path: PathBuf,
    pub key_path: PathBuf,
    pub checks_path: PathBuf,
    pub save_after: usize,
    view: RefCell<Option<View>>,
    receipts: RefCell<HashMap<String, std::rc::Rc<Journal>>>,
}

impl Board {
    pub fn new(root: &str, receipt_root: Option<&str>) -> Board {
        let root = pyfmt::pathlib_absolute(&pyfmt::expanduser(root));
        let receipt_root = match receipt_root {
            Some(r) if !r.is_empty() => pyfmt::pathlib_absolute(&pyfmt::expanduser(r)),
            _ => {
                let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                root.with_file_name(format!("{name}-receipts"))
            }
        };
        Board {
            journal: Journal::new(root.join("events.jsonl"), 2.0),
            snap_path: root.join("view.rsnap"),
            key_path: root.join("view.key"),
            checks_path: root.join("source-checks.json"),
            root,
            receipt_root,
            timeout: 2.0,
            save_after: SAVE_AFTER,
            view: RefCell::new(None),
            receipts: RefCell::default(),
        }
    }

    // ------------------------------------------------------------ artifacts

    pub fn capture(&self, source: &str) -> Result<Value> {
        let path = pyfmt::resolve_strict(&pyfmt::expanduser(source)).map_err(os)?;
        if !path.is_file() {
            return Err(Error::Value(format!("evidence is not a regular file: {}", pyfmt::path_str(&path))));
        }
        let raw = std::fs::read(&path).map_err(|e| os(pyfmt::with_path(e, &path)))?;
        let sha = digest(&raw);
        let target = self.root.join("artifacts").join(&sha);
        let meta = std::fs::symlink_metadata(&target);
        if meta.as_ref().is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(Error::Corrupt("artifact path is a symlink".into()));
        }
        if target.exists() {
            let stored = std::fs::read(&target).map_err(|e| os(pyfmt::with_path(e, &target)))?;
            if digest(&stored) != sha {
                return Err(Error::Corrupt(format!("artifact corrupted: {sha}")));
            }
        } else {
            atomic_file(&target, &raw).map_err(os)?;
        }
        Ok(crate::obj! {"source" => pyfmt::path_str(&path), "sha256" => sha, "bytes" => raw.len(), "captured_at" => utcnow()})
    }

    pub fn artifact(&self, sha: &str) -> Result<Vec<u8>> {
        if !state::is_hash(sha) {
            return Err(Error::Value("invalid artifact hash".into()));
        }
        let path = self.root.join("artifacts").join(sha);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(Error::Corrupt("artifact path is a symlink".into()));
        }
        let raw = std::fs::read(&path).map_err(|e| os(pyfmt::with_path(e, &path)))?;
        if digest(&raw) != sha {
            return Err(Error::Corrupt(format!("artifact corrupted: {sha}")));
        }
        Ok(raw)
    }

    fn cleanup_preview_from(&self, sha: &str, records: &[Record]) -> Result<Value> {
        let raw = self.artifact(sha)?;
        let recorded: HashSet<String> = records.iter().map(|r| state::py_str(r.payload.at("request").at("id"))).collect();
        let mut references = Vec::new();
        for r in records {
            let req = r.payload.at("request");
            let op = req.at("op").as_str().unwrap_or("");
            if !op.starts_with("artifact.") {
                let mut found = HashSet::new();
                state::artifact_hashes(req.at("data"), &mut found);
                if found.contains(sha) {
                    references.push(crate::obj! {"seq" => r.seq, "request_id" => req.at("id").clone(), "operation" => req.at("op").clone()});
                }
            }
        }
        let mut pending = Vec::new();
        let mut files: Vec<PathBuf> = std::fs::read_dir(&self.receipt_root)
            .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect())
            .unwrap_or_default();
        files.sort();
        for path in files {
            let j = Journal::new(path, self.timeout);
            let receipts = j.locked(None)?;
            for record in &receipts.records {
                let item = &record.payload;
                let req = item.get("request").unwrap_or(&NULL);
                if item.at("type").as_str() == Some("pending")
                    && !recorded.contains(&state::py_str(req.at("id")))
                    && !req.at("op").as_str().unwrap_or("").starts_with("artifact.")
                {
                    let mut found = HashSet::new();
                    state::artifact_hashes(req.get("data").unwrap_or(&NULL), &mut found);
                    if found.contains(sha) {
                        pending.push(crate::obj! {"session" => req.at("actor").clone(), "request_id" => req.at("id").clone()});
                    }
                }
            }
        }
        let mut preview = Map::new();
        preview.push("sha256", sha);
        preview.push("bytes", raw.len());
        preview.push("head", records.last().map(|r| Value::Str(r.sha256.clone())).unwrap_or(Value::Null));
        let count = references.len();
        preview.push("references", Value::Arr(references));
        preview.push("reference_count", count);
        preview.push("pending_receipts", Value::Arr(pending));
        let token = digest(&json::canonical(&Value::Obj(preview.clone())));
        preview.push("token", token);
        Ok(Value::Obj(preview))
    }

    pub fn cleanup_preview(&self, sha: &str) -> Result<Value> {
        let log = self.journal.locked(None)?;
        self.cleanup_preview_from(sha, &log.records)
    }

    fn compact_preview(preview: &Value) -> Value {
        let mut m = Map::new();
        for (k, v) in preview.as_obj().unwrap().iter() {
            if k != "references" && k != "pending_receipts" {
                m.push(k.clone(), v.clone());
            }
        }
        Value::Obj(m)
    }

    pub fn cleanup_remove(&self, actor: &str, sha: &str, token: &str) -> Result<Value> {
        let preview = self.cleanup_preview(sha)?;
        require(!preview.at("pending_receipts").truthy(), "reconcile pending artifact receipts before cleanup")?;
        let compact = Self::compact_preview(&preview);
        let req = self.request(actor, "artifact.remove.plan", crate::obj! {"sha256" => sha, "token" => token, "preview" => compact});
        let result = self.submit(req)?;
        if result.at("status").as_str() != Some("accepted") {
            return Ok(result);
        }
        let removal = state::py_str(result.at("removal"));
        self.cleanup_resume(actor, &removal)
    }

    pub fn cleanup_resume(&self, actor: &str, removal_id: &str) -> Result<Value> {
        let attempt = self.current(|_, view| {
            let s = &view.state;
            let rid = Value::str(removal_id);
            let key = state::entity(s, T::ArtifactRemovals, &rid)?.to_string();
            state::entity(s, T::Sessions, &Value::str(actor))?;
            let removal = s.get(T::ArtifactRemovals, &key).unwrap();
            let ok = state::may_manage(s, actor, removal.at("actor")) || s.get(T::Sessions, actor).unwrap().at("harness").as_str() == Some("human");
            require(ok, "only responsible agent or user can resume removal")?;
            if removal.at("status").as_str() == Some("removed") {
                return Ok(Some(crate::obj! {"status" => "accepted", "removal" => removal_id}));
            }
            let path = self.root.join("artifacts").join(state::py_str(removal.at("sha256")));
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                return reject("artifact path is a symlink");
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(os(pyfmt::with_path(e, &path))),
            }
            durable_directory(path.parent().unwrap()).map_err(os)?;
            Ok(None)
        });
        match attempt {
            Ok(Some(done)) => return Ok(done),
            Ok(None) => {}
            Err(e @ (Error::Os(_) | Error::Journal(_) | Error::Busy(_) | Error::Corrupt(_))) => {
                return Ok(crate::obj! {"status" => "pending", "removal" => removal_id, "error" => e.message()});
            }
            Err(e) => return Err(e),
        }
        let mut result = self.submit(self.request(actor, "artifact.remove.finish", crate::obj! {"removal" => removal_id}))?;
        result.as_obj_mut().unwrap().insert("removal", removal_id);
        Ok(result)
    }

    /// Verify preserved bytes and require fresh source snapshots for moderation/closure.
    fn validate_evidence(&self, request: &Value) -> Result<()> {
        let data = request.at("data");
        let op = request.at("op").as_str().unwrap_or("");
        let empty = Value::Arr(Vec::new());
        let mut groups: Vec<&Value> = vec![data.get("evidence").unwrap_or(&empty)];
        if op == "moderation.apply" {
            if let Some(Value::Arr(outs)) = data.get("outcomes") {
                for o in outs {
                    if o.is_obj() {
                        groups.push(o.get("evidence").unwrap_or(&empty));
                    }
                }
            }
        }
        for refs in groups {
            let Value::Arr(refs) = refs else {
                return reject("evidence must be a list");
            };
            for r in refs {
                let sha_text = state::py_str(r.get("sha256").unwrap_or(&Value::str("")));
                if !r.is_obj() || !state::is_hash(&sha_text) {
                    return reject("invalid evidence reference");
                }
                let check = || -> std::result::Result<(), String> {
                    self.artifact(&sha_text).map_err(|e| e.message())?;
                    let current_ok = r.get("current").map(|c| c.truthy()).unwrap_or(true);
                    if op == "task.finish" || op == "task.claim" || (op == "moderation.apply" && current_ok) {
                        let source = match r.get("source") {
                            Some(Value::Str(s)) => s.clone(),
                            Some(other) => state::py_str(other),
                            None => return Err("'source'".into()),
                        };
                        let current = std::fs::read(&source).map_err(|e| pyfmt::with_path(e, Path::new(&source)).to_string())?;
                        if digest(&current) != sha_text {
                            return Err(format!("source changed since capture: {source}"));
                        }
                    }
                    Ok(())
                };
                if let Err(msg) = check() {
                    return Err(Error::Rejected(format!("evidence unavailable: {msg}")));
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ views

    /// Lock the journal and run `f` with this board's view at its head.
    ///
    /// The view resumes from this process's last one, else from view.rsnap,
    /// else from a replay of the committed records read before locking, so a
    /// full replay never holds up other writers. Under the lock the journal
    /// confirms the view's position with one read and hands over only the
    /// records after it; if it cannot, everything replays from record 1. A
    /// view that needed a long replay is saved once the lock is released.
    pub fn current<R>(&self, f: impl FnOnce(&mut LockedJournal, &mut View) -> Result<R>) -> Result<R> {
        let mut replayed = 0usize;
        let cached = self.view.borrow_mut().take();
        let mut view = match cached.or_else(|| self.saved_view()) {
            Some(v) => v,
            None => {
                let mut v = View::new();
                crate::bulk_memory();
                // Replay as records validate; a replay failure is reported only
                // once the whole journal has validated, as a read-then-replay would.
                let mut failed = None;
                self.journal.committed_each(&mut |record, position| {
                    if failed.is_none() {
                        failed = v.replay_one(&record, &position).err();
                        replayed += 1;
                    }
                    crate::discard(record);
                    Ok(())
                })?;
                if let Some(e) = failed {
                    return Err(e);
                }
                v
            }
        };
        let outcome = (|| {
            let mut log = self.journal.locked(view.position.as_ref())?;
            if log.base != view.position {
                view = View::new();
                replayed = 0;
            }
            replayed += log.records.len();
            let records = std::mem::take(&mut log.records);
            let positions = std::mem::take(&mut log.positions);
            let replay = view.replay(&records, &positions);
            log.records = records;
            log.positions = positions;
            replay?;
            f(&mut log, &mut view)
        })();
        match outcome {
            Ok(r) => {
                if replayed >= self.save_after && view.position.is_some() {
                    self.save_view(&mut view);
                }
                *self.view.borrow_mut() = Some(view);
                Ok(r)
            }
            Err(e) => Err(e),
        }
    }

    /// The secret that makes saved views something only this board's replay writes.
    fn view_key(&self, create: bool) -> Option<Vec<u8>> {
        let key = std::fs::read(&self.key_path).unwrap_or_default();
        if key.len() == 64 && key.iter().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
            return Some(key);
        }
        if !create {
            return None;
        }
        let mut raw = [0u8; 32];
        pyfmt::random_bytes(&mut raw);
        atomic_file(&self.key_path, pyfmt::hex(&raw).as_bytes()).ok()?;
        self.view_key(false)
    }

    fn saved_view(&self) -> Option<View> {
        let key = self.view_key(false)?;
        let data = snapshot::Bytes::read(&self.snap_path).ok()?;
        let loaded = snapshot::decode(&key, data)?;
        Some(View { position: Some(loaded.position), state: loaded.state, requests: loaded.requests })
    }

    fn save_view(&self, view: &mut View) -> bool {
        let Some(key) = self.view_key(true) else { return false };
        let Some(position) = &view.position else { return false };
        let position = position.clone();
        let data = snapshot::encode(&key, &position, &mut view.state, &view.requests);
        // The MAC rejects a torn or stale file, so the view needs no fsync.
        write_file(&self.snap_path, &data, false).is_ok()
    }

    /// The current state (a cheap copy-on-write snapshot).
    pub fn read_state(&self) -> Result<State> {
        self.current(|_, view| Ok(view.state.clone()))
    }

    /// The current state and every committed record, read under one lock.
    pub fn read_history(&self) -> Result<(State, Vec<Record>)> {
        self.current(|log, view| Ok((view.state.clone(), log.history()?)))
    }

    /// Replay every record from the start and compare with the incremental view.
    pub fn verify(&self) -> Result<Value> {
        crate::bulk_memory();
        let (mut full, view) = {
            let mut full = View::new();
            let mut failed = None;
            let mut log = self.journal.locked_with(None, &mut |record, position| {
                if failed.is_none() {
                    failed = full.replay_one(record, position).err();
                }
                Ok(())
            })?;
            if let Some(e) = failed {
                return Err(e);
            }
            let cached = self.view.borrow_mut().take();
            let mut view = cached.or_else(|| self.saved_view());
            let mut at = view.as_ref().and_then(|v| v.position.as_ref()).map(|p| p.seq as usize).unwrap_or(0);
            let stale = match &view {
                None => true,
                Some(v) => at > 0 && (at > log.positions.len() || Some(&log.positions[at - 1]) != v.position.as_ref()),
            };
            if stale {
                view = Some(View::new());
                at = 0;
            }
            let mut view = view.unwrap();
            view.replay(&log.records[at..], &log.positions[at..])?;
            crate::discard(std::mem::take(&mut log.records));
            (full, view)
        };
        let (expected, found) = digest_pair(&full, &view);
        let matched = expected == found;
        if !matched {
            self.save_view(&mut full);
        }
        let head = full.position.as_ref().map(|p| p.to_value()).unwrap_or(Value::Null);
        let records = full.position.as_ref().map(|p| p.seq).unwrap_or(0);
        *self.view.borrow_mut() = Some(full);
        Ok(crate::obj! {
            "status" => if matched { "accepted" } else { "rejected" }, "records" => records, "head" => head,
            "replay_sha256" => expected, "view_sha256" => found, "match" => matched,
        })
    }

    pub fn request(&self, actor: &str, op: &str, data: Value) -> Value {
        crate::obj! {"id" => pyfmt::uuid4(), "actor" => actor, "op" => op, "data" => data, "observed_at" => utcnow()}
    }

    /// The outcome already recorded for this request ID, read back from its record.
    fn recorded(log: &LockedJournal, view: &View, req: &Value, sha: &str) -> Result<Option<Value>> {
        let id = state::py_str(req.at("id"));
        let Some([seq, start, end]) = view.requests.get(&id) else {
            return Ok(None);
        };
        let env = log.record_at(seq, start, end)?;
        let recorded = env.payload.at("request");
        if state::py_str(recorded.at("id")) != id {
            return Err(Error::Corrupt("request index points at another record".into()));
        }
        if digest(&json::canonical(recorded)) != sha {
            return reject("request ID reused with different content");
        }
        Ok(Some(outcome(&env)))
    }

    fn ingest(&self, req: &Value) -> Result<Value> {
        let sha = digest(&json::canonical(req));
        // Resolve committed requests before touching external evidence, then
        // recheck after validation to handle concurrent identical submissions.
        if let Some(previous) = self.current(|log, view| Self::recorded(log, view, req, &sha))? {
            return Ok(previous);
        }
        // Inspect external evidence outside the board lock.
        let evidence_error = match state::validate_request(req).and_then(|_| self.validate_evidence(req)) {
            Ok(()) => None,
            Err(Error::Rejected(m)) => Some(m),
            Err(e) => return Err(e),
        };
        self.current(|log, view| {
            if let Some(previous) = Self::recorded(log, view, req, &sha)? {
                return Ok(previous);
            }
            let recorded_at = utcnow();
            let attempt = (|| -> Result<(State, Value)> {
                if let Some(m) = &evidence_error {
                    return Err(Error::Rejected(m.clone()));
                }
                // Recheck the preserved artifacts under the lock so accepted
                // refs cannot race an explicit cleanup.
                let data = req.at("data");
                let mut groups: Vec<&Value> = vec![data.get("evidence").unwrap_or(&NULL)];
                if req.at("op").as_str() == Some("moderation.apply") {
                    for out in data.at("outcomes").as_arr().map(|a| a.as_slice()).unwrap_or(&[]) {
                        groups.push(out.get("evidence").unwrap_or(&NULL));
                    }
                }
                for refs in groups {
                    for r in refs.as_arr().map(|a| a.as_slice()).unwrap_or(&[]) {
                        if let Err(e) = self.artifact(&state::py_str(r.at("sha256"))) {
                            match e {
                                Error::Os(_) | Error::Value(_) | Error::Corrupt(_) => return Err(Error::Rejected(format!("preserved evidence unavailable: {}", e.message()))),
                                other => return Err(other),
                            }
                        }
                    }
                }
                let op = req.at("op").as_str().unwrap_or("");
                if op == "artifact.remove.finish" {
                    let key = state::entity(&view.state, T::ArtifactRemovals, data.at("removal"))?;
                    let sha = state::py_str(view.state.get(T::ArtifactRemovals, key).unwrap().at("sha256"));
                    require(!self.root.join("artifacts").join(sha).exists(), "artifact still exists; resume physical removal first")?;
                }
                if op == "artifact.remove.plan" {
                    let history = log.history()?;
                    let preview = self.cleanup_preview_from(&state::py_str(data.at("sha256")), &history)?;
                    require(!preview.at("pending_receipts").truthy(), "reconcile pending artifact receipts before cleanup")?;
                    let compact = Self::compact_preview(&preview);
                    require(preview.at("token").py_eq(data.at("token")) && compact.py_eq(data.at("preview")), "cleanup preview changed; request a new preview")?;
                }
                let mut next = view.state.clone();
                let result = state::apply(&mut next, req, &recorded_at)?;
                Ok((next, result))
            })();
            let (next, result, accepted) = match attempt {
                Ok((next, result)) => (next, result, true),
                Err(Error::Rejected(m)) => {
                    let mut next = view.state.clone();
                    state::record_rejection(&mut next, req, &m)?;
                    (next, crate::obj! {"error" => m}, false)
                }
                Err(e) => return Err(e),
            };
            let event = crate::obj! {"version" => 1i64, "request" => req.clone(), "recorded_at" => recorded_at, "accepted" => accepted, "result" => result};
            let env = log.append(event)?;
            let head = log.head.clone().unwrap();
            view.state = next;
            view.requests.insert(state::py_str(req.at("id")), [head.seq, head.start, head.end]);
            view.position = Some(head);
            Ok(outcome(&env))
        })
    }

    /// The session's receipt journal; one per session for this board, so its
    /// validated records carry over from one lock to the next.
    fn receipt_journal(&self, actor: &str) -> Result<std::rc::Rc<Journal>> {
        state::identifier(&Value::str(actor))?;
        let path = self.receipt_root.join(format!("{actor}.jsonl"));
        let mut cache = self.receipts.borrow_mut();
        match cache.get(actor) {
            Some(j) if j.path == path && j.timeout == self.timeout => Ok(j.clone()),
            _ => {
                let j = std::rc::Rc::new(Journal::new(path, self.timeout));
                cache.insert(actor.to_string(), j.clone());
                Ok(j)
            }
        }
    }

    pub fn submit(&self, request: Value) -> Result<Value> {
        state::identifier(request.at("id"))?;
        state::identifier(request.at("actor"))?;
        if !request.at("op").is_str() || !request.at("data").is_obj() {
            return reject("request needs op and data");
        }
        // Canonical serialization checks all input before any durable receipt is written.
        let encoded = json::try_canonical(&request).map_err(|e| Error::Value(e.0))?;
        let id = state::py_str(request.at("id"));
        let actor = state::py_str(request.at("actor"));
        let receipts = self.receipt_journal(&actor)?;
        {
            let mut out = receipts.locked_all()?;
            let existing = out
                .records
                .iter()
                .find(|r| r.payload.at("type").as_str() == Some("pending") && r.payload.at("request").at("id").as_str() == Some(&id))
                .map(|r| r.payload.at("request").clone());
            match existing {
                Some(existing) => {
                    if json::canonical(&existing) != encoded {
                        return reject("request ID reused with different content");
                    }
                }
                None => {
                    out.append(crate::obj! {"type" => "pending", "request" => request.clone()})?;
                }
            }
        }
        let outcomes = self.sync(&actor)?;
        for o in &outcomes {
            if o.at("request_id").as_str() == Some(&id) {
                return Ok(o.clone());
            }
        }
        if outcomes.last().is_some_and(|o| o.at("status").as_str() == Some("pending")) {
            return Ok(crate::obj! {"status" => "pending", "request_id" => id, "error" => "Earlier receipt is pending; session ordering is preserved."});
        }
        self.deliver(&request, &receipts)
    }

    fn deliver(&self, request: &Value, receipts: &Journal) -> Result<Value> {
        let mut result = match self.ingest(request) {
            Ok(r) => r,
            Err(e @ (Error::Rejected(_) | Error::Journal(_) | Error::Busy(_) | Error::Corrupt(_) | Error::Os(_))) => {
                return Ok(crate::obj! {"status" => "pending", "request_id" => request.at("id").clone(), "error" => e.message()});
            }
            Err(e) => return Err(e),
        };
        let id = request.at("id").as_str().unwrap_or("");
        let acked = (|| -> Result<()> {
            let mut out = receipts.locked_all()?;
            if !out.records.iter().any(|r| r.payload.at("type").as_str() == Some("ack") && r.payload.at("request_id").as_str() == Some(id)) {
                out.append(crate::obj! {"type" => "ack", "request_id" => id, "seq" => result.at("seq").clone()})?;
            }
            Ok(())
        })();
        if let Err(e) = acked {
            if e.is_journal() || matches!(e, Error::Os(_)) {
                result.as_obj_mut().unwrap().insert("warning", format!("Board recorded outcome; receipt acknowledgment pending: {}", e.message()));
            } else {
                return Err(e);
            }
        }
        Ok(result)
    }

    pub fn sync(&self, actor: &str) -> Result<Vec<Value>> {
        let receipts = self.receipt_journal(actor)?;
        let pending: Vec<Value> = {
            let out = receipts.locked_all()?;
            let acked: HashSet<String> = out
                .records
                .iter()
                .filter(|r| r.payload.at("type").as_str() == Some("ack"))
                .map(|r| state::py_str(r.payload.at("request_id")))
                .collect();
            out.records
                .iter()
                .filter(|r| r.payload.at("type").as_str() == Some("pending") && !acked.contains(&state::py_str(r.payload.at("request").at("id"))))
                .map(|r| r.payload.at("request").clone())
                .collect()
        };
        let mut outcomes = Vec::new();
        for req in pending {
            let o = self.deliver(&req, &receipts)?;
            let stop = o.at("status").as_str() == Some("pending");
            outcomes.push(o);
            if stop {
                break;
            }
        }
        Ok(outcomes)
    }

    // ------------------------------------------------------------ freshness

    /// Rotate a bounded, model-free freshness check at active checkpoints.
    pub fn check_sources(&self, actor: &str, project: &str, limit: usize) -> Result<Value> {
        require((1..=32).contains(&limit), "source scan limit must be 1..32")?;
        let s = self.read_state()?;
        state::entity(&s, T::Sessions, &Value::str(actor))?;
        let project_v = Value::str(project);
        let key = state::entity(&s, T::Projects, &project_v)?;
        let roots: Vec<PathBuf> = s.get(T::Projects, key).unwrap().at("roots").as_arr().map(|a| a.iter().map(|r| pyfmt::resolve(Path::new(&state::py_str(r)))).collect()).unwrap_or_default();
        let mut looked = self.source_checks();
        let mut resolved: HashMap<String, bool> = HashMap::new();
        let mut candidates: Vec<(&Value, Vec<String>, Map)> = Vec::new();
        for post in s.values(T::Posts) {
            if post.at("project").as_str() != Some(project) || post.at("due").truthy() {
                continue;
            }
            let moderation_evidence = post.get("moderation").and_then(|m| m.get("evidence")).filter(|e| e.truthy());
            let empty = Value::Arr(Vec::new());
            let proofs = moderation_evidence.or_else(|| post.get("evidence")).unwrap_or(&empty);
            let proofs = proofs.as_arr().map(|a| a.as_slice()).unwrap_or(&[]);
            let mut paths: Vec<String> = proofs.iter().map(|r| state::py_str(r.at("source"))).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
            paths.retain(|p| {
                *resolved.entry(p.clone()).or_insert_with(|| {
                    let rp = pyfmt::resolve(Path::new(p));
                    roots.iter().any(|root| pyfmt::is_relative_to(&rp, root))
                })
            });
            if !paths.is_empty() {
                let mut known = Map::new();
                for r in proofs {
                    known.insert(state::py_str(r.at("source")), r.at("sha256").clone());
                }
                if let Some(obs) = post.at("source_observations").as_obj() {
                    for (k, v) in obs.iter() {
                        known.insert(k.clone(), v.clone());
                    }
                }
                candidates.push((post, paths, known));
            }
        }
        let sort_key = |post: &Value| -> (String, String, String) {
            let a = looked.get(&state::py_str(post.at("id"))).cloned().unwrap_or_default();
            let b = post.get("source_checked_at").map(state::py_str).unwrap_or_default();
            (a.max(b), state::py_str(post.at("created_at")), state::py_str(post.at("id")))
        };
        candidates.sort_by_cached_key(|c| sort_key(c.0));
        let (mut checked, mut changed) = (0i64, 0i64);
        let mut outcomes = Vec::new();
        let any = !candidates.is_empty();
        for (post, paths, known) in candidates.into_iter().take(limit) {
            let mut observations = Map::new();
            for path in paths.iter().take(32) {
                let p = Path::new(path);
                let sha = if p.is_file() { std::fs::read(p).ok().map(|b| Value::Str(digest(&b))).unwrap_or(Value::Null) } else { Value::Null };
                observations.insert(path.clone(), sha);
            }
            if observations.iter().all(|(path, sha)| known.get(path).unwrap_or(&NULL).py_eq(sha)) {
                looked.insert(state::py_str(post.at("id")), utcnow());
                checked += 1;
                continue;
            }
            let mut proofs = Vec::new();
            let keys: Vec<String> = observations.keys().cloned().collect();
            for path in keys {
                match self.capture(&path) {
                    Ok(proof) => {
                        observations.insert(path, proof.at("sha256").clone());
                        proofs.push(proof);
                    }
                    Err(Error::Os(_) | Error::Value(_)) => observations.insert(path, Value::Null),
                    Err(e) => return Err(e),
                }
            }
            let req = self.request(
                actor,
                "post.observe",
                crate::obj! {"post" => post.at("id").clone(), "expected" => post.at("revision").clone(), "observations" => observations, "evidence" => proofs},
            );
            let o = self.submit(req)?;
            if o.at("status").as_str() == Some("accepted") {
                looked.insert(state::py_str(post.at("id")), utcnow());
                checked += 1;
                changed += o.at("changed").truthy() as i64;
            }
            outcomes.push(o);
        }
        if any {
            let kept: Map = Map(looked.into_iter().filter(|(pid, _)| s.contains(T::Posts, pid)).map(|(k, v)| (k, Value::Str(v))).collect());
            let mut data = json::canonical(&Value::Obj(kept));
            data.push(b'\n');
            let _ = atomic_file(&self.checks_path, &data);
        }
        Ok(crate::obj! {"checked" => checked, "changed" => changed, "outcomes" => outcomes})
    }

    fn source_checks(&self) -> indexmap::IndexMap<String, String> {
        let mut out = indexmap::IndexMap::new();
        let Ok(raw) = std::fs::read(&self.checks_path) else { return out };
        if let Ok(Value::Obj(m)) = json::parse(&raw, json::LENIENT) {
            for (k, v) in m.0 {
                if let Value::Str(s) = v {
                    out.insert(k, s);
                }
            }
        }
        out
    }

    pub fn due(&self, project: Option<&str>, limit: usize, force: bool) -> Result<Vec<Value>> {
        let s = self.read_state()?;
        Self::due_in(&s, project, limit, force)
    }

    pub fn due_in(s: &State, project: Option<&str>, limit: usize, force: bool) -> Result<Vec<Value>> {
        let now = Ts::now();
        let mut busy = HashSet::new();
        for lease in s.values(T::Leases) {
            if lease.at("status").as_str() == Some("active") && state::timestamp(lease.at("expires"))? > now {
                if let Some(g) = lease.at("generations").as_obj() {
                    busy.extend(g.keys().cloned());
                }
            }
        }
        let mut posts: Vec<&Value> = s
            .values(T::Posts)
            .filter(|p| p.at("due").truthy() && !busy.contains(&state::py_str(p.at("id"))) && project.is_none_or(|pr| p.at("project").as_str() == Some(pr)))
            .collect();
        posts.sort_by_cached_key(|p| (!p.at("urgent").truthy(), state::py_str(p.at("created_at")), state::py_str(p.at("id"))));
        if !force && posts.len() < 5 && !posts.iter().any(|p| p.at("urgent").truthy()) {
            return Ok(Vec::new());
        }
        Ok(posts.into_iter().take(limit).map(|p| crate::obj! {"id" => p.at("id").clone(), "revision" => p.at("revision").clone()}).collect())
    }

    // ------------------------------------------------------------ briefing

    pub fn brief(&self, actor: &str, args: BriefArgs) -> Result<Value> {
        let BriefArgs { project, limit, offset, post_id, cursor, mut view, budget, mut item_id, mut item_kind } = args;
        require((1..=50).contains(&limit), "brief limit must be 1..50")?;
        require(offset >= 0, "offset must be nonnegative")?;
        require(cursor.is_none_or(|c| c.is_empty()) || offset == 0, "use cursor or offset, not both")?;
        require(BRIEF_VIEWS.contains(&view.as_str()), "brief view must be compact or full")?;
        if let Some(b) = budget {
            require((512..=1_000_000).contains(&b), "brief budget must be 512..1000000 bytes")?;
            require(view == "compact", "brief budget is only supported for compact view")?;
        }
        if let Some(pid) = post_id {
            require(item_id.is_none() && item_kind.as_deref().is_none_or(|k| k == "post"), "post_id cannot be combined with another item")?;
            item_id = Some(pid);
            item_kind = Some("post".into());
            if view == "compact" {
                view = "full".into();
            }
        }
        let reconciliation = self.sync(actor)?;
        let mut s = self.read_state()?;
        let actor_v = Value::str(actor);
        let skey = state::entity(&s, T::Sessions, &actor_v)?;
        let project = match project.filter(|p| !p.is_empty()) {
            Some(p) => p,
            None => state::py_str(s.get(T::Sessions, skey).unwrap().at("project")),
        };
        state::entity(&s, T::Projects, &Value::str(project.as_str()))?;
        if self.check_sources(actor, &project, 8)?.at("outcomes").truthy() {
            s = self.read_state()?;
        }
        let mut items: Vec<(&'static str, &Value)> = Vec::new();
        if let Some(item_id) = &item_id {
            require(item_kind.as_deref().is_none_or(|k| ["post", "task", "notice"].contains(&k)), "invalid item kind")?;
            let kinds: Vec<&'static str> = match item_kind.as_deref() {
                Some("post") => vec!["post"],
                Some("task") => vec!["task"],
                Some("notice") => vec!["notice"],
                _ => vec!["post", "task", "notice"],
            };
            let mut candidates = Vec::new();
            for kind in kinds {
                if let Some(item) = s.get(T::from_kind(kind).unwrap(), item_id) {
                    candidates.push((kind, item));
                }
            }
            require(candidates.len() == 1, format!("unknown or ambiguous item: {item_id}"))?;
            let (kind, item) = candidates[0];
            if kind == "notice" {
                require(item.at("recipient").py_eq(&actor_v), "notice belongs to another session")?;
            }
            items.push((kind, item));
        } else {
            let mut seen: HashSet<(String, String, String)> = HashSet::new();
            for d in s.values_where(T::Deliveries, "actor", |v| v.py_eq(&actor_v)) {
                if d.at("acked").truthy() {
                    for i in d.at("items").as_arr().map(|a| a.as_slice()).unwrap_or(&[]) {
                        seen.insert((state::py_str(i.at("kind")), state::py_str(i.at("id")), state::py_repr(i.at("revision"))));
                    }
                }
            }
            for n in s.values_where(T::Notices, "recipient", |v| v.py_eq(&actor_v)) {
                if !n.at("acked").truthy() {
                    items.push(("notice", n));
                }
            }
            for t in s.values(T::Tasks) {
                if !matches!(t.at("status").as_str(), Some("done") | Some("cancelled")) && (t.at("owner").py_eq(&actor_v) || t.at("project").as_str() == Some(&project)) {
                    items.push(("task", t));
                }
            }
            for p in s.values(T::Posts) {
                let to = p.at("to");
                let mine = to.py_eq(&actor_v) || (p.at("project").as_str() == Some(&project) && !to.truthy());
                if mine && !seen.contains(&("post".to_string(), state::py_str(p.at("id")), state::py_repr(p.at("revision")))) {
                    items.push(("post", p));
                }
            }
        }
        let item_key = |kind: &str, item: &Value| -> [String; 4] {
            let id = state::py_str(item.at("id"));
            match kind {
                "notice" => ["0".into(), "0".into(), String::new(), id],
                "task" => ["1".into(), if item.at("priority").as_str() == Some("urgent") { "0" } else { "1" }.into(), state::py_str(item.at("created_at")), id],
                _ => {
                    let urgency = if item.at("urgent").truthy() { "0" } else { "1" };
                    let direct = if item.at("to").py_eq(&actor_v) { "0" } else { "1" };
                    ["2".into(), format!("{urgency}{direct}"), state::py_str(item.at("created_at")), id]
                }
            }
        };
        let mut keyed: Vec<([String; 4], &'static str, &Value)> = items.into_iter().map(|(k, i)| (item_key(k, i), k, i)).collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some(c) = cursor.filter(|c| !c.is_empty()) {
            let after = (|| -> Option<[String; 4]> {
                if c.len() > 2000 {
                    return None;
                }
                let raw = pyfmt::b64url_decode(c)?;
                let value = json::parse(&raw, json::LENIENT).ok()?;
                if !(value.is_obj() && value.at("actor").as_str() == Some(actor) && value.at("project").as_str() == Some(&project)) {
                    return None;
                }
                let after = value.at("after").as_arr()?;
                if after.len() != 4 || !after.iter().all(|x| x.is_str()) {
                    return None;
                }
                Some([0, 1, 2, 3].map(|i| after[i].as_str().unwrap().to_string()))
            })()
            .ok_or_else(|| Error::Rejected("invalid briefing cursor".into()))?;
            keyed.retain(|k| k.0 > after);
        }
        let total = keyed.len();
        let start = (offset as usize).min(total);
        let end = (start + limit as usize).min(total);
        let mut selected: Vec<&([String; 4], &'static str, &Value)> = Vec::new();
        let mut rendered: Vec<Value> = Vec::new();
        let mut used = 2usize; // "[]"
        for entry in &keyed[start..end] {
            let (_, kind, item) = entry;
            let mut shown = render_item(kind, item, &view, COMPACT_PREVIEW_LIMIT)?;
            if let Some(b) = budget {
                let size = json::canonical(&shown).len();
                let next = used + size + if rendered.is_empty() { 0 } else { 1 };
                if next > b as usize {
                    if rendered.is_empty() {
                        shown = render_item(kind, item, &view, 128)?;
                        require(json::canonical(&shown).len() + 2 <= b as usize, "brief budget is too small for one compact item")?;
                        used += json::canonical(&shown).len();
                    } else {
                        break;
                    }
                } else {
                    used = next;
                }
            }
            selected.push(entry);
            rendered.push(shown);
        }
        let refs: Vec<Value> = selected.iter().map(|(_, kind, item)| crate::obj! {"kind" => *kind, "id" => item.at("id").clone(), "revision" => item.at("revision").clone()}).collect();
        let result = self.submit(self.request(actor, "delivery.prepare", crate::obj! {"items" => refs}))?;
        if result.at("status").as_str() != Some("accepted") {
            return Ok(result);
        }
        let has_more = !selected.is_empty() && start + selected.len() < total;
        let next_cursor = if has_more {
            let last = &selected.last().unwrap().0;
            let anchor = crate::obj! {"actor" => actor, "project" => project.clone(), "after" => Value::Arr(last.iter().map(|x| Value::Str(x.clone())).collect())};
            Value::Str(pyfmt::b64url_encode(&json::canonical(&anchor)))
        } else {
            Value::Null
        };
        let roster = presence::listing(&s, &Ts::now(), Some(actor), Some(&project), false);
        let due = Self::due_in(&s, Some(&project), 4, false)?;
        let returned = rendered.len();
        let mut out = result.as_obj().cloned().unwrap_or_default();
        out.insert("project", project);
        out.insert("items", Value::Arr(rendered));
        out.insert("total", total);
        out.insert("returned", returned);
        out.insert("view", view);
        out.insert("budget", budget.map(Value::Int).unwrap_or(Value::Null));
        out.insert("truncated", has_more);
        out.insert("reconciliation", Value::Arr(reconciliation));
        out.insert("next_offset", if has_more { Value::Int(offset + selected.len() as i64) } else { Value::Null });
        out.insert("next_cursor", next_cursor);
        for (k, v) in roster.0 {
            out.insert(k, v);
        }
        out.insert("moderation_due", Value::Arr(due));
        out.insert("instruction", INSTRUCTION);
        Ok(Value::Obj(out))
    }
}

/// The verify digests of two views.
/// The digests of both views. The first hashes on another thread while the
/// second encodes; identical encodings share one hash.
fn digest_pair(full: &View, view: &View) -> (String, String) {
    let first = full.canonical();
    std::thread::scope(|scope| {
        let hashing = scope.spawn(|| pyfmt::sha256_hex(&first));
        let second = view.canonical();
        let same = second == first;
        let found = if same { None } else { Some(pyfmt::sha256_hex(&second)) };
        let expected = hashing.join().expect("digest thread");
        let found = found.unwrap_or_else(|| expected.clone());
        crate::discard(second);
        (expected, found)
    })
}
