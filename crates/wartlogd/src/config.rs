//! `wartlog.toml` (spec section 12): parsing, defaults, and comment-preserving edits.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use wartlog_model::time::parse_duration;

pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()).map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        home.join(".config")
    });
    base.join("wartlog").join("wartlog.toml")
}

/// The file written the first time the user changes sources (and by `wl setup`). It spells out the
/// built-in defaults so they can be edited.
pub fn default_text() -> String {
    format!(
        r#"# WartLog configuration. wartlogd reloads this file when it changes.
# Docs: https://github.com/tmcqueen/wartlog (spec section 12)

[storage]
# path = "~/.local/share/wartlog"
max_size = "10GiB"
max_age = "30d"

# OpenTelemetry (Aspire apps): OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317
[[receiver]]
type = "otlp-grpc"
listen = "127.0.0.1:4317"
# forward = [{{ endpoint = "http://localhost:18889", headers = {{ "x-otlp-api-key" = "..." }} }}]

# OTLP/HTTP (/v1/logs, /v1/traces) and NDJSON (POST /ingest?source=name)
[[receiver]]
type = "http"
listen = "127.0.0.1:4318"

# NDJSON or plain text lines: `my-app 2>&1 | wl send --source my-app`
[[receiver]]
type = "ndjson-unix"
path = "{}"

[[receiver]]
type = "ndjson-tcp"
listen = "127.0.0.1:5170"

[[collector]]
type = "journald"
journals = ["user"]
backfill = "24h"

# [[collector]]
# type = "kubernetes"
# context = "kind-dev"
# namespaces = ["orders"]
# label_selector = "app.kubernetes.io/part-of=shop"
"#,
        wartlog_api::ingest_socket_path().display()
    )
}

#[derive(Clone, Debug, PartialEq)]
pub struct Storage {
    pub path: PathBuf,
    pub max_size: Option<u64>,
    pub max_age: Option<Duration>,
}

/// One configured source, after defaults. `table` is its TOML, for display and change detection.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceCfg {
    pub name: String,
    /// receiver or collector
    pub role: &'static str,
    pub kind: String,
    pub enabled: bool,
    pub spec: Spec,
    pub table: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Spec {
    OtlpGrpc { listen: String, forward: Vec<(String, BTreeMap<String, String>)> },
    Http { listen: String, otlp: bool, ndjson: bool, source: String, forward: Vec<(String, BTreeMap<String, String>)> },
    NdjsonUnix { path: PathBuf, source: String },
    NdjsonTcp { listen: String, source: String },
    Journald { journal: String, directory: Option<PathBuf>, backfill: Duration, include: Vec<String>, exclude: Vec<String> },
    Kubernetes { context: Option<String>, namespaces: Vec<String>, label_selector: Option<String>, labels: Vec<String>, backfill: Duration },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub storage: Storage,
    pub sources: Vec<SourceCfg>,
}

pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (n, u) = s.split_at(split);
    let n: f64 = n.trim().parse().ok()?;
    let mul: f64 = match u.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "kib" => 1024.0,
        "m" | "mb" => 1e6,
        "mib" => 1048576.0,
        "g" | "gb" => 1e9,
        "gib" => 1073741824.0,
        "t" | "tb" => 1e12,
        "tib" => 1099511627776.0,
        _ => return None,
    };
    Some((n * mul) as u64)
}

