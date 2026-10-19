//! Command-line interface (port of `cli.py`), including an argparse emulation
//! that reproduces the Python parser's grammar, prefix matching and error text.

use crate::board::{self, Board, BriefArgs};
use crate::error::{Error, Result};
use crate::json::{self, Map, Value};
use crate::obj;
use crate::presence;
use crate::pyfmt::{self, Ts};
use crate::state::{self, HARNESSES, T};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use crate::argparse::{parse_args, py_int, Exit, Spec, Val};

const DECLARE: &str = "doing 'WHAT YOU ARE WORKING ON' --summary 'SHORT SUMMARY'";

fn cli_error<R>(msg: impl Into<String>) -> Result<R> {
    Err(Error::Cli(msg.into()))
}

const VIEWS: &[&str] = &["compact", "full"];

fn build_parser() -> Spec {
    let mut p = Spec::new("");
    p.store("--home").default(&std::env::var("COLLAB_HOME").unwrap_or_else(|_| "~/.agent-collab".into()));
    p.store("--receipts").default_opt(std::env::var("COLLAB_RECEIPTS").ok());
    p.store("--session").default_opt(std::env::var("COLLAB_SESSION").ok());
    p.flag("--json");

    let mut wake = Spec::new("wake");
    wake.store("--harness").req().choices(&HARNESSES);
    wake.store("--project");
    wake.store("--parent");
    wake.store("--name");
    wake.store("--limit").int(Some(12));
    wake.store("--view").choices(VIEWS).default("compact");
    wake.store("--budget").int(Some(board::DEFAULT_BRIEF_BUDGET));
    wake.store("--doing");
    wake.store("--summary");
    let mut doing = Spec::new("doing");
    doing.pos("doing");
    doing.store("--summary").req();
    wake.append("--uses");
    doing.append("--uses");
    let mut bye = Spec::new("bye");
    bye.store("--summary");
    let mut who = Spec::new("who");
    who.store("--project");
    let sync = Spec::new("sync");
    let verify = Spec::new("verify");

    let mut project = Spec::new("project");
    let plist = Spec::new("project list");
    let mut create = Spec::new("project create");
    create.pos("name");
    create.pos("id");
    create.append("--root");
    let mut attach = Spec::new("project attach");
    attach.pos("project");
    attach.pos("root");
    attach.store("--expected").req().int(None);
    project.subparsers("project_command", vec![("list", plist), ("create", create), ("attach", attach)]);

    let mut post = Spec::new("post");
    post.pos("text");
    post.store("--kind").choices(&["message", "finding", "request", "summary", "work"]).default("message");
    post.store("--project");
    post.store("--to");
    post.append("--source");
    post.append("--ref");
    post.append("--scope");
    let mut read = Spec::new("read");
    read.pos("item");
    read.store("--kind").choices(&["post", "task", "notice"]);
    read.store("--view").choices(VIEWS).default("full");
    let mut inbox = Spec::new("inbox");
    inbox.store("--project");
    inbox.store("--limit").int(Some(12));
    inbox.store("--offset").int(Some(0));
    inbox.store("--cursor");
    inbox.store("--view").choices(VIEWS).default("compact");
    inbox.store("--budget").int(Some(board::DEFAULT_BRIEF_BUDGET));
    let mut ack = Spec::new("ack");
    ack.pos("delivery");

    let mut task = Spec::new("task");
    let mut tlist = Spec::new("task list");
    tlist.store("--project");
    tlist.store("--view").choices(VIEWS).default("full");
    let mut tc = Spec::new("task create");
    tc.pos("title");
    tc.store("--owner");
    tc.store("--project");
    tc.append("--scope");
    tc.append("--dependency");
    tc.store("--priority").choices(&["normal", "urgent"]).default("normal");
    let mut claim = Spec::new("task claim");
    claim.pos("id");
    claim.store("--expected").req().int(None);
    claim.flag("--takeover");
    claim.append("--source");
    let mut finish = Spec::new("task finish");
    finish.pos("id");
    finish.store("--expected").req().int(None);
    finish.append("--source");
    finish.store("--resolution").choices(&["corrected", "mistaken"]);
    let mut subs = vec![("list", tlist), ("create", tc), ("claim", claim), ("finish", finish)];
    for action in ["block", "cancel"] {
        let mut s = Spec::new(if action == "block" { "task block" } else { "task cancel" });
        s.pos("id");
        s.store("--expected").req().int(None);
        s.store("--reason").req();
        subs.push((action, s));
    }
    task.subparsers("task_command", subs);

    let mut report = Spec::new("report");
    report.pos("post");
    report.store("--expected").req().int(None);
    report.store("--reason").req();
    report.append("--source");
    let mut status = Spec::new("status");
    status.store("--project");
    status.store("--view").choices(VIEWS).default("full");
    let mut artifact = Spec::new("artifact");
    artifact.pos("sha");
    artifact.flag("--display").group(1);
    artifact.flag("--path").group(1);
    let mut moderate = Spec::new("moderate");
    moderate.store("--project");
    moderate.flag("--force");
    let mut cleanup = Spec::new("cleanup");
    let mut preview = Spec::new("cleanup preview");
    preview.pos("sha");
    let mut remove = Spec::new("cleanup remove");
    remove.pos("sha");
    remove.store("--confirm").req();
    let mut resume = Spec::new("cleanup resume");
    resume.pos("removal");
    cleanup.subparsers("cleanup_command", vec![("preview", preview), ("remove", remove), ("resume", resume)]);

    p.subparsers(
        "command",
        vec![
            ("wake", wake),
            ("doing", doing),
            ("bye", bye),
            ("who", who),
            ("sync", sync),
            ("verify", verify),
            ("project", project),
            ("post", post),
            ("read", read),
            ("inbox", inbox),
            ("ack", ack),
            ("task", task),
            ("report", report),
            ("status", status),
            ("artifact", artifact),
            ("moderate", moderate),
            ("cleanup", cleanup),
        ],
    );
    p
}

