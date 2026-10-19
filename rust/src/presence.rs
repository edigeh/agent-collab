//! Who is online, decided at read time from recorded activity and a process
//! check (`presence.py`). The process check reads the process table natively
//! instead of running `ps`, but produces the same `pid` and `lstart` text, so
//! sessions registered by either build are recognised by both.

use crate::json::{Map, Value};
use crate::pyfmt::{self, Ts};
use crate::state::{self, State, T};
use std::collections::{BTreeSet, HashMap, HashSet};

pub const ACTIVE: f64 = 900.0;
pub const FALLBACK_IDLE: f64 = 7200.0;
pub const LEFT_WINDOW: f64 = 8.0 * 3600.0;
pub const LIMIT: usize = 20;
pub const SCOPE_LIMIT: usize = 3;
const PASSTHROUGH: [&str; 19] = [
    "sh", "bash", "zsh", "dash", "ksh", "mksh", "fish", "tcsh", "csh", "env", "timeout", "gtimeout", "nice", "nohup", "time", "xargs",
    "sandbox-exec", "script", "caffeinate",
];

fn rank(p: &str) -> u8 {
    match p {
        "active" => 0,
        "idle" => 1,
        _ => 2,
    }
}

// ---------------------------------------------------------------- processes

/// One process: parent pid, start time as `ps -o lstart` prints it (C locale), command name.
pub struct Proc {
    pub ppid: i64,
    pub started: String,
    pub comm: String,
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// `ps -o lstart` text for a start time in seconds since the epoch, in local time,
/// with runs of spaces collapsed the way `' '.join(fields[:5])` does.
pub fn lstart(secs: i64) -> String {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!(
        "{} {} {} {:02}:{:02}:{:02} {}",
        DAYS[tm.tm_wday.clamp(0, 6) as usize],
        MONTHS[tm.tm_mon.clamp(0, 11) as usize],
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        tm.tm_year as i64 + 1900
    )
}

#[cfg(target_os = "linux")]
mod native {
    use super::Proc;
    use std::sync::OnceLock;

