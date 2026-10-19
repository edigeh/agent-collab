//! A small emulation of Python's argparse: the grammar, abbreviation rules
//! and error texts the Python command lines produce, for the parsers the three
//! binaries define.

use crate::error::Error;
use crate::json::Value;
use crate::pyfmt;
use crate::state;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Kind {
    Help,
    Store,
    StoreTrue,
    Append,
    Positional,
    Parser,
}

pub(crate) struct Action {
    pub(crate) flags: Vec<&'static str>,
    pub(crate) dest: &'static str,
    pub(crate) kind: Kind,
    pub(crate) required: bool,
    pub(crate) choices: Option<Vec<&'static str>>,
    pub(crate) int: bool,
    pub(crate) default: Val,
    pub(crate) group: u8,
}

impl Action {
    pub(crate) fn name(&self) -> String {
        if self.flags.is_empty() {
            self.dest.to_string()
        } else {
            self.flags.join("/")
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Val {
    None,
    Str(String),
    Bool(bool),
    Int(Value),
    List(Vec<String>),
}

pub(crate) struct Spec {
    pub(crate) path: String,
    pub(crate) actions: Vec<Action>,
    pub(crate) subs: Vec<(&'static str, Spec)>,
}

impl Spec {
    pub(crate) fn new(path: &str) -> Spec {
        let help = Action { flags: vec!["-h", "--help"], dest: "help", kind: Kind::Help, required: false, choices: None, int: false, default: Val::None, group: 0 };
        Spec { path: path.to_string(), actions: vec![help], subs: Vec::new() }
    }
    pub(crate) fn add(&mut self, flag: &'static str, kind: Kind) -> &mut Action {
        let (flags, dest) = if flag.starts_with('-') {
            (vec![flag], &*Box::leak(flag.trim_start_matches('-').replace('-', "_").into_boxed_str()))
        } else {
            (vec![], flag)
        };
        let default = match kind {
            Kind::StoreTrue => Val::Bool(false),
            Kind::Append => Val::List(Vec::new()),
            _ => Val::None,
        };
        self.actions.push(Action { flags, dest, kind, required: false, choices: None, int: false, default, group: 0 });
        self.actions.last_mut().unwrap()
    }
    pub(crate) fn store(&mut self, flag: &'static str) -> &mut Action {
        self.add(flag, Kind::Store)
    }
    pub(crate) fn pos(&mut self, dest: &'static str) -> &mut Action {
        self.add(dest, Kind::Positional)
    }
    pub(crate) fn flag(&mut self, flag: &'static str) -> &mut Action {
        self.add(flag, Kind::StoreTrue)
    }
    pub(crate) fn append(&mut self, flag: &'static str) -> &mut Action {
        self.add(flag, Kind::Append)
    }
    pub(crate) fn subparsers(&mut self, dest: &'static str, names: Vec<(&'static str, Spec)>) {
        let choices: Vec<&'static str> = names.iter().map(|(n, _)| *n).collect();
        self.actions.push(Action { flags: vec![], dest, kind: Kind::Parser, required: true, choices: Some(choices), int: false, default: Val::None, group: 0 });
        self.subs = names;
    }
    pub(crate) fn find(&self, flag: &str) -> Option<usize> {
        self.actions.iter().position(|a| a.flags.contains(&flag))
    }
    pub(crate) fn option_strings(&self) -> impl Iterator<Item = (usize, &'static str)> + '_ {
        self.actions.iter().enumerate().flat_map(|(i, a)| a.flags.iter().map(move |f| (i, *f)))
    }
}

impl Action {
    pub(crate) fn req(&mut self) -> &mut Self {
        self.required = true;
        self
    }
    pub(crate) fn choices(&mut self, c: &[&'static str]) -> &mut Self {
        self.choices = Some(c.to_vec());
        self
    }
    pub(crate) fn int(&mut self, default: Option<i64>) -> &mut Self {
        self.int = true;
        if let Some(d) = default {
            self.default = Val::Int(Value::Int(d));
        }
        self
    }
    pub(crate) fn default(&mut self, d: &str) -> &mut Self {
        self.default = Val::Str(d.to_string());
        self
    }
    pub(crate) fn default_opt(&mut self, d: Option<String>) -> &mut Self {
        self.default = d.map(Val::Str).unwrap_or(Val::None);
        self
    }
    pub(crate) fn group(&mut self, g: u8) -> &mut Self {
        self.group = g;
        self
    }
}

pub(crate) type Tuple = (Option<usize>, String, Option<String>, bool);

pub(crate) fn negative_number(s: &str) -> bool {
    let b = s.as_bytes();
    if b.first() != Some(&b'-') {
        return false;
    }
    let rest = &s[1..];
    if !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit()) {
        return true;
    }
    if let Some((int, frac)) = rest.split_once('.') {
        return int.bytes().all(|c| c.is_ascii_digit()) && !frac.is_empty() && frac.bytes().all(|c| c.is_ascii_digit());
    }
    false
}

pub(crate) fn parse_optional(spec: &Spec, arg: &str) -> Option<Vec<Tuple>> {
    if arg.is_empty() || !arg.starts_with('-') {
        return None;
    }
    if let Some(i) = spec.find(arg) {
        return Some(vec![(Some(i), arg.to_string(), None, false)]);
    }
    if arg.chars().count() == 1 {
        return None;
    }
    if let Some((option, explicit)) = arg.split_once('=') {
        if let Some(i) = spec.find(option) {
            return Some(vec![(Some(i), option.to_string(), Some(explicit.to_string()), true)]);
        }
    }
    let mut result = Vec::new();
    let second = arg.chars().nth(1);
    if second == Some('-') {
        let (prefix, explicit) = match arg.split_once('=') {
            Some((p, e)) => (p, Some(e.to_string())),
            None => (arg, None),
        };
        for (i, option) in spec.option_strings() {
            if option.starts_with(prefix) {
                result.push((Some(i), option.to_string(), explicit.clone(), explicit.is_some()));
            }
        }
    } else {
        let (prefix, explicit) = match arg.split_once('=') {
            Some((p, e)) => (p, Some(e.to_string())),
            None => (arg, None),
        };
        let short: String = arg.chars().take(2).collect();
        let short_explicit: String = arg.chars().skip(2).collect();
        for (i, option) in spec.option_strings() {
            if option == short {
                result.push((Some(i), option.to_string(), Some(short_explicit.clone()), false));
            } else if option.starts_with(prefix) {
                result.push((Some(i), option.to_string(), explicit.clone(), explicit.is_some()));
            }
        }
    }
    if !result.is_empty() {
        return Some(result);
    }
    if negative_number(arg) || arg.contains(' ') {
        return None;
    }
    Some(vec![(None, arg.to_string(), None, false)])
}

/// Python's `int(str)` for the decimal strings argparse hands it.
pub(crate) fn py_int(text: &str) -> Option<Value> {
    let t = pyfmt::py_strip(text);
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return None;
    }
    if !digits.bytes().all(|c| c.is_ascii_digit() || c == b'_') {
        return None;
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    let trimmed = clean.trim_start_matches('0');
    let body = if trimmed.is_empty() { "0" } else { trimmed };
    let text = if neg && body != "0" { format!("-{body}") } else { body.to_string() };
    Some(match text.parse::<i64>() {
        Ok(i) => Value::Int(i),
        Err(_) => Value::Big(text.into()),
    })
}

pub(crate) struct Parsed {
    pub(crate) ns: HashMap<&'static str, Val>,
    pub(crate) path: Vec<&'static str>,
}

pub(crate) enum Exit {
    /// `--help`: print this text and exit 0.
    Help(String),
    Error(Error),
    /// An argparse error raised by the parser at this path (`parser.error(message)`).
    Arg(String, String),
}

impl From<Error> for Exit {
    fn from(e: Error) -> Exit {
        Exit::Error(e)
    }
}

pub(crate) fn arg_error<R>(action: &Action, msg: impl std::fmt::Display) -> std::result::Result<R, Exit> {
    Err(Exit::Error(Error::Cli(format!("argument {}: {msg}", action.name()))))
}

pub(crate) fn plain_error<R>(msg: impl Into<String>) -> std::result::Result<R, Exit> {
    Err(Exit::Error(Error::Cli(msg.into())))
}

/// Greedy match of `positionals` (nargs None => `-*A-*`, PARSER => `-*A[-AO]*`)
/// against `pattern`, the way `_match_arguments_partial` does.
pub(crate) fn match_positionals(kinds: &[Kind], pattern: &[u8]) -> Vec<usize> {
    fn go(kinds: &[Kind], pattern: &[u8], at: usize, out: &mut Vec<usize>) -> bool {
        let Some((first, rest)) = kinds.split_first() else { return true };
        let mut i = at;
        while i < pattern.len() && pattern[i] == b'-' {
            i += 1;
        }
        if i >= pattern.len() || pattern[i] != b'A' {
            return false;
        }
        i += 1;
        let max_end = if *first == Kind::Parser {
            pattern.len()
        } else {
            let mut j = i;
            while j < pattern.len() && pattern[j] == b'-' {
                j += 1;
            }
            j
        };
        let mut end = max_end;
        loop {
            out.push(end - at);
            if go(rest, pattern, end, out) {
                return true;
            }
            out.pop();
            if end == i {
                return false;
            }
            end -= 1;
            if *first != Kind::Parser && end < i {
                return false;
            }
        }
    }
    for n in (1..=kinds.len()).rev() {
        let mut out = Vec::new();
        if go(&kinds[..n], pattern, 0, &mut out) {
            return out;
        }
    }
    Vec::new()
}

pub(crate) struct Run<'s> {
    pub(crate) spec: &'s Spec,
    pub(crate) args: &'s [String],
    pub(crate) pattern: Vec<u8>,
    pub(crate) seen: HashSet<usize>,
    pub(crate) seen_non_default: HashSet<usize>,
    pub(crate) own_extras: Vec<String>,
    pub(crate) sub_extras: Vec<String>,
    pub(crate) positionals: Vec<usize>,
}

pub(crate) type Step<T> = std::result::Result<T, Exit>;

impl<'s> Run<'s> {
    pub(crate) fn take(&mut self, idx: usize, values: Vec<String>, ns: &mut HashMap<&'static str, Val>, path: &mut Vec<&'static str>) -> Step<()> {
        let spec = self.spec;
        let action = &spec.actions[idx];
        self.seen.insert(idx);
        let invalid_choice = |value: &str, choices: &[&'static str]| -> Step<()> {
            let listed: Vec<String> = choices.iter().map(|c| state::py_repr(&Value::from(*c))).collect();
            arg_error(action, format!("invalid choice: {} (choose from {})", state::py_repr(&Value::from(value)), listed.join(", ")))
        };
        let converted = match action.kind {
            Kind::Store | Kind::Append | Kind::Positional => {
                let raw = &values[0];
                let v = if action.int {
                    match py_int(raw) {
                        Some(v) => Val::Int(v),
                        None => return arg_error(action, format!("invalid int value: {}", state::py_repr(&Value::from(raw.as_str())))),
                    }
                } else {
                    Val::Str(raw.clone())
                };
                if let (Some(choices), Val::Str(s)) = (&action.choices, &v) {
                    if !choices.contains(&s.as_str()) {
                        invalid_choice(s, choices)?;
                    }
                }
                v
            }
            Kind::Parser => {
                let choices = action.choices.as_ref().unwrap();
                if !choices.contains(&values[0].as_str()) {
                    invalid_choice(&values[0], choices)?;
                }
                Val::None
            }
            _ => Val::None,
        };
        if !action.flags.is_empty() || !values.is_empty() {
            self.seen_non_default.insert(idx);
            if action.group != 0 {
                for (j, other) in spec.actions.iter().enumerate() {
                    if j != idx && other.group == action.group && self.seen_non_default.contains(&j) {
                        return arg_error(action, format!("not allowed with argument {}", other.name()));
                    }
                }
            }
        }
        match action.kind {
            Kind::Help => return Err(Exit::Help(crate::help::text(&spec.path).to_string())),
            Kind::Store | Kind::Positional => {
                ns.insert(action.dest, converted);
            }
            Kind::StoreTrue => {
                ns.insert(action.dest, Val::Bool(true));
            }
            Kind::Append => {
                if let Val::Str(s) = converted {
                    match ns.get_mut(action.dest) {
                        Some(Val::List(list)) => list.push(s),
                        _ => {
                            ns.insert(action.dest, Val::List(vec![s]));
                        }
                    }
                }
            }
            Kind::Parser => {
                let name = values[0].as_str();
                ns.insert(action.dest, Val::Str(name.to_string()));
                let (n, sub) = spec.subs.iter().find(|(n, _)| *n == name).unwrap();
                path.push(n);
                let mut subns = HashMap::new();
                let rest = parse(sub, &values[1..], &mut subns, path)?;
                ns.extend(subns);
                self.sub_extras.extend(rest);
            }
        }
        Ok(())
    }

    pub(crate) fn consume_positionals(&mut self, start: usize, ns: &mut HashMap<&'static str, Val>, path: &mut Vec<&'static str>) -> Step<usize> {
        let kinds: Vec<Kind> = self.positionals.iter().map(|i| self.spec.actions[*i].kind).collect();
        let counts = match_positionals(&kinds, &self.pattern[start..]);
        let mut at = start;
        for (k, count) in counts.iter().enumerate() {
            let idx = self.positionals[k];
            let mut values: Vec<String> = self.args[at..at + count].to_vec();
            if self.spec.actions[idx].kind == Kind::Parser {
                if self.pattern[at] == b'-' {
                    values.remove(0);
                }
            } else if self.pattern[at..at + count].contains(&b'-') {
                if let Some(p) = values.iter().position(|v| v == "--") {
                    values.remove(p);
                }
            }
            at += count;
            self.take(idx, values, ns, path)?;
        }
        self.positionals.drain(..counts.len());
        Ok(at)
    }

    pub(crate) fn consume_optional(&mut self, start: usize, tuples: &[Tuple], ns: &mut HashMap<&'static str, Val>, path: &mut Vec<&'static str>) -> Step<usize> {
        let spec = self.spec;
        if tuples.len() > 1 {
            let names: Vec<&str> = tuples.iter().map(|t| t.1.as_str()).collect();
            return plain_error(format!("ambiguous option: {} could match {}", self.args[start], names.join(", ")));
        }
        let (action, mut option, mut explicit, mut sep) = tuples[0].clone();
        let Some(mut idx) = action else {
            self.own_extras.push(self.args[start].clone());
            return Ok(start + 1);
        };
        let mut queued: Vec<(usize, Vec<String>)> = Vec::new();
        let stop;
        loop {
            let a = &spec.actions[idx];
            let takes_one = matches!(a.kind, Kind::Store | Kind::Append);
            if let Some(ex) = explicit.clone() {
                if takes_one {
                    queued.push((idx, vec![ex]));
                    stop = start + 1;
                    break;
                }
                let single = option.chars().nth(1) != Some('-');
                if !(single && !ex.is_empty()) || sep || ex.starts_with('-') {
                    return arg_error(a, format!("ignored explicit argument {}", state::py_repr(&Value::from(ex.as_str()))));
                }
                queued.push((idx, Vec::new()));
                let first = ex.chars().next().unwrap();
                option = format!("-{first}");
                match spec.find(&option) {
                    Some(j) => {
                        idx = j;
                        let tail: String = ex.chars().skip(1).collect();
                        if tail.is_empty() {
                            explicit = None;
                            sep = false;
                        } else if let Some(t) = tail.strip_prefix('=') {
                            explicit = Some(t.to_string());
                            sep = true;
                        } else {
                            explicit = Some(tail);
                            sep = false;
                        }
                    }
                    None => {
                        self.own_extras.push(format!("-{ex}"));
                        stop = start + 1;
                        break;
                    }
                }
                continue;
            }
            if takes_one {
                if self.pattern.get(start + 1) != Some(&b'A') {
                    return arg_error(a, "expected one argument");
                }
                queued.push((idx, vec![self.args[start + 1].clone()]));
                stop = start + 2;
            } else {
                queued.push((idx, Vec::new()));
                stop = start + 1;
            }
            break;
        }
        for (i, values) in queued {
            self.take(i, values, ns, path)?;
        }
        Ok(stop)
    }
}

pub(crate) fn parse(spec: &Spec, args: &[String], ns: &mut HashMap<&'static str, Val>, path: &mut Vec<&'static str>) -> Step<Vec<String>> {
    parse_inner(spec, args, ns, path).map_err(|e| match e {
        Exit::Error(Error::Cli(m)) => Exit::Arg(spec.path.clone(), m),
        other => other,
    })
}

fn parse_inner(spec: &Spec, args: &[String], ns: &mut HashMap<&'static str, Val>, path: &mut Vec<&'static str>) -> Step<Vec<String>> {
    for a in &spec.actions {
        if !matches!(a.kind, Kind::Help) {
            ns.insert(a.dest, a.default.clone());
        }
    }
    let mut pattern = Vec::with_capacity(args.len());
    let mut options: HashMap<usize, Vec<Tuple>> = HashMap::new();
    let mut dashed = false;
    for (i, arg) in args.iter().enumerate() {
        if dashed {
            pattern.push(b'A');
        } else if arg == "--" {
            dashed = true;
            pattern.push(b'-');
        } else if let Some(t) = parse_optional(spec, arg) {
            options.insert(i, t);
            pattern.push(b'O');
        } else {
            pattern.push(b'A');
        }
    }
    let positionals = spec.actions.iter().enumerate().filter(|(_, a)| matches!(a.kind, Kind::Positional | Kind::Parser)).map(|(i, _)| i).collect();
    let mut run = Run { spec, args, pattern, seen: HashSet::new(), seen_non_default: HashSet::new(), own_extras: Vec::new(), sub_extras: Vec::new(), positionals };
    let mut start = 0usize;
    if let Some(max_option) = options.keys().max().copied() {
        while start <= max_option {
            let mut next = start;
            while next <= max_option && !options.contains_key(&next) {
                next += 1;
            }
            if start != next {
                let end = run.consume_positionals(start, ns, path)?;
                if end > start {
                    start = end;
                    continue;
                }
                start = end;
            }
            if !options.contains_key(&start) {
                run.own_extras.extend(args[start..next].iter().cloned());
                start = next;
            }
            start = run.consume_optional(start, &options[&start], ns, path)?;
        }
    }
    let end = run.consume_positionals(start, ns, path)?;
    run.own_extras.extend(args[end..].iter().cloned());
    let mut missing = Vec::new();
    for (i, a) in spec.actions.iter().enumerate() {
        if !run.seen.contains(&i) && (a.required || a.kind == Kind::Positional) {
            missing.push(a.name());
        }
    }
    if !missing.is_empty() {
        return plain_error(format!("the following arguments are required: {}", missing.join(", ")));
    }
    let mut extras = run.own_extras;
    extras.extend(run.sub_extras);
    Ok(extras)
}

pub(crate) fn parse_args(spec: &Spec, argv: &[String]) -> Step<Parsed> {
    let mut ns = HashMap::new();
    let mut path = Vec::new();
    let extras = parse(spec, argv, &mut ns, &mut path)?;
    if !extras.is_empty() {
        return Err(Exit::Arg(spec.path.clone(), format!("unrecognized arguments: {}", extras.join(" "))));
    }
    Ok(Parsed { ns, path })
}



/// Argparse's own reporting for a parser that does not override `error()`:
/// usage and `PROG: error: MESSAGE` on stderr, exit status 2.
pub(crate) fn report(prog_of: impl Fn(&str) -> String, exit: Exit) -> i32 {
    use std::io::Write;
    match exit {
        Exit::Help(text) => {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
            0
        }
        Exit::Arg(path, message) => {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "{}{}: error: {message}\n", crate::help::usage(&path), prog_of(&path));
            2
        }
        Exit::Error(e) => {
            eprintln!("{}", e.message());
            1
        }
    }
}
