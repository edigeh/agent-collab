//! Bounded evidence-only moderation through explicitly selected harness models
//! (port of `moderator.py`).

use crate::board::{atomic_file, Board};
use crate::error::{Error, Result};
use crate::json::{self, Map, Value};
use crate::obj;
use crate::pyfmt;
use crate::state::{self, T};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const MODELS: [(&str, &str); 4] = [
    ("codex", "gpt-5.6-luna"),
    ("claude", "sonnet"),
    ("pi", "openai-codex/gpt-5.6-luna"),
    ("omp", "opencode-go/deepseek-v4.1-flash"),
];

pub const SCHEMA: &str = r#"{"additionalProperties":false,"properties":{"outcomes":{"items":{"additionalProperties":false,"properties":{"evidence":{"items":{"type":"string"},"type":"array"},"post":{"type":"string"},"reason":{"type":"string"},"source_fixes":{"items":{"additionalProperties":false,"properties":{"project":{"type":"string"},"reason":{"type":"string"},"source":{"type":"string"}},"required":["project","source","reason"],"type":"object"},"type":"array"},"status":{"enum":["supported","unsupported","disputed","corrected","superseded"],"type":"string"},"text":{"type":"string"}},"required":["post","status","reason","text","evidence","source_fixes"],"type":"object"},"type":"array"}},"required":["outcomes"],"type":"object"}"#;

pub const PROMPT: &str = include_str!("moderator_prompt.txt");
const OMP_OVERLAY: &[u8] = include_bytes!("omp_overlay.yml");

/// `ModerationUnavailable`; every caller converts it to a pending result.
fn unavailable<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Value(msg.into()))
}

fn models_list() -> String {
    MODELS.iter().map(|(h, _)| *h).collect::<Vec<_>>().join(", ")
}

pub fn model_for(harness: &str) -> Result<String> {
    let Some((_, default)) = MODELS.iter().find(|(h, _)| *h == harness) else {
        return unavailable(format!("unknown moderator harness: {harness}"));
    };
    let chosen = std::env::var(format!("COLLAB_MODEL_{}", harness.to_uppercase())).unwrap_or_else(|_| default.to_string());
    if chosen.is_empty() || chosen.chars().any(|c| pyfmt::py_isspace(c) || (c as u32) < 32) {
        return unavailable(format!("invalid configured model for {harness}"));
    }
    if harness == "pi" && !chosen.contains('/') {
        return unavailable("Pi model must include its provider, for example provider/model");
    }
    Ok(chosen)
}

fn is_relative_resolved(path: &Path, root: &str) -> bool {
    pyfmt::is_relative_to(&pyfmt::resolve(path), &pyfmt::resolve(Path::new(root)))
}

type SourceKey = (String, String, bool);

