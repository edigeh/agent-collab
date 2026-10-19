//! Idempotent local installation of the Rust build and managed harness notes
//! (port of `install.py`).
//!
//! The prefix layout is the Python installer's: `collab` and `viewer`
//! launchers carrying the same markers and relocation lines, `current ->
//! releases/<id>`, a release `manifest.json`, `integration.json`, `backups/`
//! and the managed instruction blocks. A Rust release holds the three binaries
//! under `bin/` and the templates. Either installer can run over the other's
//! prefix, which is how a rollback works.

use crate::argparse::{parse_args, report, Spec, Val};
use crate::board::atomic_file;
use crate::json::{self, Map, Value};
use crate::obj;
use crate::pyfmt;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MARKER_BEGIN: &[u8] = b"<!-- agent-collab:begin v1 -->";
const MARKER_END: &[u8] = b"<!-- agent-collab:end -->";
const LAUNCHER_MARKER: &str = "# agent-collab managed launcher v1";
const VIEWER_MARKER: &str = "# agent-collab managed viewer v1";
const BINARIES: [&str; 3] = ["collab", "collab-install", "collab-viewer"];
const TEMPLATE_NAMES: [&str; 4] = ["codex", "claude", "pi", "omp"];
const INTEGRATION_FILE: &str = "integration.json";

const TEMPLATES: [(&str, &str); 4] = [
    ("claude", include_str!("../../templates/claude.md")),
    ("codex", include_str!("../../templates/codex.md")),
    ("omp", include_str!("../../templates/omp.md")),
    ("pi", include_str!("../../templates/pi.md")),
];

#[derive(Debug)]
pub struct InstallError(pub String);

type Result<T> = std::result::Result<T, InstallError>;

fn fail<T>(msg: impl Into<String>) -> Result<T> {
    Err(InstallError(msg.into()))
}

impl From<std::io::Error> for InstallError {
    fn from(e: std::io::Error) -> Self {
        InstallError(e.to_string())
    }
}

fn io_at(path: &Path) -> impl Fn(std::io::Error) -> InstallError + '_ {
    move |e| InstallError(pyfmt::with_path(e, path).to_string())
}