    fn boot_time() -> Option<i64> {
        static BOOT: OnceLock<Option<i64>> = OnceLock::new();
        *BOOT.get_or_init(|| {
            let stat = std::fs::read_to_string("/proc/stat").ok()?;
            stat.lines().find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse().ok()))
        })
    }

    pub fn available() -> bool {
        boot_time().is_some() && std::path::Path::new("/proc/self/stat").exists()
    }

    pub fn process(pid: i64) -> Option<Proc> {
        if pid <= 0 {
            return None;
        }
        let raw = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        let open = raw.iter().position(|&b| b == b'(')?;
        let close = raw.iter().rposition(|&b| b == b')')?;
        let comm = String::from_utf8_lossy(&raw[open + 1..close]).into_owned();
        let rest = std::str::from_utf8(&raw[close + 1..]).ok()?;
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // After the command: state(0) ppid(1) ... starttime(19), in clock ticks since boot.
        let ppid: i64 = fields.get(1)?.parse().ok()?;
        let ticks: u64 = fields.get(19)?.parse().ok()?;
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
        let started = boot_time()? + (ticks / hz) as i64;
        Some(Proc { ppid, started: super::lstart(started), comm })
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::Proc;

    pub fn available() -> bool {
        true
    }

    /// `proc_pidinfo(PROC_PIDTBSDINFO)`: `Err(())` when the process exists but
    /// cannot be inspected (another user's), `Ok(None)` when there is none.
    fn bsdinfo(pid: i64) -> Result<Option<libc::proc_bsdinfo>, ()> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let rc = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut libc::c_void, size) };
        if rc == size && info.pbi_pid as i64 == pid {
            return Ok(Some(info));
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Ok(None),
            _ => Err(()),
        }
    }

    /// argv[0] from KERN_PROCARGS2, which is what `ps -o comm` prints.
    fn argv0(pid: i64) -> Option<String> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        let mut size: libc::size_t = 0;
        if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) } != 0 || size < 8 {
            return None;
        }
        let mut buf = vec![0u8; size];
        if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr() as *mut libc::c_void, &mut size, std::ptr::null_mut(), 0) } != 0 {
            return None;
        }
        buf.truncate(size);
        let mut i = 4; // argc
        while i < buf.len() && buf[i] != 0 {
            i += 1; // exec path
        }
        while i < buf.len() && buf[i] == 0 {
            i += 1;
        }
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        (i > start).then(|| String::from_utf8_lossy(&buf[start..i]).into_owned())
    }

    pub fn process(pid: i64) -> Option<Proc> {
        if pid <= 0 {
            return None;
        }
        let info = match bsdinfo(pid) {
            Ok(info) => info?,
            Err(()) => return from_ps(pid),
        };
        let comm = argv0(pid).unwrap_or_else(|| {
            let raw: Vec<u8> = info.pbi_comm.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
            String::from_utf8_lossy(&raw).into_owned()
        });
        Some(Proc { ppid: info.pbi_ppid as i64, started: super::lstart(info.pbi_start_tvsec as i64), comm })
    }

    /// One process as `ps` reports it, for processes this user cannot inspect.
    fn from_ps(pid: i64) -> Option<Proc> {
        let out = super::ps(&["-o", "pid=,ppid=,lstart=,comm=", "-p", &pid.to_string()])?;
        out.lines().find_map(|line| {
            let parts = crate::pyfmt::py_split(line);
            if parts.len() >= 8 && parts[0].parse::<i64>().ok()? == pid {
                Some(Proc { ppid: parts[1].parse().ok()?, started: parts[2..7].join(" "), comm: parts[7..].join(" ") })
            } else {
                None
            }
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod native {
    use super::Proc;
    pub fn available() -> bool {
        false
    }
    pub fn process(_pid: i64) -> Option<Proc> {
        None
    }
}

/// `ps` itself, for platforms without a native reader (same calls as Python).
fn ps(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(args)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn probe_rows(output: &str) -> HashMap<i64, String> {
    let mut rows = HashMap::new();
    for line in output.lines() {
        let parts = pyfmt::py_split(line);
        if parts.len() >= 6 && parts[0].bytes().all(|c| c.is_ascii_digit()) {
            if let Ok(pid) = parts[0].parse() {
                rows.insert(pid, parts[1..6].join(" "));
            }
        }
    }
    rows
}

/// The start text of each pid that is running.
fn starts(pids: &[i64]) -> Option<HashMap<i64, String>> {
    if native::available() {
        return Some(pids.iter().filter_map(|&p| native::process(p).map(|proc_| (p, proc_.started))).collect());
    }
    let list: Vec<String> = pids.iter().map(|p| p.to_string()).collect();
    ps(&["-o", "pid=,lstart=", "-p", &list.join(",")]).map(|o| probe_rows(&o))
}

fn basename_comm(comm: &str) -> String {
    let joined = pyfmt::py_split(comm).join(" ");
    let base = joined.rsplit('/').next().unwrap_or("");
    base.trim_start_matches('-').to_string()
}

/// Identify the long-lived harness process above this CLI call, or None.
pub fn detect_host(harness: &str) -> Option<Value> {
    if let Ok(over) = std::env::var("COLLAB_HOST_PID") {
        let trimmed = pyfmt::py_strip(&over);
        let pid: i64 = if !trimmed.is_empty() && trimmed.bytes().all(|c| c.is_ascii_digit()) { trimmed.parse().unwrap_or(0) } else { 0 };
        if pid <= 1 {
            return None;
        }
        let started = starts(&[pid]).and_then(|m| m.get(&pid).cloned())?;
        return Some(crate::obj! {"pid" => pid, "started" => started});
    }
    let mut chain: Vec<(i64, String, String)> = Vec::new();
    let mut pid = unsafe { libc::getppid() } as i64;
    if native::available() {
        while pid > 1 && chain.len() < 32 {
            let Some(p) = native::process(pid) else { break };
            chain.push((pid, p.started, basename_comm(&p.comm)));
            pid = p.ppid;
        }
    } else {
        let mut table: HashMap<i64, (i64, String, String)> = HashMap::new();
        for line in ps(&["-A", "-o", "pid=,ppid=,lstart=,comm="]).unwrap_or_default().lines() {
            let parts = pyfmt::py_split(line);
            if parts.len() >= 8 && parts[0].bytes().all(|c| c.is_ascii_digit()) && parts[1].bytes().all(|c| c.is_ascii_digit()) {
                let name = basename_comm(&parts[7..].join(" "));
                if let (Ok(a), Ok(b)) = (parts[0].parse(), parts[1].parse()) {
                    table.insert(a, (b, parts[2..7].join(" "), name));
                }
            }
        }
        while let Some((parent, started, name)) = table.get(&pid).cloned() {
            if pid <= 1 || chain.len() >= 32 {
                break;
            }
            chain.push((pid, started, name));
            pid = parent;
        }
    }
    let chosen = chain.iter().find(|(_, _, n)| n == harness).or_else(|| chain.iter().find(|(_, _, n)| !PASSTHROUGH.contains(&n.as_str())))?;
    Some(crate::obj! {"pid" => chosen.0, "started" => chosen.1.clone()})
}

/// The (pid, start) pairs still running, or None when the process table cannot be read.
pub fn running(hosts: &[(i64, String)]) -> Option<HashSet<(i64, String)>> {
    let own = std::process::id() as i64;
    let mut pids: Vec<i64> = hosts.iter().map(|h| h.0).collect();
    pids.push(own);
    pids.sort();
    pids.dedup();
    let rows = starts(&pids)?;
    if !rows.contains_key(&own) {
        return None;
    }
    Some(rows.into_iter().collect())
}

// ---------------------------------------------------------------- listing

fn classify(session: &Value, quiet: f64, live: &Option<HashSet<(i64, String)>>, superseded: bool) -> Option<&'static str> {
    if session.get("left").is_some() {
        return (quiet < LEFT_WINDOW).then_some("left");
    }
    if quiet < ACTIVE {
        return Some("active");
    }
    if session.at("parent").truthy() || superseded {
        return None;
    }
    let host = session.at("host");
    if host.truthy() {
        if let Some(live) = live {
            let key = (host.at("pid").as_int().unwrap_or(-1), state::py_str(host.at("started")));
            return live.contains(&key).then_some("idle");
        }
    }
    (quiet < FALLBACK_IDLE).then_some("idle")
}

fn process_key(session: &Value) -> Option<(String, String, String)> {
    let host = session.at("host");
    if host.truthy() && !session.at("parent").truthy() {
        Some((state::py_str(session.at("harness")), state::py_repr(host.at("pid")), state::py_str(host.at("started"))))
    } else {
        None
    }
}

fn seconds_of(v: &Value) -> f64 {
    state::timestamp(v).map(|t| t.seconds()).unwrap_or(f64::NEG_INFINITY)
}

pub struct Seen<'a> {
    pub session: &'a Value,
    pub presence: &'static str,
    pub quiet: f64,
}

/// (session, presence, quiet seconds) for visible sessions, most present first.
pub fn survey<'a>(state: &'a State, now: &Ts) -> Vec<Seen<'a>> {
    let sessions: Vec<&Value> = state.values(T::Sessions).collect();
    let quiet: Vec<f64> = sessions.iter().map(|s| state::timestamp(s.at("seen_at")).map(|t| now.diff_seconds(&t)).unwrap_or(f64::INFINITY)).collect();
    let mut newest: HashMap<(String, String, String), f64> = HashMap::new();
    for s in &sessions {
        if let Some(k) = process_key(s) {
            let at = seconds_of(s.at("host").at("at"));
            let e = newest.entry(k).or_insert(0.0);
            if at > *e {
                *e = at;
            }
        }
    }
    let yielded: Vec<bool> = sessions
        .iter()
        .map(|s| process_key(s).is_some_and(|k| newest[&k] > seconds_of(s.at("seen_at"))))
        .collect();
    let hosts: Vec<(i64, String)> = sessions
        .iter()
        .enumerate()
        .filter(|(i, s)| process_key(s).is_some() && s.get("left").is_none() && !yielded[*i] && quiet[*i] >= ACTIVE)
        .map(|(_, s)| (s.at("host").at("pid").as_int().unwrap_or(-1), state::py_str(s.at("host").at("started"))))
        .collect();
    let live = if hosts.is_empty() { Some(HashSet::new()) } else { running(&hosts) };
    let mut seen: Vec<Seen> = Vec::new();
    for (i, s) in sessions.iter().enumerate() {
        if let Some(p) = classify(s, quiet[i], &live, yielded[i]) {
            seen.push(Seen { session: s, presence: p, quiet: quiet[i] });
        }
    }
    seen.sort_by(|a, b| {
        rank(a.presence)
            .cmp(&rank(b.presence))
            .then(a.quiet.partial_cmp(&b.quiet).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| state::py_str(a.session.at("id")).cmp(&state::py_str(b.session.at("id"))))
    });
    seen
}