pub fn build_bundle(board: &Board, posts: &[Value]) -> Result<(Value, Map)> {
    let s = board.read_state()?;
    let mut sources: Vec<(SourceKey, Value)> = Vec::new();
    let mut index: HashMap<SourceKey, usize> = HashMap::new();
    let mut evidence_map = Map::new();
    let mut add = |reference: &Value, current: bool, projects: &[String], sources: &mut Vec<(SourceKey, Value)>| -> String {
        let key = (state::py_str(reference.at("source")), state::py_str(reference.at("sha256")), current);
        if let Some(&i) = index.get(&key) {
            return state::py_str(sources[i].1.at("id"));
        }
        let eid = format!("e{}", sources.len());
        let sha = state::py_str(reference.at("sha256"));
        let entry = match board.artifact(&sha) {
            Ok(raw) => {
                let content = String::from_utf8_lossy(&raw[..raw.len().min(4000)]).into_owned();
                let mut stored = reference.as_obj().cloned().unwrap_or_default();
                stored.insert("current", current);
                evidence_map.insert(eid.clone(), stored);
                obj! {
                    "id" => eid.clone(), "source" => reference.at("source").clone(), "sha256" => reference.at("sha256").clone(),
                    "captured_at" => reference.at("captured_at").clone(), "current" => current,
                    "projects" => projects.iter().map(Value::from).collect::<Vec<_>>(),
                    "content" => content, "truncated" => raw.len() > 4000, "available" => true,
                }
            }
            Err(e) => obj! {"id" => eid.clone(), "source" => reference.at("source").clone(), "available" => false, "error" => e.message()},
        };
        index.insert(key, sources.len());
        sources.push((Default::default(), entry));
        eid
    };
    let mut targets = Vec::new();
    for reference in posts {
        let key = state::entity(&s, T::Posts, reference.at("id"))?;
        let p = s.get(T::Posts, key).expect("entity checked");
        let reports: Vec<Value> = p.at("reports").as_arr().cloned().unwrap_or_default();
        let mut target = crate::cli_map(obj! {
            "post" => p.at("id").clone(), "revision" => p.at("revision").clone(), "project" => p.at("project").clone(),
            "claim" => p.at("text").clone(), "status" => p.at("status").clone(), "evidence" => Vec::<Value>::new(),
            "reports" => reports[reports.len().saturating_sub(4)..].to_vec(),
        });
        let mut all: Vec<Value> = p.at("evidence").as_arr().cloned().unwrap_or_default();
        for report in &reports {
            all.extend(report.at("evidence").as_arr().cloned().unwrap_or_default());
        }
        let project_roots: Vec<String> = s
            .get(T::Projects, &state::py_str(p.at("project")))
            .and_then(|pr| pr.at("roots").as_arr())
            .map(|a| a.iter().map(state::py_str).collect())
            .unwrap_or_default();
        let mut ids = Vec::new();
        let mut refresh_errors = Vec::new();
        for saved in &all[all.len().saturating_sub(4)..] {
            let path = PathBuf::from(state::py_str(saved.at("source")));
            let authorized = project_roots.iter().any(|r| is_relative_resolved(&path, r));
            let projects: Vec<String> = s
                .values(T::Projects)
                .filter(|pr| pr.at("roots").as_arr().is_some_and(|a| a.iter().any(|r| is_relative_resolved(&path, &state::py_str(r)))))
                .map(|pr| state::py_str(pr.at("id")))
                .collect();
            ids.push(Value::from(add(saved, false, &projects, &mut sources)));
            if authorized {
                match board.capture(&pyfmt::path_str(&path)) {
                    Ok(current) => ids.push(Value::from(add(&current, true, &projects, &mut sources))),
                    Err(e @ (Error::Os(_) | Error::Value(_) | Error::Rejected(_) | Error::Cli(_))) => {
                        refresh_errors.push(obj! {"source" => pyfmt::path_str(&path), "error" => e.message()})
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        target.insert("evidence", ids);
        if !refresh_errors.is_empty() {
            target.insert("refresh_errors", refresh_errors);
        }
        targets.push(Value::Obj(target));
    }
    let mut wanted: std::collections::HashSet<String> = targets.iter().map(|t| state::py_str(t.at("project"))).collect();
    for (_, source) in &sources {
        if let Some(a) = source.at("projects").as_arr() {
            wanted.extend(a.iter().map(state::py_str));
        }
    }
    let projects: Vec<Value> = s
        .values(T::Projects)
        .filter(|p| wanted.contains(&state::py_str(p.at("id"))))
        .map(|p| obj! {"id" => p.at("id").clone(), "name" => p.at("name").clone()})
        .collect();
    let bundle = obj! {"targets" => targets, "sources" => sources.into_iter().map(|(_, v)| v).collect::<Vec<_>>(), "projects" => projects};
    if json::canonical(&bundle).len() > 64000 {
        return unavailable("evidence batch exceeds bounded prompt size; retry a smaller batch");
    }
    Ok((bundle, evidence_map))
}

pub fn parse_result(raw: Value, refs: &[Value], evidence_map: &Map, bundle: &Value) -> Result<Vec<Value>> {
    let raw = match raw {
        Value::Str(text) => match json::parse(text.as_bytes(), json::LENIENT) {
            Ok(v) => v,
            Err(_) => return unavailable("moderator did not return strict JSON"),
        },
        other => other,
    };
    let outcomes = match &raw {
        Value::Obj(m) if m.len() == 1 && m.get("outcomes").is_some_and(Value::is_arr) => m.get("outcomes").unwrap().as_arr().unwrap(),
        _ => return unavailable("invalid moderation object"),
    };
    let wanted: std::collections::HashSet<String> = refs.iter().map(|r| state::py_str(r.at("id"))).collect();
    if outcomes.len() != wanted.len() {
        return unavailable("moderator did not cover the complete batch");
    }
    let allowed_projects: std::collections::HashSet<String> =
        bundle.at("projects").as_arr().map(|a| a.iter().map(|p| state::py_str(p.at("id"))).collect()).unwrap_or_default();
    let fields = ["post", "status", "reason", "text", "evidence", "source_fixes"];
    let mut seen = std::collections::HashSet::new();
    let mut converted = Vec::new();
    for out in outcomes {
        let Some(m) = out.as_obj().filter(|m| m.len() == fields.len() && fields.iter().all(|f| m.contains_key(f))) else {
            return unavailable("invalid moderation outcome fields");
        };
        let post = match out.at("post").as_str() {
            Some(p) if wanted.contains(p) && !seen.contains(p) => p.to_string(),
            _ => return unavailable("unknown/duplicate moderation target"),
        };
        seen.insert(post);
        let status = out.at("status").as_str().unwrap_or("");
        if !["supported", "unsupported", "disputed", "corrected", "superseded"].contains(&status) || !out.at("status").is_str() {
            return unavailable("invalid moderation classification");
        }
        match out.at("reason").as_str() {
            Some(r) if !pyfmt::py_strip(r).is_empty() && pyfmt::char_len(r) <= 8000 => {}
            _ => return unavailable("invalid moderation reason"),
        }
        match out.at("text").as_str() {
            Some(t) if !(["corrected", "superseded"].contains(&status) && pyfmt::py_strip(t).is_empty()) => {}
            _ => return unavailable("missing justified replacement"),
        }
        let Some(evidence) = out.at("evidence").as_arr().filter(|a| a.iter().all(|e| e.as_str().is_some_and(|e| evidence_map.contains_key(e)))) else {
            return unavailable("moderator invented or referenced unavailable evidence");
        };
        if ["supported", "corrected", "superseded"].contains(&status) && evidence.is_empty() {
            return unavailable("classification requires supporting evidence");
        }
        let Some(fixes) = out.at("source_fixes").as_arr() else {
            return unavailable("invalid source fixes");
        };
        let cited: std::collections::HashSet<String> = evidence.iter().map(|e| state::py_str(evidence_map.get(e.as_str().unwrap()).unwrap().at("source"))).collect();
        for fix in fixes {
            let ok = fix.as_obj().is_some_and(|f| f.len() == 3 && ["project", "source", "reason"].iter().all(|k| f.contains_key(k)))
                && fix.at("source").as_str().is_some_and(|s| cited.contains(s))
                && fix.at("project").as_str().is_some_and(|p| allowed_projects.contains(p))
                && fix.at("reason").as_str().is_some_and(|r| !pyfmt::py_strip(r).is_empty());
            if !ok {
                return unavailable("source fix references unknown context");
            }
            let inside = bundle.at("sources").as_arr().is_some_and(|a| {
                a.iter().any(|s| {
                    s.at("source").py_eq(fix.at("source")) && s.at("projects").as_arr().is_some_and(|ps| ps.iter().any(|p| p.py_eq(fix.at("project"))))
                })
            });
            if !inside {
                return unavailable("source fix is outside the attached project roots");
            }
        }
        let mut unique: Vec<&str> = Vec::new();
        for e in evidence {
            let e = e.as_str().unwrap();
            if !unique.contains(&e) {
                unique.push(e);
            }
        }
        let mut result = m.clone();
        result.insert("evidence", unique.into_iter().map(|e| evidence_map.get(e).unwrap().clone()).collect::<Vec<_>>());
        converted.push(Value::Obj(result));
    }
    Ok(converted)
}

fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

fn atomic(path: &Path, data: &[u8]) -> Result<()> {
    atomic_file(path, data).map_err(Error::from)
}

pub fn invoke(harness: &str, prompt: String, run_dir: &Path, timeout: f64) -> Result<Value> {
    if !MODELS.iter().any(|(h, _)| *h == harness) {
        return unavailable(format!("moderation needs a {} binary; choose an explicit installed harness", models_list()));
    }
    let chosen = model_for(harness)?;
    let Some(binary) = which(harness) else {
        return unavailable(format!("{harness} executable not available"));
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(run_dir)
        .map_err(|e| Error::from(pyfmt::with_path(e, run_dir)))?;
    let schema_path = run_dir.join("schema.json");
    atomic(&schema_path, SCHEMA.as_bytes())?;
    let final_path = run_dir.join("result.json");
    let prompt = if harness == "pi" || harness == "omp" { format!("REQUIRED_RESPONSE_JSON_SCHEMA:\n{SCHEMA}\n\n{prompt}") } else { prompt };
    let b = pyfmt::path_str(&binary);
    let s = |x: &str| x.to_string();
    let argv: Vec<String> = match harness {
        "codex" => vec![
            b, s("-a"), s("never"), s("exec"), s("--model"), chosen.clone(), s("--sandbox"), s("read-only"),
            s("--skip-git-repo-check"), s("--ephemeral"), s("--ignore-user-config"),
            s("--output-schema"), pyfmt::path_str(&schema_path), s("--output-last-message"), pyfmt::path_str(&final_path), s("--json"), s("-"),
        ],
        "claude" => vec![
            b, s("--print"), s("--model"), chosen.clone(), s("--output-format"), s("json"),
            s("--json-schema"), s(SCHEMA), s("--tools"), s(""),
            s("--strict-mcp-config"), s("--mcp-config"), s(r#"{"mcpServers":{}}"#),
            s("--permission-mode"), s("dontAsk"), s("--permission-prompts"), s("none"), s("--no-session-persistence"),
        ],
        "omp" => {
            let overlay = run_dir.join("overlay.yml");
            atomic(&overlay, OMP_OVERLAY)?;
            vec![
                b, s("--print"), s("--mode"), s("text"), s("--model"), chosen.clone(), s("--thinking"), s("low"),
                s("--no-tools"), s("--no-extensions"), s("--no-skills"), s("--no-rules"), s("--no-session"),
                s("--config"), pyfmt::path_str(&overlay),
            ]
        }
        _ => vec![
            b, s("--print"), s("--provider"), chosen.split('/').next().unwrap_or("").to_string(), s("--model"), chosen.clone(),
            s("--thinking"), s("low"), s("--mode"), s("text"), s("--no-tools"), s("--no-extensions"), s("--no-skills"),
            s("--no-prompt-templates"), s("--no-context-files"), s("--offline"), s("--no-session"),
        ],
    };
    atomic(&run_dir.join("prompt.txt"), prompt.as_bytes())?;
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).current_dir(run_dir).env("COLLAB_MODERATING", "1");
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|e| Error::Value(format!("cannot invoke {harness}: {}", pyfmt::os_error_text(&e))))?;
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(prompt.as_bytes());
    });
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out_pipe.read_to_end(&mut b);
        b
    });
    let err_reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err_pipe.read_to_end(&mut b);
        b
    });
    let pid = child.id() as i32;
    let started = Instant::now();
    let wait_until = |child: &mut std::process::Child, limit: Duration| -> Option<std::process::ExitStatus> {
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(status);
            }
            if started.elapsed() >= limit {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let mut status = wait_until(&mut child, Duration::from_secs_f64(timeout));
    let timed_out = status.is_none();
    if timed_out {
        unsafe { libc::killpg(pid, libc::SIGTERM) };
        status = wait_until(&mut child, Duration::from_secs_f64(timeout + 5.0));
        if status.is_none() {
            unsafe { libc::killpg(pid, libc::SIGKILL) };
            status = child.wait().ok();
        }
    }
    let _ = writer.join();
    let stdout = String::from_utf8_lossy(&out_reader.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
    atomic(&run_dir.join("stdout.txt"), stdout.as_bytes())?;
    atomic(&run_dir.join("stderr.txt"), stderr.as_bytes())?;
    if timed_out {
        return unavailable(format!("{harness} moderator timed out; work remains pending"));
    }
    let code: i64 = match status {
        Some(st) => {
            use std::os::unix::process::ExitStatusExt;
            st.code().map(i64::from).unwrap_or_else(|| -(st.signal().unwrap_or(0) as i64))
        }
        None => -1,
    };
    let execution = obj! {"harness" => harness, "model" => chosen.clone(), "exit_code" => code, "finished_at" => pyfmt::utcnow()};
    atomic(&run_dir.join("execution.json"), &json::canonical(&execution))?;
    if code != 0 {
        return unavailable(format!("{harness} exited {code}; see {}/stderr.txt", pyfmt::path_str(run_dir)));
    }
    match harness {
        "codex" => {
            for line in stdout.lines() {
                let Ok(event) = json::parse(line.as_bytes(), json::LENIENT) else { continue };
                if let Some(kind) = event.at("item").at("type").as_str() {
                    if ["command_execution", "mcp_tool_call", "web_search"].contains(&kind) {
                        return unavailable("evidence-only moderator attempted a tool call; result not applied");
                    }
                }
            }
            if !final_path.exists() {
                return unavailable("Codex produced no final structured result");
            }
            let raw = std::fs::read(&final_path).map_err(|e| Error::from(pyfmt::with_path(e, &final_path)))?;
            Ok(Value::Str(String::from_utf8_lossy(&raw).into_owned()))
        }
        "claude" => {
            let Ok(response) = json::parse(stdout.as_bytes(), json::LENIENT) else {
                return unavailable("Claude returned no JSON result envelope");
            };
            if response.at("is_error").truthy() {
                return unavailable("Claude reported an error; result not applied");
            }
            let result = match response.get("structured_output") {
                Some(v) => v.clone(),
                None => response.at("result").clone(),
            };
            if result.is_null() {
                return unavailable("Claude produced no structured result");
            }
            Ok(result)
        }
        _ => Ok(Value::Str(pyfmt::py_strip(&stdout).to_string())),
    }
}

use std::os::unix::fs::DirBuilderExt;

fn caught(e: &Error) -> bool {
    matches!(e, Error::Value(_) | Error::Os(_) | Error::Rejected(_) | Error::Cli(_))
}

pub fn run_moderation(board: &Board, session: &str, project: Option<&str>, force: bool) -> Result<Value> {
    let s = board.read_state()?;
    let key = state::entity(&s, T::Sessions, &Value::from(session))?.to_string();
    let parent = s.get(T::Sessions, &key).unwrap().clone();
    drop(s);
    let harness = state::py_str(parent.at("harness"));
    if !MODELS.iter().any(|(h, _)| *h == harness) {
        return Ok(obj! {"status" => "pending", "error" => format!("Moderation needs a {} session.", models_list())});
    }
    let chosen = match model_for(&harness) {
        Ok(m) => m,
        Err(e) => return Ok(obj! {"status" => "pending", "error" => e.message()}),
    };
    let project = match project {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => state::py_str(parent.at("project")),
    };
    board.check_sources(session, &project, 8)?;
    let refs = board.due(Some(&project), 4, force)?;
    if refs.is_empty() {
        return Ok(obj! {"status" => "idle", "message" => "No moderation batch due."});
    }
    let child = format!("m-{}", pyfmt::uuid4());
    let registered = board.submit(board.request(
        &child,
        "session.register",
        obj! {"harness" => harness.clone(), "parent" => session, "project" => project.clone(), "name" => "moderator"},
    ))?;
    if registered.at("status").as_str() != Some("accepted") {
        return Ok(registered);
    }
    let claimed = board.submit(board.request(&child, "moderation.claim", obj! {"posts" => refs.clone(), "seconds" => 600i64}))?;
    if claimed.at("status").as_str() != Some("accepted") {
        return Ok(claimed);
    }
    let lease = state::py_str(claimed.at("lease"));
    let run_dir = board.root.join("runs").join(&lease);
    let attempt = (|| -> Result<Value> {
        let (bundle, evidence_map) = build_bundle(board, &refs)?;
        let prompt = format!("{PROMPT}{}", String::from_utf8(json::canonical(&bundle)).unwrap_or_default());
        let raw = invoke(&harness, prompt, &run_dir, 180.0)?;
        let outcomes = parse_result(raw, &refs, &evidence_map, &bundle)?;
        let result = board.submit(board.request(&child, "moderation.apply", obj! {"lease" => lease.clone(), "outcomes" => outcomes}))?;
        if result.at("status").as_str() != Some("accepted") {
            let reason = match result.get("error") {
                Some(e) => e.clone(),
                None => Value::from("Result not accepted"),
            };
            board.submit(board.request(&child, "moderation.release", obj! {"lease" => lease.clone(), "reason" => reason}))?;
        }
        let mut out = result.as_obj().cloned().unwrap_or_default();
        out.insert("model", chosen.clone());
        out.insert("run_dir", pyfmt::path_str(&run_dir));
        Ok(Value::Obj(out))
    })();
    match attempt {
        Ok(v) => Ok(v),
        Err(e) if caught(&e) => {
            let released = board.submit(board.request(&child, "moderation.release", obj! {"lease" => lease.clone(), "reason" => e.message()}))?;
            Ok(obj! {
                "status" => "pending", "error" => e.message(), "model" => chosen,
                "lease_release" => released.at("status").clone(), "run_dir" => pyfmt::path_str(&run_dir),
            })
        }
        Err(e) => Err(e),
    }
}