type Entries = Vec<(&'static str, PathBuf)>;

fn default_entries() -> Vec<(&'static str, PathBuf)> {
    let home = pyfmt::expanduser("~");
    vec![
        ("codex", home.join(".codex").join("AGENTS.md")),
        ("claude", home.join(".claude").join("CLAUDE.md")),
        ("pi", home.join(".pi").join("agent").join("AGENTS.md")),
        ("omp", home.join(".omp").join("agent").join("AGENTS.md")),
    ]
}

/// `_entries`: TEMPLATE_NAMES order, expanded and absolute.
fn entries(values: &[(String, PathBuf)]) -> Result<Entries> {
    if values.iter().any(|(n, _)| !TEMPLATE_NAMES.contains(&n.as_str())) {
        return fail(format!("entries must use only {}", TEMPLATE_NAMES.join(", ")));
    }
    let mut out = Vec::new();
    for name in TEMPLATE_NAMES {
        if let Some((_, p)) = values.iter().rev().find(|(n, _)| n == name) {
            out.push((name, pyfmt::pathlib_absolute(&pyfmt::expanduser(&pyfmt::path_str(p)))));
        }
    }
    Ok(out)
}

fn all_defaults() -> Result<Entries> {
    entries(&default_entries().into_iter().map(|(n, p)| (n.to_string(), p)).collect::<Vec<_>>())
}

fn same(a: &Entries, b: &Entries) -> bool {
    a.len() == b.len() && a.iter().all(|(n, p)| b.iter().any(|(m, q)| m == n && q == p))
}

fn saved_entries(prefix: &Path) -> Result<Option<Entries>> {
    let path = prefix.join(INTEGRATION_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let invalid = || InstallError(format!("invalid integration manifest: {}", pyfmt::path_str(&path)));
    let raw = fs::read(&path).map_err(|_| invalid())?;
    let data = json::parse(&raw, json::LENIENT).map_err(|_| invalid())?;
    if !data.at("version").py_eq(&Value::Int(1)) {
        return Err(invalid());
    }
    let Some(map) = data.at("entries").as_obj() else { return Err(invalid()) };
    let mut values = Vec::new();
    for (k, v) in map.iter() {
        let Some(s) = v.as_str() else { return Err(invalid()) };
        values.push((k.clone(), PathBuf::from(s)));
    }
    entries(&values).map(Some).map_err(|_| invalid())
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

fn select_entries(prefix: &Path, harnesses: Option<Vec<String>>) -> Result<Entries> {
    let saved = saved_entries(prefix)?;
    let current = prefix.join("current").exists();
    let chosen = if let Some(harnesses) = harnesses {
        let mut unknown: Vec<&String> = harnesses.iter().filter(|h| !TEMPLATE_NAMES.contains(&h.as_str())).collect();
        if !unknown.is_empty() {
            unknown.sort();
            unknown.dedup();
            return fail(format!("unknown harness: {}", unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
        }
        let defaults = default_entries();
        let picked: Vec<(String, PathBuf)> =
            harnesses.iter().map(|h| (h.clone(), defaults.iter().find(|(n, _)| n == h).unwrap().1.clone())).collect();
        entries(&picked)?
    } else if let Some(saved) = &saved {
        saved.clone()
    } else if current {
        all_defaults()?
    } else {
        let picked: Vec<(String, PathBuf)> = default_entries().into_iter().filter(|(n, _)| which(n).is_some()).map(|(n, p)| (n.to_string(), p)).collect();
        entries(&picked)?
    };
    if let Some(saved) = &saved {
        if !same(&chosen, saved) {
            return fail("installed harness selection differs; uninstall before changing it");
        }
    }
    if saved.is_none() && current && !same(&chosen, &all_defaults()?) {
        return fail("legacy installation manages all harnesses; uninstall before changing the selection");
    }
    Ok(chosen)
}

fn template_text(name: &str) -> &'static str {
    TEMPLATES.iter().find(|(n, _)| *n == name).map(|(_, t)| *t).unwrap_or("")
}

fn template(name: &str, prefix: &Path) -> Result<Vec<u8>> {
    let body = template_text(name).replace("{{PREFIX}}", &pyfmt::path_str(prefix));
    if body.contains("{{PREFIX}}") {
        return fail("unexpanded template placeholder");
    }
    let mut out = b"\n".to_vec();
    out.extend_from_slice(MARKER_BEGIN);
    out.push(b'\n');
    out.extend_from_slice(body.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(MARKER_END);
    out.push(b'\n');
    Ok(out)
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while let Some(at) = find(&hay[i..], needle) {
        n += 1;
        i += at + needle.len();
    }
    n
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn managed_span(raw: &[u8], path: &Path) -> Result<Option<(usize, usize)>> {
    let begins = count(raw, MARKER_BEGIN);
    let ends = count(raw, MARKER_END);
    if begins == 0 && ends == 0 {
        return Ok(None);
    }
    if begins != 1 || ends != 1 {
        return fail(format!("duplicate or malformed managed markers: {}", pyfmt::path_str(path)));
    }
    let begin = find(raw, MARKER_BEGIN).unwrap();
    let mut end = find(raw, MARKER_END).unwrap();
    if end < begin {
        return fail(format!("malformed managed marker order: {}", pyfmt::path_str(path)));
    }
    end += MARKER_END.len();
    if end < raw.len() && raw[end] == b'\n' {
        end += 1;
    }
    let start = if begin > 0 && raw[begin - 1] == b'\n' { begin - 1 } else { begin };
    Ok(Some((start, end)))
}

fn read_instruction(path: &Path) -> Result<(Vec<u8>, u32)> {
    if path.exists() {
        let link = fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false);
        if !path.is_file() || link {
            return fail(format!("instruction path is not a regular file: {}", pyfmt::path_str(path)));
        }
        let raw = fs::read(path).map_err(io_at(path))?;
        let mode = fs::metadata(path).map_err(io_at(path))?.permissions().mode() & 0o7777;
        return Ok((raw, mode));
    }
    Ok((Vec::new(), 0o600))
}

fn atomic_text(path: &Path, raw: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("/"));
    fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(io_at(parent))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (tmp, mut file) = loop {
        let mut rnd = [0u8; 8];
        pyfmt::random_bytes(&mut rnd);
        let suffix: String = rnd.iter().map(|b| (b"abcdefghijklmnopqrstuvwxyz0123456789_"[(*b as usize) % 37]) as char).collect();
        let tmp = parent.join(format!(".{name}.{suffix}"));
        match fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_at(&tmp)(e)),
        }
    };
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        file.write_all(raw)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, path)
    })();
    if tmp.exists() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(io_at(path))
}

