//! WartLog daemon API (spec section 10).
//!
//! - [`proto`]: request and response messages, and the framing (`u32` big-endian length, then CBOR).
//! - [`client`]: an async client for the daemon socket (feature `async`, on by default).
//! - [`blocking`]: a client on plain blocking sockets, for hosts without an async runtime.
//! - [`ops`]: the operations that work on a store snapshot (query, schema, record context, traces,
//!   templates). Local clients call these in-process; the daemon calls the same code to answer
//!   clients that cannot read the files.

pub mod blocking;
#[cfg(feature = "async")]
pub mod client;
pub mod ops;
pub mod proto;

#[cfg(feature = "async")]
pub use client::Client;
pub use proto::{Request, Response};

use std::path::PathBuf;

/// Bumped on any incompatible message change. The handshake refuses a mismatch.
pub const PROTOCOL_VERSION: u32 = 1;

/// `$XDG_RUNTIME_DIR/wartlog`, or `/tmp/wartlog-$UID` when there is no runtime dir.
pub fn runtime_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("wartlog");
    }
    let uid = std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap_or(0);
    PathBuf::from(format!("/tmp/wartlog-{uid}"))
}

pub fn api_socket_path() -> PathBuf {
    runtime_dir().join("api.sock")
}

pub fn ingest_socket_path() -> PathBuf {
    runtime_dir().join("ingest.sock")
}

/// A KQL string literal for `s`.
pub fn kql_string(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A KQL literal for a value (numbers and booleans bare, everything else as a string).
pub fn kql_literal(v: &wartlog_model::Value) -> String {
    use wartlog_model::Value::*;
    match v {
        I64(i) => i.to_string(),
        F64(f) if f.is_finite() => format!("{f:?}"),
        Bool(b) => b.to_string(),
        other => kql_string(&other.to_text()),
    }
}

/// A KQL column reference for an attribute key: bare when it is a plain identifier that is not a
/// built-in column, else `Attributes["key"]`.
pub fn kql_attr(key: &str) -> String {
    let plain = key.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && key.chars().all(|c| c.is_alphanumeric() || c == '_');
    let builtin = !matches!(wartlog_query::ast::Column::from_name(key), wartlog_query::ast::Column::Named(_));
    if plain && !builtin && !wartlog_query::is_keyword(key) {
        key.to_string()
    } else {
        format!("Attributes[{}]", kql_string(key))
    }
}