// ====================================================================== namespace

struct Args {
    ns: HashMap<&'static str, Val>,
}

impl Args {
    fn s(&self, k: &str) -> Option<&str> {
        match self.ns.get(k) {
            Some(Val::Str(s)) => Some(s.as_str()),
            _ => None,
        }
    }
    /// The value when it is truthy, like Python's `args.x or ...`.
    fn t(&self, k: &str) -> Option<&str> {
        self.s(k).filter(|s| !s.is_empty())
    }
    fn b(&self, k: &str) -> bool {
        matches!(self.ns.get(k), Some(Val::Bool(true)))
    }
    fn list(&self, k: &str) -> Vec<String> {
        match self.ns.get(k) {
            Some(Val::List(l)) => l.clone(),
            _ => Vec::new(),
        }
    }
    fn int(&self, k: &str) -> Value {
        match self.ns.get(k) {
            Some(Val::Int(v)) => v.clone(),
            _ => Value::Null,
        }
    }
    fn small(&self, k: &str) -> i64 {
        match self.ns.get(k) {
            Some(Val::Int(Value::Int(i))) => *i,
            Some(Val::Int(Value::Big(b))) => {
                if b.starts_with('-') {
                    i64::MIN
                } else {
                    i64::MAX
                }
            }
            _ => 0,
        }
    }
    fn value(&self, k: &str) -> Value {
        match self.ns.get(k) {
            Some(Val::Str(s)) => Value::from(s.as_str()),
            Some(Val::Int(v)) => v.clone(),
            Some(Val::Bool(b)) => Value::Bool(*b),
            Some(Val::List(l)) => Value::Arr(l.iter().map(|s| Value::from(s.as_str())).collect()),
            _ => Value::Null,
        }
    }
    fn board(&self) -> Board {
        Board::new(self.s("home").unwrap_or(""), self.s("receipts"))
    }
    fn session(&self) -> Result<String> {
        match self.t("session") {
            Some(s) => Ok(s.to_string()),
            None => cli_error("--session is required (or set COLLAB_SESSION); run wake first"),
        }
    }
}

// ====================================================================== handlers

fn refs(values: &[String]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for value in values {
        let (post, revision) = match value.rsplit_once(':') {
            Some((p, r)) if !p.is_empty() => (p, r),
            _ => return cli_error("references must use POST_ID:REVISION"),
        };
        let number = match py_int(revision) {
            Some(n) => n,
            None => return cli_error("reference revision must be an integer"),
        };
        let positive = match &number {
            Value::Int(i) => *i >= 1,
            Value::Big(b) => !b.starts_with('-'),
            _ => false,
        };
        if !positive {
            return cli_error("reference revision must be positive");
        }
        out.push(obj! {"id" => post, "revision" => number});
    }
    Ok(out)
}

fn cwd() -> Result<PathBuf> {
    std::env::current_dir().map_err(Error::from)
}

fn outcome(board: &Board, actor: &str, op: &str, data: Map) -> Result<Value> {
    let mut data = data;
    if op == "post.create" || op == "task.create" {
        let here = pyfmt::resolve(&cwd()?);
        let (root, checkout) = crate::git::checkout(&here);
        data.insert(
            "context",
            obj! {"cwd" => pyfmt::path_str(&here), "repository" => pyfmt::path_str(&root), "worktree" => pyfmt::path_str(&checkout)},
        );
        if let Some(Value::Arr(scope)) = data.get("scope") {
            let mut set: Vec<String> = scope
                .iter()
                .map(|e| {
                    let e = state::py_str(e);
                    if e.starts_with("resource:") {
                        e
                    } else {
                        pyfmt::path_str(&pyfmt::resolve(&pyfmt::expanduser(&e)))
                    }
                })
                .collect();
            set.sort();
            set.dedup();
            data.insert("scope", set.into_iter().map(Value::from).collect::<Vec<_>>());
        }
    }
    board.submit(board.request(actor, op, Value::Obj(data)))
}