struct FileRecord {
    path: String,
    data: Vec<u8>,
    mode: u32,
}

fn source_root(source: &str) -> Result<PathBuf> {
    let root = pyfmt::resolve_strict(&pyfmt::expanduser(source)).map_err(|e| InstallError(e.to_string()))?;
    if !BINARIES.iter().all(|b| root.join(b).is_file()) {
        return fail("source must contain the collab, collab-viewer and collab-install binaries");
    }
    Ok(root)
}

fn tree(source: &Path) -> Result<Vec<FileRecord>> {
    let mut files = Vec::new();
    for b in BINARIES {
        let path = source.join(b);
        let data = fs::read(&path).map_err(io_at(&path))?;
        let mode = fs::metadata(&path).map_err(io_at(&path))?.permissions().mode() & 0o7777;
        files.push(FileRecord { path: format!("bin/{b}"), data, mode });
    }
    for (name, text) in TEMPLATES {
        files.push(FileRecord { path: format!("templates/{name}.md"), data: text.as_bytes().to_vec(), mode: 0o644 });
    }
    Ok(files)
}

fn records(files: &[FileRecord]) -> Vec<Value> {
    files.iter().map(|f| obj! {"path" => f.path.clone(), "sha256" => pyfmt::sha256_hex(&f.data), "mode" => f.mode as i64}).collect()
}

fn release_id(records: &[Value]) -> String {
    pyfmt::sha256_hex(&json::canonical(&Value::Arr(records.to_vec())))[..24].to_string()
}

fn write_release(prefix: &Path, files: &[FileRecord], records: &[Value], release: &str) -> Result<PathBuf> {
    let releases = prefix.join("releases");
    let target = releases.join(release);
    if target.exists() {
        return Ok(target);
    }
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&releases).map_err(io_at(&releases))?;
    let staging = loop {
        let mut rnd = [0u8; 8];
        pyfmt::random_bytes(&mut rnd);
        let candidate = releases.join(format!(".release-{}", pyfmt::hex(&rnd)));
        match fs::DirBuilder::new().mode(0o700).create(&candidate) {
            Ok(()) => break candidate,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_at(&candidate)(e)),
        }
    };
    let result = (|| -> Result<PathBuf> {
        for f in files {
            let destination = staging.join(&f.path);
            let parent = destination.parent().unwrap();
            fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(io_at(parent))?;
            fs::write(&destination, &f.data).map_err(io_at(&destination))?;
            fs::set_permissions(&destination, fs::Permissions::from_mode(f.mode)).map_err(io_at(&destination))?;
        }
        let manifest = obj! {"version" => 1i64, "release" => release, "files" => records.to_vec()};
        let mut raw = json::canonical(&manifest);
        raw.push(b'\n');
        atomic_file(&staging.join("manifest.json"), &raw)?;
        if fs::rename(&staging, &target).is_err() && !target.exists() {
            return fail(format!("could not publish release {}", pyfmt::path_str(&target)));
        }
        Ok(target.clone())
    })();
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn set_current(prefix: &Path, release: &Path) -> Result<()> {
    let current = prefix.join("current");
    if fs::symlink_metadata(&current).is_ok_and(|m| m.file_type().is_symlink()) && pyfmt::resolve(&current) == pyfmt::resolve(release) {
        return Ok(());
    }
    let temporary = prefix.join(".current-new");
    let _ = fs::remove_file(&temporary);
    let relative = release.strip_prefix(prefix).map(Path::to_path_buf).unwrap_or_else(|_| release.to_path_buf());
    std::os::unix::fs::symlink(&relative, &temporary).map_err(io_at(&temporary))?;
    fs::rename(&temporary, &current).map_err(io_at(&current))?;
    Ok(())
}

