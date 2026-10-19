//! The board replayed up to one journal position (`board.View`).

use crate::error::{Error, Result};
use crate::journal::{Position, Record};
use crate::json::{self, Value};
use crate::snapshot::Requests;
use crate::state::{self, State};

#[derive(Clone, Default)]
pub struct View {
    pub position: Option<Position>,
    pub state: State,
    /// request id -> [seq, start, end] of the record that holds it.
    pub requests: Requests,
}

impl View {
    pub fn new() -> View {
        View { position: None, state: State::empty(), requests: Requests::default() }
    }

    /// Apply committed records in order; each must reproduce its recorded result.
    pub fn replay(&mut self, records: &[Record], positions: &[Position]) -> Result<()> {
        for (env, at) in records.iter().zip(positions) {
            self.replay_one(env, at)?;
        }
        Ok(())
    }

    /// Apply one committed record.
    pub fn replay_one(&mut self, env: &Record, at: &Position) -> Result<()> {
        {
            let event = &env.payload;
            if !matches!(event.at("version"), Value::Int(1)) && !matches!(event.at("version"), Value::Bool(true)) && !matches!(event.at("version"), Value::Float(f) if *f == 1.0) {
                return Err(Error::Corrupt("unsupported board event version".into()));
            }
            let req = event.at("request");
            let id = match req.at("id") {
                Value::Str(s) => s.clone(),
                other => state::py_str(other),
            };
            if self.requests.contains_key(&id) {
                return Err(Error::Corrupt("duplicate canonical request ID".into()));
            }
            let recorded_at = event.at("recorded_at").as_str().unwrap_or("");
            if event.at("accepted").truthy() {
                let result = state::apply(&mut self.state, req, recorded_at)
                    .map_err(|e| Error::Corrupt(format!("accepted event cannot replay: {}", e.message())))?;
                if !result.py_eq(event.at("result")) {
                    return Err(Error::Corrupt("reducer result differs from recorded result".into()));
                }
            } else {
                let reason = state::py_str(event.at("result").at("error"));
                state::record_rejection(&mut self.state, req, &reason)?;
            }
            self.requests.insert(id, [at.seq, at.start, at.end]);
        }
        self.position = Some(at.clone());
        Ok(())
    }

    /// `canonical(self.requests)`.
    pub fn encode_requests(&self, out: &mut Vec<u8>) {
        let mut entries = self.requests.entries();
        entries.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        out.push(b'{');
        for (n, (k, [a, b, c])) in entries.into_iter().enumerate() {
            if n > 0 {
                out.push(b',');
            }
            json::encode_str(k, false, out);
            out.extend_from_slice(format!(":[{a},{b},{c}]").as_bytes());
        }
        out.push(b'}');
    }

    /// `digest(canonical([state, requests]))`.
    pub fn digest(&self) -> String {
        crate::pyfmt::sha256_hex(&self.canonical())
    }

    /// `canonical([state, requests])`.
    pub fn canonical(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 << 20);
        out.push(b'[');
        self.state.encode_canonical(&mut out);
        out.push(b',');
        self.encode_requests(&mut out);
        out.push(b']');
        out
    }
}