fn known_project<'a>(s: &'a state::State, root: &str, checkout: &str) -> Option<&'a Value> {
    s.values(T::Projects).find(|p| p.at("roots").as_arr().is_some_and(|r| r.iter().any(|x| x.as_str() == Some(root) || x.as_str() == Some(checkout))))
}

fn automatic_project(board: &Board, actor: &str, cwd: &Path) -> Result<(String, Option<Value>)> {
    let (root, checkout) = crate::git::checkout(cwd);
    let root_text = pyfmt::path_str(&root);
    let checkout = pyfmt::path_str(&checkout);
    let s = board.read_state()?;
    let mut project: Value = match known_project(&s, &root_text, &checkout) {
        Some(p) => p.clone(),
        None => {
            let project_id = format!("auto-{}", &pyfmt::sha256_hex(root_text.as_bytes())[..16]);
            if s.contains(T::Projects, &project_id) {
                return cli_error("automatic project ID already belongs to different roots");
            }
            drop(s);
            let name = root.file_name().map(|n| pyfmt::path_str(Path::new(n))).filter(|n| !n.is_empty()).unwrap_or_else(|| root_text.clone());
            let mut roots = vec![root_text.clone(), checkout.clone()];
            roots.sort();
            roots.dedup();
            let created = outcome(
                board,
                actor,
                "project.create",
                obj_map(obj! {"id" => project_id.clone(), "name" => name, "roots" => roots.into_iter().map(Value::from).collect::<Vec<_>>()}),
            )?;
            let s = board.read_state()?;
            match s.get(T::Projects, &project_id) {
                Some(p) if p.at("roots").as_arr().is_some_and(|r| r.iter().any(|x| x.as_str() == Some(&root_text))) => p.clone(),
                _ => {
                    let msg = created.get("error").map(state::py_str).unwrap_or_else(|| "could not create automatic project".into());
                    return cli_error(msg);
                }
            }
        }
    };
    for _ in 0..3 {
        let id = state::py_str(project.at("id"));
        if project.at("roots").as_arr().is_some_and(|r| r.iter().any(|x| x.as_str() == Some(&checkout))) {
            return Ok((id, None));
        }
        let result = outcome(board, actor, "project.attach", obj_map(obj! {"project" => id.clone(), "root" => checkout.clone(), "expected" => project.at("revision").clone()}))?;
        let s = board.read_state()?;
        project = match s.get(T::Projects, &id) {
            Some(p) => p.clone(),
            None => return Err(Error::Crash(format!("KeyError: {}", state::py_repr(&Value::from(id.as_str()))))),
        };
        if result.at("status").as_str() == Some("accepted") {
            return Ok((id, Some(result)));
        }
    }
    cli_error("project attachment changed concurrently; retry wake")
}

fn obj_map(v: Value) -> Map {
    match v {
        Value::Obj(m) => m,
        _ => Map::new(),
    }
}

fn capture(board: &Board, sources: &[String]) -> Result<Vec<Value>> {
    sources.iter().map(|s| board.capture(s)).collect()
}

fn declared(a: &Args) -> Result<Option<Value>> {
    let doing = a.s("doing");
    let summary = a.s("summary");
    let uses = a.list("uses");
    if doing.is_none() && summary.is_none() {
        if !uses.is_empty() {
            return cli_error("--uses needs --doing and --summary");
        }
        return Ok(None);
    }
    let (Some(doing), Some(summary)) = (doing.filter(|d| !d.is_empty()), summary.filter(|s| !s.is_empty())) else {
        return cli_error("--doing and --summary go together");
    };
    let mut d = obj! {"doing" => doing, "summary" => summary};
    if !uses.is_empty() {
        d.as_obj_mut().unwrap().insert("uses", uses.into_iter().map(Value::from).collect::<Vec<_>>());
    }
    state::presence_status(&d, &pyfmt::utcnow())?;
    Ok(Some(d))
}