/// `_managed_launcher_defaults`: board relocation kept across upgrades.
fn launcher_defaults(prefix: &Path) -> (Option<String>, Option<String>) {
    let Ok(raw) = fs::read(prefix.join("collab")) else { return (None, None) };
    let Ok(raw) = String::from_utf8(raw) else { return (None, None) };
    if !raw.contains(LAUNCHER_MARKER) {
        return (None, None);
    }
    let pick = |name: &str| -> Option<String> {
        let needle = format!("export {name}=${{{name}:-");
        let at = raw.find(&needle)? + needle.len();
        let rest = &raw[at..];
        let end = rest.find(['}', '\n'])?;
        if end == 0 || rest.as_bytes()[end] != b'}' {
            return None;
        }
        let value = pyfmt::py_strip(&rest[..end]).trim_matches(['\'', '"']);
        Some(value.to_string())
    };
    (pick("COLLAB_HOME"), pick("COLLAB_RECEIPTS"))
}

/// `shlex.quote`.
fn quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.bytes().all(|c| c.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn launcher(prefix: &Path, home: Option<&str>, receipts: Option<&str>) -> Vec<u8> {
    let p = pyfmt::path_str(prefix);
    let home = home.filter(|h| !h.is_empty()).map(str::to_string).unwrap_or_else(|| p.clone());
    let receipts = receipts.filter(|r| !r.is_empty()).map(str::to_string).unwrap_or_else(|| format!("{p}-receipts"));
    format!(
        "#!/bin/sh\n{LAUNCHER_MARKER}\nexport COLLAB_HOME=${{COLLAB_HOME:-{}}}\nexport COLLAB_RECEIPTS=${{COLLAB_RECEIPTS:-{}}}\nexec {}/current/bin/collab \"$@\"\n",
        quote(&home),
        quote(&receipts),
        quote(&p)
    )
    .into_bytes()
}

fn viewer_launcher(prefix: &Path, home: Option<&str>, receipts: Option<&str>) -> Vec<u8> {
    let p = pyfmt::path_str(prefix);
    let home = home.filter(|h| !h.is_empty()).map(str::to_string).unwrap_or_else(|| p.clone());
    let receipts = receipts.filter(|r| !r.is_empty()).map(str::to_string).unwrap_or_else(|| format!("{p}-receipts"));
    format!(
        "#!/bin/sh\n{VIEWER_MARKER}\nexport COLLAB_HOME=${{COLLAB_HOME:-{}}}\nexport COLLAB_RECEIPTS=${{COLLAB_RECEIPTS:-{}}}\nexec {}/bin/collab-viewer \"$@\"\n",
        quote(&home),
        quote(&receipts),
        quote(&pyfmt::path_str(&prefix.join("current")))
    )
    .into_bytes()
}

fn backup(prefix: &Path, name: &str, raw: &[u8], mode: u32) -> Result<()> {
    let key = &pyfmt::sha256_hex(raw)[..24];
    let destination = prefix.join("backups").join(name).join(format!("{key}.md"));
    if !destination.exists() {
        atomic_text(&destination, raw, mode)?;
    }
    Ok(())
}

struct Planned {
    name: &'static str,
    path: PathBuf,
    before: Vec<u8>,
    after: Vec<u8>,
    mode: u32,
}

fn instruction_plan(prefix: &Path, entries: &Entries) -> Result<Vec<Planned>> {
    let mut plan = Vec::new();
    for (name, path) in entries {
        let (raw, mode) = read_instruction(path)?;
        let span = managed_span(&raw, path)?;
        let block = template(name, prefix)?;
        let after = match span {
            None => [raw.as_slice(), block.as_slice()].concat(),
            Some((a, b)) => [&raw[..a], block.as_slice(), &raw[b..]].concat(),
        };
        plan.push(Planned { name, path: path.clone(), before: raw, after, mode });
    }
    Ok(plan)
}

fn prefix_path(prefix: &str) -> PathBuf {
    pyfmt::pathlib_absolute(&pyfmt::expanduser(prefix))
}

pub fn install(source: &str, prefix: &str, dry_run: bool, harnesses: Option<Vec<String>>) -> Result<Value> {
    let source = source_root(source)?;
    let prefix = prefix_path(prefix);
    let entries = select_entries(&prefix, harnesses)?;
    let plan = instruction_plan(&prefix, &entries)?;
    let files = tree(&source)?;
    let records = records(&files);
    let release = release_id(&records);
    let (home, receipts) = launcher_defaults(&prefix);
    let changes: Vec<Value> = plan.iter().filter(|p| p.before != p.after).map(|p| Value::from(pyfmt::path_str(&p.path))).collect();
    let result = obj! {
        "prefix" => pyfmt::path_str(&prefix), "release" => release.clone(), "launcher" => pyfmt::path_str(&prefix.join("collab")),
        "instruction_changes" => changes, "harnesses" => entries.iter().map(|(n, _)| Value::from(*n)).collect::<Vec<_>>(), "dry_run" => dry_run,
    };
    if dry_run {
        return Ok(result);
    }
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&prefix).map_err(io_at(&prefix))?;
    let target = write_release(&prefix, &files, &records, &release)?;
    set_current(&prefix, &target)?;
    let script = launcher(&prefix, home.as_deref(), receipts.as_deref());
    let path = prefix.join("collab");
    if fs::read(&path).ok().as_deref() != Some(script.as_slice()) {
        atomic_text(&path, &script, 0o700)?;
    }
    let script = viewer_launcher(&prefix, home.as_deref(), receipts.as_deref());
    let path = prefix.join("viewer");
    if fs::read(&path).ok().as_deref() != Some(script.as_slice()) {
        atomic_text(&path, &script, 0o700)?;
    }
    let mut map = Map::new();
    for (n, p) in &entries {
        map.push(*n, pyfmt::path_str(p));
    }
    let mut raw = json::canonical(&obj! {"version" => 1i64, "entries" => map});
    raw.push(b'\n');
    atomic_file(&prefix.join(INTEGRATION_FILE), &raw)?;
    for p in &plan {
        if p.before != p.after {
            backup(&prefix, p.name, &p.before, p.mode)?;
            atomic_text(&p.path, &p.after, p.mode)?;
        }
    }
    Ok(result)
}