fn relative(path: &str, roots: &[String]) -> String {
    for root in roots {
        let base = root.trim_end_matches('/');
        if path == base {
            return ".".into();
        }
        if path.starts_with(base) && path[base.len()..].starts_with('/') {
            return path[base.len() + 1..].to_string();
        }
    }
    path.to_string()
}

/// Paths of the session's open tasks, relative to its project roots.
pub fn radar(state: &State, session: &Value) -> Vec<String> {
    let project = session.at("project");
    let mut roots: Vec<String> = project
        .as_str()
        .and_then(|p| state.get(T::Projects, p))
        .and_then(|p| p.at("roots").as_arr().cloned())
        .unwrap_or_default()
        .iter()
        .map(state::py_str)
        .collect();
    roots.sort_by_key(|r| std::cmp::Reverse(r.chars().count()));
    let mut paths = BTreeSet::new();
    for task in state.values(T::Tasks) {
        if task.at("owner").py_eq(session.at("id")) && task.at("project").py_eq(project) && !matches!(task.at("status").as_str(), Some("done") | Some("cancelled")) {
            for p in task.at("scope").as_arr().map(|a| a.as_slice()).unwrap_or(&[]) {
                paths.insert(state::py_str(p));
            }
        }
    }
    paths.into_iter().map(|p| relative(&p, &roots)).collect()
}