fn wake(a: &Args) -> Result<Value> {
    let board = a.board();
    let actor = a.t("session").map(str::to_string).unwrap_or_else(pyfmt::uuid4);
    let declared = declared(a)?;
    let harness = a.s("harness").unwrap_or("").to_string();
    let process = presence::detect_host(&harness);
    let mut extra = Map::new();
    if let Some(d) = declared.as_ref().filter(|d| d.truthy()) {
        extra.insert("status", d.clone());
    }
    if let Some(p) = process.filter(|p| p.truthy()) {
        extra.insert("host", p);
    }
    let replayed: Vec<Value> = board.sync(&actor)?;
    let s = match board.read_state() {
        Ok(s) => s,
        Err(e) if e.is_journal() || matches!(e, Error::Os(_)) => {
            let mut data = obj_map(obj! {"harness" => harness.clone()});
            for (k, v) in extra.iter() {
                data.insert(k.clone(), v.clone());
            }
            for field in ["project", "parent", "name"] {
                if let Some(v) = a.s(field) {
                    data.insert(field, v);
                }
            }
            let registered = outcome(&board, &actor, "session.register", data)?;
            let mut out = obj_map(registered);
            out.insert("session", actor.clone());
            out.insert("sync", replayed);
            out.insert("warning", format!("Board unavailable; continue authorized work, keep this session ID, and sync later. {}", e.message()));
            return Ok(Value::Obj(out));
        }
        Err(e) => return Err(e),
    };
    let prior = s.get(T::Sessions, &actor).cloned();
    let mut attach = false;
    let project: Value;
    let parent: Value;
    let name: Value;
    if let Some(prior) = &prior {
        if prior.at("harness").as_str() != Some(harness.as_str()) {
            return cli_error(format!("session already belongs to harness {}", state::py_str(prior.at("harness"))));
        }
        if let Some(p) = a.s("parent") {
            if prior.at("parent").as_str() != Some(p) {
                return cli_error(format!("session already has parent {}", state::py_str(prior.at("parent"))));
            }
        }
        project = a.t("project").map(Value::from).unwrap_or_else(|| prior.at("project").clone());
        parent = prior.at("parent").clone();
        name = a.t("name").map(Value::from).unwrap_or_else(|| prior.at("name").clone());
        drop(s);
    } else {
        if let Some(p) = a.t("project") {
            if !s.contains(T::Projects, p) {
                return cli_error(format!("unknown project: {p}"));
            }
        }
        parent = a.s("parent").map(Value::from).unwrap_or(Value::Null);
        name = Value::from(a.t("name").unwrap_or(&actor));
        let mut chosen: Value = a.s("project").map(Value::from).unwrap_or(Value::Null);
        if !chosen.truthy() {
            let (root, checkout) = crate::git::checkout(&cwd()?);
            let checkout = pyfmt::path_str(&checkout);
            if let Some(known) = known_project(&s, &pyfmt::path_str(&root), &checkout) {
                attach = !known.at("roots").as_arr().is_some_and(|r| r.iter().any(|x| x.as_str() == Some(&checkout)));
                chosen = known.at("id").clone();
            } else {
                chosen = Value::Null;
            }
        }
        drop(s);
        if !chosen.truthy() {
            let bootstrap = outcome(
                &board,
                &actor,
                "session.register",
                obj_map(obj! {"harness" => harness.clone(), "project" => "global", "parent" => parent.clone(), "name" => name.clone()}),
            )?;
            if bootstrap.at("status").as_str() != Some("accepted") {
                return Ok(obj! {"session" => actor, "sync" => replayed, "registration" => bootstrap});
            }
            chosen = Value::from(automatic_project(&board, &actor, &cwd()?)?.0);
        }
        project = chosen;
    }
    let mut data = obj_map(obj! {"harness" => harness.clone(), "project" => project.clone(), "parent" => parent, "name" => name});
    for (k, v) in extra.iter() {
        data.insert(k.clone(), v.clone());
    }
    let registered = outcome(&board, &actor, "session.register", data)?;
    if registered.at("status").as_str() != Some("accepted") {
        return Ok(obj! {"session" => actor, "sync" => replayed, "registration" => registered});
    }
    if attach {
        automatic_project(&board, &actor, &cwd()?)?;
    }
    let view = a.s("view").unwrap_or("compact").to_string();
    let budget = if view == "compact" { Some(a.small("budget")) } else { None };
    let brief = board.brief(
        &actor,
        BriefArgs { project: project.as_str().map(str::to_string), limit: a.small("limit"), offset: 0, view, budget, ..Default::default() },
    )?;
    let mut next = Vec::new();
    let prior_status = prior.as_ref().is_some_and(|p| p.at("status").truthy());
    if declared.is_none() && !prior_status {
        next.push(Value::from(format!("collab --session {actor} {DECLARE}")));
    }
    if brief.at("delivery").truthy() {
        next.push(Value::from(format!("collab --session {actor} ack {}", state::py_str(brief.at("delivery")))));
    }
    if brief.at("next_cursor").truthy() {
        next.push(Value::from(format!("collab --session {actor} inbox --cursor {}", state::py_str(brief.at("next_cursor")))));
    }
    let status = brief.get("status").cloned().unwrap_or_else(|| Value::from("accepted"));
    Ok(obj! {"status" => status, "session" => actor, "project" => project, "sync" => replayed, "registration" => registered, "brief" => brief, "next" => next})
}