fn has_marker(path: &Path, marker: &str) -> bool {
    path.is_file() && fs::read(path).is_ok_and(|raw| find(&raw, marker.as_bytes()).is_some())
}

fn chosen_or_saved(prefix: &Path) -> Result<Entries> {
    match saved_entries(prefix)? {
        Some(saved) => Ok(saved),
        None => all_defaults(),
    }
}

pub fn doctor(prefix: &str) -> Result<Value> {
    let prefix = prefix_path(prefix);
    let entries = chosen_or_saved(&prefix)?;
    let mut checks: Vec<Value> = Vec::new();
    let current = prefix.join("current");
    let manifest = (|| -> Option<Vec<Value>> {
        let raw = fs::read(current.join("manifest.json")).ok()?;
        let data = json::parse(&raw, json::LENIENT).ok()?;
        let files = data.at("files").as_arr()?;
        let mut out = Vec::new();
        for record in files {
            let rel = record.get("path")?.as_str()?;
            let sha = record.get("sha256")?;
            let path = current.join(rel);
            let ok = path.is_file() && fs::read(&path).is_ok_and(|b| Value::from(pyfmt::sha256_hex(&b)).py_eq(sha));
            out.push(obj! {"name" => format!("source:{rel}"), "ok" => ok});
        }
        Some(out)
    })();
    match manifest {
        Some(found) => checks.extend(found),
        None => checks.push(obj! {"name" => "source-manifest", "ok" => false}),
    }
    checks.push(obj! {"name" => "launcher", "ok" => has_marker(&prefix.join("collab"), LAUNCHER_MARKER)});
    checks.push(obj! {"name" => "viewer-launcher", "ok" => has_marker(&prefix.join("viewer"), VIEWER_MARKER)});
    for (name, path) in &entries {
        let ok = match read_instruction(path) {
            Ok((raw, _)) => matches!(managed_span(&raw, path), Ok(Some(_))),
            Err(_) => false,
        };
        checks.push(obj! {"name" => format!("instruction:{name}"), "ok" => ok});
    }
    let ok = checks.iter().all(|c| c.at("ok").truthy());
    let mut harnesses = Map::new();
    for (name, _) in &entries {
        harnesses.push(*name, which(name).is_some());
    }
    Ok(obj! {"prefix" => pyfmt::path_str(&prefix), "ok" => ok, "checks" => checks, "harnesses" => harnesses})
}