/// One compact line of presence; empty fields are omitted to save context.
pub fn entry(state: &State, session: &Value, presence: &str, quiet: f64, mine: &BTreeSet<String>, full: bool, project: bool) -> Map {
    let empty = Value::Obj(Map::new());
    let status = if session.at("status").truthy() { session.at("status") } else { &empty };
    let mut item = Map::new();
    item.push("id", session.at("id").clone());
    item.push("harness", session.at("harness").clone());
    item.push("presence", presence);
    item.push("ago", ((quiet / 60.0).floor() as i64).max(0));
    if session.at("parent").truthy() {
        item.push("parent", session.at("parent").clone());
    }
    if project {
        let pid = session.at("project");
        let name = pid.as_str().and_then(|p| state.get(T::Projects, p)).and_then(|p| p.get("name").cloned()).unwrap_or_else(|| pid.clone());
        item.push("project", name);
    }
    if status.at("doing").truthy() {
        item.push("doing", status.at("doing").clone());
    }
    let left = session.at("left");
    let summary = if left.truthy() && left.at("summary").truthy() { left.at("summary") } else { status.at("summary") };
    if full && summary.truthy() {
        item.push("summary", summary.clone());
    }
    if presence != "left" && status.at("uses").truthy() {
        item.push("uses", status.at("uses").clone());
        let uses: BTreeSet<String> = status.at("uses").as_arr().map(|a| a.iter().map(state::py_str).collect()).unwrap_or_default();
        let clash: Vec<Value> = uses.intersection(mine).map(|s| Value::Str(s.clone())).collect();
        if !clash.is_empty() {
            item.push("clash", Value::Arr(clash));
        }
    }
    if full {
        let paths = radar(state, session);
        if !paths.is_empty() {
            item.push("scope", Value::Arr(paths.iter().take(SCOPE_LIMIT).map(|p| Value::Str(p.clone())).collect()));
        }
        if paths.len() > SCOPE_LIMIT {
            item.push("scope_more", paths.len() - SCOPE_LIMIT);
        }
    }
    item
}