fn project_cmd(a: &Args, sub: &str) -> Result<Value> {
    let board = a.board();
    if sub == "list" {
        let s = board.read_state()?;
        return Ok(obj! {"status" => "accepted", "projects" => s.values(T::Projects).cloned().collect::<Vec<_>>()});
    }
    let actor = a.session()?;
    if sub == "create" {
        let roots: Vec<Value> = a.list("root").iter().map(|r| Value::from(pyfmt::path_str(&pyfmt::resolve(&pyfmt::expanduser(r))))).collect();
        return outcome(&board, &actor, "project.create", obj_map(obj! {"id" => a.value("id"), "name" => a.value("name"), "roots" => roots}));
    }
    let root = pyfmt::path_str(&pyfmt::resolve(&pyfmt::expanduser(a.s("root").unwrap_or(""))));
    outcome(&board, &actor, "project.attach", obj_map(obj! {"project" => a.value("project"), "root" => root, "expected" => a.int("expected")}))
}

/// `{key: value for ... if value not in (None, [], "")}`.
fn kept(m: Map) -> Map {
    let mut out = Map::new();
    for (k, v) in m.0 {
        let empty = match &v {
            Value::Null => true,
            Value::Arr(a) => a.is_empty(),
            Value::Str(s) => s.is_empty(),
            _ => false,
        };
        if !empty {
            out.push(k, v);
        }
    }
    out
}

fn post(a: &Args) -> Result<Value> {
    let board = a.board();
    let actor = a.session()?;
    let refs = refs(&a.list("ref"))?;
    let evidence = capture(&board, &a.list("source"))?;
    let data = obj_map(obj! {
        "text" => a.value("text"), "kind" => a.value("kind"), "project" => a.value("project"), "to" => a.value("to"),
        "refs" => refs, "scope" => a.value("scope"), "evidence" => evidence,
    });
    outcome(&board, &actor, "post.create", kept(data))
}

fn inbox(a: &Args, post_id: Option<&str>) -> Result<Value> {
    let board = a.board();
    let actor = a.session()?;
    let view = a.s("view").map(str::to_string).unwrap_or_else(|| if post_id.is_some() { "full".into() } else { "compact".into() });
    let budget = if view == "compact" && a.ns.contains_key("budget") { Some(a.small("budget")) } else { None };
    board.brief(
        &actor,
        BriefArgs {
            project: a.s("project").map(str::to_string),
            limit: if a.ns.contains_key("limit") { a.small("limit") } else { 12 },
            offset: if a.ns.contains_key("offset") { a.small("offset") } else { 0 },
            item_id: post_id.map(str::to_string),
            item_kind: a.s("kind").map(str::to_string),
            cursor: a.s("cursor"),
            view,
            budget,
            post_id: None,
        },
    )
}

fn task(a: &Args, sub: &str) -> Result<Value> {
    let board = a.board();
    let actor = a.session()?;
    if sub == "list" {
        let s = board.read_state()?;
        let project = match a.t("project") {
            Some(p) => Value::from(p),
            None => match s.get(T::Sessions, &actor) {
                Some(session) => session.at("project").clone(),
                None => return Err(Error::Crash(format!("KeyError: {}", state::py_repr(&Value::from(actor.as_str()))))),
            },
        };
        let view = a.s("view").unwrap_or("full").to_string();
        let mut tasks = Vec::new();
        for t in s.values(T::Tasks) {
            if t.at("project").py_eq(&project) {
                tasks.push(if view == "compact" { board::render_item("task", t, "compact", board::COMPACT_PREVIEW_LIMIT)? } else { t.clone() });
            }
        }
        return Ok(obj! {"status" => "accepted", "tasks" => tasks, "view" => view});
    }
    if sub == "create" {
        let mut data = obj_map(obj! {
            "title" => a.value("title"), "owner" => a.t("owner").map(str::to_string).unwrap_or_else(|| actor.clone()),
            "project" => a.value("project"), "scope" => a.value("scope"), "dependencies" => a.value("dependency"), "priority" => a.value("priority"),
        });
        data.0.retain(|(_, v)| !v.is_null());
        return outcome(&board, &actor, "task.create", data);
    }
    let mut data = obj_map(obj! {"task" => a.value("id"), "expected" => a.int("expected")});
    match sub {
        "claim" => {
            if a.b("takeover") && a.list("source").is_empty() {
                return cli_error("takeover requires at least one --source evidence file");
            }
            data.insert("takeover", a.b("takeover"));
            data.insert("evidence", capture(&board, &a.list("source"))?);
            outcome(&board, &actor, "task.claim", data)
        }
        "finish" => {
            if a.list("source").is_empty() {
                return cli_error("finish requires at least one --source evidence file");
            }
            data.insert("evidence", capture(&board, &a.list("source"))?);
            if let Some(r) = a.t("resolution") {
                data.insert("resolution", r);
            }
            outcome(&board, &actor, "task.finish", data)
        }
        _ => {
            data.insert("reason", a.value("reason"));
            outcome(&board, &actor, &format!("task.{sub}"), data)
        }
    }
}