pub fn uninstall(prefix: &str, dry_run: bool) -> Result<Value> {
    let prefix = prefix_path(prefix);
    let entries = chosen_or_saved(&prefix)?;
    let mut plan = Vec::new();
    for (name, path) in &entries {
        let (raw, mode) = read_instruction(path)?;
        let after = match managed_span(&raw, path)? {
            None => raw.clone(),
            Some((a, b)) => [&raw[..a], &raw[b..]].concat(),
        };
        plan.push(Planned { name, path: path.clone(), before: raw, after, mode });
    }
    let launcher_path = prefix.join("collab");
    let remove_launcher = has_marker(&launcher_path, LAUNCHER_MARKER);
    let viewer_path = prefix.join("viewer");
    let remove_viewer = has_marker(&viewer_path, VIEWER_MARKER);
    let changes: Vec<Value> = plan.iter().filter(|p| p.before != p.after).map(|p| Value::from(pyfmt::path_str(&p.path))).collect();
    let result = obj! {
        "prefix" => pyfmt::path_str(&prefix), "instruction_changes" => changes,
        "remove_launcher" => remove_launcher, "remove_viewer" => remove_viewer, "dry_run" => dry_run,
    };
    if dry_run {
        return Ok(result);
    }
    for p in &plan {
        if p.before != p.after {
            backup(&prefix, p.name, &p.before, p.mode)?;
            atomic_text(&p.path, &p.after, p.mode)?;
        }
    }
    if remove_launcher {
        fs::remove_file(&launcher_path).map_err(io_at(&launcher_path))?;
    }
    if remove_viewer {
        fs::remove_file(&viewer_path).map_err(io_at(&viewer_path))?;
    }
    let integration = prefix.join(INTEGRATION_FILE);
    match fs::remove_file(&integration) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_at(&integration)(e)),
    }
    Ok(result)
}

fn spec() -> Spec {
    let mut p = Spec::new("@install");
    p.store("--prefix").default("~/.agent-collab");
    p.flag("--json");
    let mut put = Spec::new("@install install");
    put.pos("source");
    put.flag("--dry-run");
    put.append("--harness").choices(&TEMPLATE_NAMES).group(1);
    put.flag("--no-harnesses").group(1);
    // argparse leaves an unused append option at its default None.
    put.actions.iter_mut().find(|a| a.dest == "harness").unwrap().default = Val::None;
    let doctor = Spec::new("@install doctor");
    let mut remove = Spec::new("@install uninstall");
    remove.flag("--dry-run");
    p.subparsers("command", vec![("install", put), ("doctor", doctor), ("uninstall", remove)]);
    p
}

pub fn main(argv: Vec<String>) -> i32 {
    let parsed = match parse_args(&spec(), &argv) {
        Ok(p) => p,
        Err(e) => return report(|path| path.replacen("@install", "collab-install", 1), e),
    };
    let s = |k: &str| match parsed.ns.get(k) {
        Some(Val::Str(v)) => v.clone(),
        _ => String::new(),
    };
    let b = |k: &str| matches!(parsed.ns.get(k), Some(Val::Bool(true)));
    let prefix = s("prefix");
    let result = match parsed.path.first().copied() {
        Some("install") => {
            let harnesses = if b("no_harnesses") {
                Some(Vec::new())
            } else {
                match parsed.ns.get("harness") {
                    Some(Val::List(l)) => Some(l.clone()),
                    _ => None,
                }
            };
            install(&s("source"), &prefix, b("dry_run"), harnesses)
        }
        Some("doctor") => doctor(&prefix),
        _ => uninstall(&prefix, b("dry_run")),
    };
    let result = match result {
        Ok(r) => r,
        Err(InstallError(message)) => {
            eprintln!("collab-install: error: {message}");
            return 1;
        }
    };
    let text = if b("json") {
        json::dumps(&result, json::Style { sort_keys: true, ensure_ascii: true, compact: false, allow_nan: true })
    } else {
        crate::state::py_repr(&result)
    };
    println!("{text}");
    if result.get("ok").is_none_or(|ok| ok.truthy()) {
        0
    } else {
        1
    }
}