/// Group presence for one reader: its project in full, other projects briefly, recent hand-offs.
pub fn listing(state: &State, now: &Ts, actor: Option<&str>, project: Option<&str>, you: bool) -> Map {
    let empty = Value::Obj(Map::new());
    let me = actor.and_then(|a| state.get(T::Sessions, a)).unwrap_or(&empty);
    let mine: BTreeSet<String> = if me.get("left").is_none() {
        let status = me.at("status");
        status.at("uses").as_arr().map(|a| a.iter().map(state::py_str).collect()).unwrap_or_default()
    } else {
        BTreeSet::new()
    };
    let seen = survey(state, now);
    let online: HashSet<String> = seen.iter().filter(|s| s.presence != "left").map(|s| state::py_str(s.session.at("id"))).collect();
    let mut peers: Vec<Map> = Vec::new();
    let mut elsewhere: Vec<Map> = Vec::new();
    let mut left: Vec<Map> = Vec::new();
    let mut folded: HashMap<String, usize> = HashMap::new();
    for s in &seen {
        let sid = state::py_str(s.session.at("id"));
        if Some(sid.as_str()) == actor {
            continue;
        }
        let here = project.is_none() || s.session.at("project").as_str() == project;
        if s.presence == "left" {
            if here {
                left.push(entry(state, s.session, s.presence, s.quiet, &mine, true, project.is_none()));
            }
        } else if here {
            peers.push(entry(state, s.session, s.presence, s.quiet, &mine, true, project.is_none()));
        } else if s.session.at("parent").as_str().is_some_and(|p| online.contains(p)) {
            *folded.entry(s.session.at("parent").as_str().unwrap().to_string()).or_default() += 1;
        } else {
            elsewhere.push(entry(state, s.session, s.presence, s.quiet, &mine, false, true));
        }
    }
    for item in peers.iter_mut().chain(elsewhere.iter_mut()) {
        let id = state::py_str(item.get("id").unwrap_or(&Value::Null));
        if let Some(n) = folded.get(&id) {
            item.insert("subagents", *n);
        }
    }
    let mut result = Map::new();
    let peer_count = peers.len();
    result.push("peers", Value::Arr(peers.into_iter().take(LIMIT).map(Value::Obj).collect()));
    result.push("peer_count", peer_count);
    if !elsewhere.is_empty() {
        result.push("elsewhere", Value::Arr(elsewhere.into_iter().take(LIMIT).map(Value::Obj).collect()));
    }
    if !left.is_empty() {
        result.push("left", Value::Arr(left.into_iter().take(LIMIT).map(Value::Obj).collect()));
    }
    if you && me.as_obj().is_some_and(|m| !m.is_empty()) {
        let own = entry(state, me, "active", 0.0, &BTreeSet::new(), true, false);
        let mut you_map = Map::new();
        for (k, v) in own.iter() {
            if k != "presence" && k != "ago" {
                you_map.push(k.clone(), v.clone());
            }
        }
        result.push("you", Value::Obj(you_map));
    }
    result
}

/// Every visible session for the local viewer, flagging resources two agents hold.
pub fn everyone(state: &State, now: &Ts) -> Vec<Value> {
    let seen = survey(state, now);
    let mut holders: HashMap<String, usize> = HashMap::new();
    for s in &seen {
        if s.presence != "left" {
            for u in s.session.at("status").at("uses").as_arr().map(|a| a.as_slice()).unwrap_or(&[]) {
                *holders.entry(state::py_str(u)).or_default() += 1;
            }
        }
    }
    let shared: BTreeSet<String> = holders.into_iter().filter(|(_, c)| *c > 1).map(|(u, _)| u).collect();
    seen.iter()
        .map(|s| {
            let mut e = entry(state, s.session, s.presence, s.quiet, &shared, true, false);
            e.insert("project", s.session.at("project").clone());
            Value::Obj(e)
        })
        .collect()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn native_start_times_match_ps() {
        let table = ps(&["-A", "-o", "pid=,ppid=,lstart="]).expect("ps runs");
        let mut compared = 0;
        for line in table.lines() {
            let parts = pyfmt::py_split(line);
            let (Ok(pid), Ok(ppid)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) else { continue };
            let Some(p) = native::process(pid) else { continue }; // exited since
            assert_eq!(p.ppid, ppid, "pid {pid}");
            assert_eq!(p.started, parts[2..7].join(" "), "pid {pid}");
            compared += 1;
        }
        assert!(compared > 0);
    }
}