fn status(a: &Args) -> Result<Value> {
    let board = a.board();
    let s = board.read_state()?;
    let project: Option<String> = match a.t("project") {
        Some(p) => Some(p.to_string()),
        None => a.t("session").and_then(|sess| s.get(T::Sessions, sess)).and_then(|x| x.at("project").as_str().map(str::to_string)),
    };
    let project = project.filter(|p| !p.is_empty());
    let here = |v: &Value| project.as_deref().is_none_or(|p| v.at("project").as_str() == Some(p));
    let view = a.s("view").unwrap_or("full").to_string();
    let mut tasks = Vec::new();
    for t in s.values(T::Tasks).filter(|t| here(t)) {
        tasks.push(if view == "compact" { board::render_item("task", t, "compact", board::COMPACT_PREVIEW_LIMIT)? } else { t.clone() });
    }
    let open = |t: &Value| !matches!(t.at("status").as_str(), Some("done") | Some("cancelled"));
    let active = tasks.iter().filter(|t| open(t)).count();
    let requests = tasks.iter().filter(|t| t.at("source_post").truthy() && open(t)).count();
    let disputed = s.values(T::Posts).filter(|p| here(p) && p.at("status").as_str() == Some("disputed")).count();
    let due = Board::due_in(&s, project.as_deref(), 4, false)?;
    Ok(obj! {
        "status" => "accepted", "project" => project, "view" => view, "sessions" => s.len(T::Sessions),
        "active_tasks" => active, "open_requests" => requests, "disputed_posts" => disputed,
        "moderation_due" => due, "tasks" => tasks,
    })
}

fn doing(a: &Args) -> Result<Value> {
    let board = a.board();
    let actor = a.session()?;
    let declared = declared(a)?.unwrap_or(Value::Null);
    let mut result = outcome(&board, &actor, "session.touch", obj_map(obj! {"status" => declared.clone()}))?;
    if result.at("status").as_str() == Some("accepted") && declared.at("uses").truthy() {
        let s = board.read_state()?;
        let project = s.get(T::Sessions, &actor).map(|x| x.at("project").clone()).unwrap_or(Value::Null);
        let view = presence::listing(&s, &Ts::now(), Some(&actor), project.as_str(), false);
        let mut clashes = Vec::new();
        for key in ["peers", "elsewhere"] {
            if let Some(items) = view.get(key).and_then(Value::as_arr) {
                clashes.extend(items.iter().filter(|i| i.at("clash").truthy()).cloned());
            }
        }
        result.as_obj_mut().unwrap().insert("clashes", clashes);
    }
    Ok(result)
}

fn bye(a: &Args) -> Result<Value> {
    let mut data = obj_map(obj! {"leave" => true});
    if let Some(summary) = a.t("summary") {
        let v = Value::from(summary);
        data.insert("summary", state::text(&v, "summary", 280)?);
    }
    let board = a.board();
    outcome(&board, &a.session()?, "session.touch", data)
}

fn who(a: &Args) -> Result<Value> {
    let s = a.board().read_state()?;
    let actor = a.t("session");
    if let Some(actor) = actor {
        if !s.contains(T::Sessions, actor) {
            return cli_error(format!("unknown session: {actor}"));
        }
    }
    let project: Option<String> = match a.t("project") {
        Some(p) => Some(p.to_string()),
        None => actor.and_then(|x| s.get(T::Sessions, x)).map(|x| state::py_str(x.at("project"))),
    };
    if let Some(p) = project.as_deref().filter(|p| !p.is_empty()) {
        if !s.contains(T::Projects, p) {
            return cli_error(format!("unknown project: {p}"));
        }
    }
    let view = presence::listing(&s, &Ts::now(), actor, project.as_deref(), true);
    let mut out = obj_map(obj! {"status" => "accepted", "project" => project});
    for (k, v) in view.0 {
        out.insert(k, v);
    }
    Ok(Value::Obj(out))
}

fn artifact(a: &Args) -> Result<Value> {
    let board = a.board();
    let sha = a.s("sha").unwrap_or("");
    let raw = board.artifact(sha)?;
    let mut out = obj! {"status" => "accepted", "sha256" => sha, "bytes" => raw.len(), "path" => pyfmt::path_str(&board.root.join("artifacts").join(sha))};
    if a.b("display") {
        out.as_obj_mut().unwrap().insert("content", String::from_utf8_lossy(&raw).into_owned());
    }
    Ok(out)
}

fn cleanup(a: &Args, sub: &str) -> Result<Value> {
    let board = a.board();
    match sub {
        "preview" => board.cleanup_preview(a.s("sha").unwrap_or("")),
        "remove" => board.cleanup_remove(&a.session()?, a.s("sha").unwrap_or(""), a.s("confirm").unwrap_or("")),
        _ => board.cleanup_resume(&a.session()?, a.s("removal").unwrap_or("")),
    }
}