fn dur(s: &str) -> Result<Duration> {
    let ns = parse_duration(s).ok_or_else(|| anyhow!("bad duration `{s}` (try 30d, 24h, 15m)"))?;
    Ok(Duration::from_nanos(ns.max(0) as u64))
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
        None => PathBuf::from(p),
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawStorage {
    path: Option<String>,
    max_size: Option<toml::Value>,
    max_age: Option<String>,
}

#[derive(Deserialize)]
struct Forward {
    endpoint: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSource {
    #[serde(rename = "type")]
    kind: String,
    name: Option<String>,
    #[serde(default = "yes")]
    enabled: bool,
    listen: Option<String>,
    path: Option<String>,
    source: Option<String>,
    #[serde(default)]
    forward: Vec<Forward>,
    otlp: Option<bool>,
    ndjson: Option<bool>,
    // journald
    journals: Option<Vec<String>>,
    directory: Option<String>,
    backfill: Option<String>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    // kubernetes
    context: Option<String>,
    #[serde(default)]
    namespaces: Vec<String>,
    label_selector: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
}

fn yes() -> bool {
    true
}

fn source_list(role: &'static str, tables: &[toml::Table], out: &mut Vec<SourceCfg>) -> Result<()> {
    for t in tables {
        let text = toml::to_string(t).unwrap_or_default();
        let r: RawSource = toml::Value::Table(t.clone()).try_into().with_context(|| format!("in a [[{role}]] table:\n{text}"))?;
        let fwd = || r.forward.iter().map(|f| (f.endpoint.clone(), f.headers.clone())).collect::<Vec<_>>();
        let base = r.name.clone().unwrap_or_else(|| r.kind.clone());
        let mk = |name: String, spec: Spec| SourceCfg { name, role, kind: r.kind.clone(), enabled: r.enabled, spec, table: text.clone() };
        match (role, r.kind.as_str()) {
            ("receiver", "otlp-grpc") => out.push(mk(base, Spec::OtlpGrpc { listen: r.listen.clone().unwrap_or("127.0.0.1:4317".into()), forward: fwd() })),
            ("receiver", "http" | "otlp-http") => out.push(mk(
                base,
                Spec::Http {
                    listen: r.listen.clone().unwrap_or("127.0.0.1:4318".into()),
                    otlp: r.otlp.unwrap_or(true),
                    ndjson: r.ndjson.unwrap_or(true),
                    source: r.source.clone().unwrap_or("http".into()),
                    forward: fwd(),
                },
            )),
            ("receiver", "ndjson-unix") => out.push(mk(
                base,
                Spec::NdjsonUnix { path: r.path.as_deref().map(expand).unwrap_or_else(wartlog_api::ingest_socket_path), source: r.source.clone().unwrap_or("ndjson".into()) },
            )),
            ("receiver", "ndjson-tcp") => {
                out.push(mk(base, Spec::NdjsonTcp { listen: r.listen.clone().unwrap_or("127.0.0.1:5170".into()), source: r.source.clone().unwrap_or("tcp".into()) }))
            }
            ("collector", "journald") => {
                let backfill = dur(r.backfill.as_deref().unwrap_or("24h"))?;
                if let Some(d) = &r.directory {
                    out.push(mk(base, Spec::Journald { journal: "directory".into(), directory: Some(expand(d)), backfill, include: r.include.clone(), exclude: r.exclude.clone() }));
                } else {
                    let journals = r.journals.clone().unwrap_or_else(|| vec!["user".into()]);
                    for j in &journals {
                        if j != "user" && j != "system" {
                            bail!("journald: unknown journal `{j}` (use \"user\" or \"system\")");
                        }
                        let name = if journals.len() > 1 || r.name.is_none() { format!("{base}:{j}") } else { base.clone() };
                        out.push(mk(name, Spec::Journald { journal: j.clone(), directory: None, backfill, include: r.include.clone(), exclude: r.exclude.clone() }));
                    }
                }
            }
            ("collector", "kubernetes") => {
                let name = r.name.clone().unwrap_or_else(|| format!("kubernetes:{}", r.context.as_deref().unwrap_or("current")));
                out.push(mk(
                    name,
                    Spec::Kubernetes {
                        context: r.context.clone(),
                        namespaces: r.namespaces.clone(),
                        label_selector: r.label_selector.clone(),
                        labels: r.labels.clone(),
                        backfill: dur(r.backfill.as_deref().unwrap_or("1h"))?,
                    },
                ))
            }
            (role, k) => bail!("unknown {role} type `{k}` (receivers: otlp-grpc, http, ndjson-unix, ndjson-tcp; collectors: journald, kubernetes)"),
        }
    }
    Ok(())
}

impl Config {
    pub fn parse(text: &str) -> Result<Config> {
        let mut doc: toml::Table = toml::from_str(text)?;
        let storage: RawStorage = match doc.remove("storage") {
            Some(v) => v.try_into().context("in [storage]")?,
            None => RawStorage::default(),
        };
        let take = |doc: &mut toml::Table, k: &str| -> Result<Vec<toml::Table>> {
            match doc.remove(k) {
                None => Ok(vec![]),
                Some(toml::Value::Array(a)) => a.into_iter().map(|v| v.try_into::<toml::Table>().map_err(|e| anyhow!("[[{k}]]: {e}"))).collect(),
                Some(_) => bail!("`{k}` must be written [[{k}]]"),
            }
        };
        let receivers = take(&mut doc, "receiver")?;
        let collectors = take(&mut doc, "collector")?;
        if let Some(k) = doc.keys().next() {
            bail!("unknown top-level key `{k}` (expected [storage], [[receiver]], [[collector]])");
        }
        let mut sources = Vec::new();
        source_list("receiver", &receivers, &mut sources)?;
        source_list("collector", &collectors, &mut sources)?;
        // unique names
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for s in &mut sources {
            let n = seen.entry(s.name.clone()).or_default();
            *n += 1;
            if *n > 1 {
                s.name = format!("{}-{}", s.name, n);
            }
        }
        let max_size = match &storage.max_size {
            None => Some(10 << 30),
            Some(toml::Value::String(s)) if s == "none" || s == "unlimited" => None,
            Some(toml::Value::String(s)) => Some(parse_size(s).ok_or_else(|| anyhow!("bad max_size `{s}` (try 10GiB)"))?),
            Some(toml::Value::Integer(i)) => Some(*i as u64),
            Some(v) => bail!("bad max_size {v}"),
        };
        let max_age = match storage.max_age.as_deref() {
            None => Some(Duration::from_secs(30 * 86400)),
            Some("none" | "unlimited") => None,
            Some(s) => Some(dur(s)?),
        };
        let path = storage.path.as_deref().map(expand).unwrap_or_else(wartlog_store::default_root);
        Ok(Config { storage: Storage { path, max_size, max_age }, sources })
    }

    /// The file, or the built-in defaults when it does not exist.
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(t) => Config::parse(&t).with_context(|| format!("{}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::parse(&default_text()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

// ---------- edits (the daemon is the only programmatic writer) ----------

fn read_doc(path: &Path) -> Result<toml_edit::DocumentMut> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => default_text(),
        Err(e) => return Err(e.into()),
    };
    Ok(text.parse()?)
}

fn write_doc(path: &Path, doc: &toml_edit::DocumentMut) -> Result<()> {
    let text = doc.to_string();
    // refuse to write something we cannot read back
    Config::parse(&text)?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Finds the table that produced source `name` (by its `name` key or its generated name).
fn find(doc: &toml_edit::DocumentMut, name: &str) -> Result<(String, usize)> {
    let cfg = Config::parse(&doc.to_string())?;
    let target = cfg.sources.iter().find(|s| s.name == name).ok_or_else(|| anyhow!("no source named `{name}`"))?;
    for role in ["receiver", "collector"] {
        if let Some(arr) = doc.get(role).and_then(|i| i.as_array_of_tables()) {
            for (i, t) in arr.iter().enumerate() {
                let text = toml::to_string(&toml::from_str::<toml::Table>(&t.to_string()).unwrap_or_default()).unwrap_or_default();
                if role == target.role && text == target.table {
                    return Ok((role.to_string(), i));
                }
            }
        }
    }
    bail!("source `{name}` is not in the config file")
}

fn parse_table(config: &str) -> Result<toml_edit::Table> {
    let d: toml_edit::DocumentMut = config.parse().context("source config must be TOML key = value lines")?;
    Ok(d.as_table().clone())
}

pub fn add_source(path: &Path, role: &str, config: &str) -> Result<()> {
    anyhow::ensure!(role == "receiver" || role == "collector", "kind must be receiver or collector");
    let mut doc = read_doc(path)?;
    let t = parse_table(config)?;
    let entry = doc.entry(role).or_insert(toml_edit::Item::ArrayOfTables(Default::default()));
    entry.as_array_of_tables_mut().ok_or_else(|| anyhow!("`{role}` is not an array of tables"))?.push(t);
    write_doc(path, &doc)
}

pub fn remove_source(path: &Path, name: &str) -> Result<()> {
    let mut doc = read_doc(path)?;
    let (role, i) = find(&doc, name)?;
    doc[role.as_str()].as_array_of_tables_mut().unwrap().remove(i);
    write_doc(path, &doc)
}

pub fn update_source(path: &Path, name: &str, config: &str) -> Result<()> {
    let mut doc = read_doc(path)?;
    let (role, i) = find(&doc, name)?;
    let t = parse_table(config)?;
    let arr = doc[role.as_str()].as_array_of_tables_mut().unwrap();
    let slot = arr.get_mut(i).unwrap();
    // keep the comments above the table
    let decor = slot.decor().clone();
    *slot = t;
    *slot.decor_mut() = decor;
    write_doc(path, &doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_parse() {
        let c = Config::parse(&default_text()).unwrap();
        let names: Vec<&str> = c.sources.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["otlp-grpc", "http", "ndjson-unix", "ndjson-tcp", "journald:user"]);
        assert_eq!(c.storage.max_size, Some(10 << 30));
        assert_eq!(c.storage.max_age, Some(Duration::from_secs(30 * 86400)));
        let c = Config::parse(
            r#"
            [storage]
            max_size = "500MiB"
            max_age = "none"
            [[collector]]
            type = "journald"
            journals = ["user", "system"]
            [[collector]]
            type = "kubernetes"
            context = "kind-dev"
            namespaces = ["orders"]
            "#,
        )
        .unwrap();
        assert_eq!(c.storage.max_size, Some(500 << 20));
        assert_eq!(c.storage.max_age, None);
        assert_eq!(c.sources.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["journald:user", "journald:system", "kubernetes:kind-dev"]);
        assert!(Config::parse("[[receiver]]\ntype = \"carrier-pigeon\"").unwrap_err().to_string().contains("unknown receiver type"));
        assert!(Config::parse("[[receiver]]\ntype = \"http\"\nlisten_on = 1").is_err(), "unknown keys are errors");
    }

    #[test]
    fn edits_keep_comments() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("wartlog.toml");
        std::fs::write(&p, "# mine\n[storage]\nmax_age = \"7d\" # a week\n\n# the journal\n[[collector]]\ntype = \"journald\"\n").unwrap();
        add_source(&p, "collector", "type = \"kubernetes\"\nname = \"k\"\ncontext = \"kind-dev\"").unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(t.contains("# a week") && t.contains("# the journal") && t.contains("kind-dev"), "{t}");
        update_source(&p, "k", "type = \"kubernetes\"\nname = \"k\"\ncontext = \"prod\"").unwrap();
        let c = Config::load(&p).unwrap();
        assert!(matches!(&c.sources[1].spec, Spec::Kubernetes { context: Some(x), .. } if x == "prod"));
        remove_source(&p, "journald:user").unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(t.contains("# a week") && !t.contains("journald"), "{t}");
        assert!(add_source(&p, "receiver", "type = \"bogus\"").is_err());
        assert!(remove_source(&p, "nope").is_err());
    }
}
