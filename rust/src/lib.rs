//! Agent Collab, Rust build: a local append-only collaboration board for
//! coding agents, sharing its journal byte for byte with the Python build.

pub mod error;
pub mod json;
pub mod pyfmt;
pub mod journal;
pub mod state;
pub mod snapshot;
pub mod view;
pub mod presence;
pub mod board;
pub mod git;
pub mod help;
pub mod moderator;
pub mod argparse;
pub mod cli;
pub mod viewer;
pub mod install;

use std::sync::atomic::{AtomicBool, Ordering};

static FORGET: AtomicBool = AtomicBool::new(false);

/// Skip freeing large values in a short-lived process; the OS reclaims them at exit.
pub fn set_forget(on: bool) {
    FORGET.store(on, Ordering::Relaxed);
}

pub fn forgetting() -> bool {
    FORGET.load(Ordering::Relaxed)
}

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Allow transparent huge pages again before work that builds a large heap
/// (a full replay): fewer page faults and a faster exit outweigh zeroing.
pub fn bulk_memory() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_THP_DISABLE, 0, 0, 0, 0);
    }
}

pub fn discard<T>(v: T) {
    if FORGET.load(Ordering::Relaxed) {
        std::mem::forget(v);
    } else {
        drop(v);
    }
}

/// The map inside an object value (empty for anything else).
pub fn cli_map(v: json::Value) -> json::Map {
    match v {
        json::Value::Obj(m) => m,
        _ => json::Map::new(),
    }
}