fn dispatch(a: &Args, path: &[&str]) -> Result<Value> {
    let sub = path.get(1).copied().unwrap_or("");
    match path.first().copied().unwrap_or("") {
        "wake" => wake(a),
        "doing" => doing(a),
        "bye" => bye(a),
        "who" => who(a),
        "sync" => {
            let board = a.board();
            let results = board.sync(&a.session()?)?;
            Ok(obj! {"status" => "accepted", "results" => results})
        }
        "verify" => a.board().verify(),
        "project" => project_cmd(a, sub),
        "post" => post(a),
        "read" => {
            let item = a.s("item").unwrap_or("").to_string();
            inbox(a, Some(&item))
        }
        "inbox" => inbox(a, None),
        "ack" => {
            let board = a.board();
            outcome(&board, &a.session()?, "delivery.ack", obj_map(obj! {"delivery" => a.value("delivery")}))
        }
        "task" => task(a, sub),
        "report" => {
            let board = a.board();
            let actor = a.session()?;
            let evidence = capture(&a.board(), &a.list("source"))?;
            outcome(&board, &actor, "post.report", obj_map(obj! {"post" => a.value("post"), "expected" => a.int("expected"), "reason" => a.value("reason"), "evidence" => evidence}))
        }
        "status" => status(a),
        "artifact" => artifact(a),
        "moderate" => {
            let board = a.board();
            let actor = a.session()?;
            crate::moderator::run_moderation(&board, &actor, a.s("project"), a.b("force"))
        }
        "cleanup" => cleanup(a, sub),
        _ => cli_error("unknown command"),
    }
}

// ====================================================================== human output

fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let brk = matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}');
        if brk {
            out.push(&s[start..i]);
            let mut next = i + c.len_utf8();
            if c == '\r' {
                if let Some((_, '\n')) = chars.peek() {
                    chars.next();
                    next += 1;
                }
            }
            start = next;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn ps(v: &Value) -> String {
    state::py_str(v)
}

fn joined(v: &Value) -> String {
    v.as_arr().map(|a| a.iter().map(ps).collect::<Vec<_>>().join(", ")).unwrap_or_default()
}

fn is_long_id(id: &str) -> bool {
    id.chars().count() >= 32 && id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f' | b'-'))
}

fn presence_line(item: &Value) -> String {
    let id = ps(item.at("id"));
    let ident = if is_long_id(&id) { pyfmt::prefix_chars(&id, 8).to_string() } else { id };
    let mut parts: Vec<String> = Vec::new();
    for part in [item.at("harness").clone(), Value::from(ident), item.at("presence").clone()] {
        if part.truthy() {
            parts.push(ps(&part));
        }
    }
    if let Some(ago) = item.get("ago") {
        parts.push(format!("{}m", ps(ago)));
    }
    let mut head = parts.join(" ");
    if item.at("project").truthy() {
        head.push_str(&format!(" @{}", ps(item.at("project"))));
    }
    let mut text = match item.get("doing") {
        Some(d) => ps(d),
        None => "no status declared".into(),
    };
    if item.at("summary").truthy() {
        text.push_str(&format!(": {}", ps(item.at("summary"))));
    }
    let mut extras = Vec::new();
    if item.at("uses").truthy() {
        extras.push(format!("uses {}", joined(item.at("uses"))));
    }
    if item.at("clash").truthy() {
        extras.push(format!("shared {}", joined(item.at("clash"))));
    }
    if item.at("scope").truthy() {
        let more = if item.at("scope_more").truthy() { format!(" +{}", ps(item.at("scope_more"))) } else { String::new() };
        extras.push(format!("scope {}{more}", joined(item.at("scope"))));
    }
    if item.at("subagents").truthy() {
        extras.push(format!("{} subagents", ps(item.at("subagents"))));
    }
    let mut line = format!("{head} · {text}");
    for e in extras {
        line.push_str(&format!(" [{e}]"));
    }
    line
}

fn presence_lines(value: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    if value.at("you").truthy() {
        lines.push(format!("you: {}", presence_line(value.at("you"))));
    }
    for (key, label) in [("peers", "online"), ("elsewhere", "elsewhere"), ("left", "left"), ("clashes", "clash")] {
        if let Some(items) = value.at(key).as_arr() {
            for item in items {
                lines.push(format!("{label}: {}", presence_line(item)));
            }
        }
    }
    lines
}

fn arr(v: &Value) -> &[Value] {
    v.as_arr().map(Vec::as_slice).unwrap_or(&[])
}

fn human(value: &Value) -> String {
    let status = value.at("status");
    let mut lines = vec![if status.truthy() { ps(status) } else { "accepted".to_string() }];
    if value.is_obj() {
        for key in [
            "session", "project", "post", "task", "delivery", "next_cursor", "sha256", "path", "bytes", "token", "removal", "sessions", "active_tasks", "open_requests",
            "disputed_posts", "view", "returned", "budget", "truncated", "records", "match", "error", "warning",
        ] {
            if let Some(v) = value.get(key) {
                lines.push(format!("{key}: {}", ps(v)));
            }
        }
        for r in arr(value.at("references")) {
            lines.push(format!("affected history: event {} {} {}", ps(r.at("seq")), ps(r.at("operation")), ps(r.at("request_id"))));
        }
        for r in arr(value.at("pending_receipts")) {
            lines.push(format!("pending receipt blocks cleanup: {} {}", ps(r.at("session")), ps(r.at("request_id"))));
        }
        for c in arr(value.at("next")) {
            lines.push(format!("next: {}", ps(c)));
        }
        let brief = value.at("brief");
        if brief.is_obj() {
            let peers = brief.get("peer_count").cloned().unwrap_or(Value::Int(0));
            lines.push(format!("brief: {} item(s), {} peer(s)", arr(brief.at("items")).len(), ps(&peers)));
            let sub = human(brief);
            lines.extend(py_splitlines(&sub).into_iter().skip(1).map(str::to_string));
        }
        if let Some(items) = value.get("items") {
            for item in arr(items) {
                let text = match item.get("text") {
                    Some(t) => ps(t),
                    None => item.get("title").map(ps).unwrap_or_default(),
                };
                let mut label = item.get("status").map(ps).unwrap_or_else(|| "notice".into());
                if item.at("kind").as_str() == Some("notice") && item.at("notice_kind").truthy() {
                    label = format!("notice/{}", ps(item.at("notice_kind")));
                }
                let priority = if item.at("urgent").truthy() || item.at("priority").as_str() == Some("urgent") { " urgent" } else { "" };
                lines.push(format!("{} {} r{} [{label}{priority}]: {text}", ps(item.at("kind")), ps(item.at("id")), ps(item.at("revision"))));
                if item.at("kind").as_str() == Some("task") {
                    lines.push(format!("owner: {}; scope: {}", ps(item.at("owner")), joined(item.at("scope"))));
                }
                for proof in arr(item.at("evidence")) {
                    lines.push(format!("captured source: {} sha256={}", ps(proof.at("source")), ps(proof.at("sha256"))));
                }
                let moderation = item.at("moderation");
                if moderation.truthy() {
                    lines.push(format!("moderation reason: {}", ps(moderation.at("reason"))));
                    for proof in arr(moderation.at("evidence")) {
                        lines.push(format!("moderation source: {} sha256={}", ps(proof.at("source")), ps(proof.at("sha256"))));
                    }
                }
                if let Some(report) = arr(item.at("reports")).last() {
                    lines.push(format!("latest report: {}", ps(report.at("reason"))));
                    for proof in arr(report.at("evidence")) {
                        lines.push(format!("report source: {} sha256={}", ps(proof.at("source")), ps(proof.at("sha256"))));
                    }
                }
            }
        }
        if let Some(tasks) = value.get("tasks") {
            for t in arr(tasks) {
                lines.push(format!("task {} r{} {} owner={}: {}", ps(t.at("id")), ps(t.at("revision")), ps(t.at("status")), ps(t.at("owner")), ps(t.at("title"))));
                lines.push(format!("scope: {}", joined(t.at("scope"))));
            }
        }
        lines.extend(presence_lines(value));
        if value.at("moderation_due").truthy() {
            lines.push(format!("moderation due: {} target(s); run moderate at this checkpoint", arr(value.at("moderation_due")).len()));
        }
        if value.at("instruction").truthy() {
            lines.push(ps(value.at("instruction")));
        }
        if let Some(c) = value.get("content") {
            lines.push(ps(c));
        }
    }
    lines.join("\n")
}

// ====================================================================== main

pub fn main(argv: Vec<String>) -> i32 {
    let json_mode = argv.iter().any(|a| a == "--json");
    let (value, code) = match parse_args(&build_parser(), &argv) {
        Err(Exit::Help(text)) => {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
            return 0;
        }
        Err(Exit::Error(e)) => (obj! {"status" => "rejected", "error" => e.message()}, 1),
        Err(Exit::Arg(_, message)) => (obj! {"status" => "rejected", "error" => message}, 1),
        Ok(parsed) => {
            let args = Args { ns: parsed.ns };
            match dispatch(&args, &parsed.path) {
                Ok(v) => {
                    let code = if v.at("status").as_str() == Some("rejected") { 1 } else { 0 };
                    (v, code)
                }
                Err(Error::Crash(msg)) => {
                    eprintln!("Traceback (most recent call last):\n{msg}");
                    return 1;
                }
                Err(e) => (obj! {"status" => "rejected", "error" => e.message()}, 1),
            }
        }
    };
    let mut text = if json_mode { json::dumps(&value, json::CLI) } else { human(&value) };
    text.push('\n');
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
    code
}
