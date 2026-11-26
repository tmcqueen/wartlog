//! Runs the configured sources and applies config changes: sources whose table changed are
//! restarted, removed ones stopped, new ones started. A source that fails is retried with backoff.

use crate::config::{SourceCfg, Spec};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use wartlog_api::proto::SourceInfo;
use wartlog_sources::{CursorStore, Sink, SourceStatus, StatusRegistry};

struct Running {
    cfg: SourceCfg,
    stop: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
    status: Arc<SourceStatus>,
    state: Arc<Mutex<String>>,
}

pub struct Manager {
    running: BTreeMap<String, Running>,
    disabled: BTreeMap<String, SourceCfg>,
    sink: Sink,
    cursors: CursorStore,
    registry: StatusRegistry,
}

enum Built {
    R(Box<dyn wartlog_sources::Receiver>),
    C(Box<dyn wartlog_sources::Collector>),
}

fn build(c: &SourceCfg) -> anyhow::Result<Built> {
    let name = c.name.clone();
    Ok(match &c.spec {
        #[cfg(feature = "otlp")]
        Spec::OtlpGrpc { listen, forward } => {
            Built::R(Box::new(wartlog_sources::otlp::GrpcReceiver { name, listen: listen.clone(), forward: wartlog_sources::otlp::Forwarder::new(forward.clone()) }))
        }
        #[cfg(not(feature = "otlp"))]
        Spec::OtlpGrpc { .. } => anyhow::bail!("this wartlogd was built without OTLP support"),
        Spec::Http { listen, otlp, ndjson, source, forward } => {
            let _ = forward;
            Built::R(Box::new(wartlog_sources::http::HttpReceiver {
                name,
                listen: listen.clone(),
                otlp: *otlp,
                ndjson: *ndjson,
                default_source: source.clone(),
                #[cfg(feature = "otlp")]
                forward: wartlog_sources::otlp::Forwarder::new(forward.clone()),
            }))
        }
        Spec::NdjsonUnix { path, source } => Built::R(Box::new(wartlog_sources::listeners::UnixReceiver { name, path: path.clone(), source: source.clone() })),
        Spec::NdjsonTcp { listen, source } => Built::R(Box::new(wartlog_sources::listeners::TcpReceiver { name, listen: listen.clone(), source: source.clone() })),
        #[cfg(feature = "journald")]
        Spec::Journald { journal, directory, backfill, include, exclude } => {
            use wartlog_sources::journald::{JournalKind, JournaldCollector};
            let kind = match (journal.as_str(), directory) {
                (_, Some(d)) => JournalKind::Directory(d.clone()),
                ("system", _) => JournalKind::System,
                _ => JournalKind::User,
            };
            Built::C(Box::new(JournaldCollector { name, kind, backfill: *backfill, include: include.clone(), exclude: exclude.clone(), follow: true }))
        }
        #[cfg(not(feature = "journald"))]
        Spec::Journald { .. } => anyhow::bail!("this wartlogd was built without journald support"),
        #[cfg(feature = "kubernetes")]
        Spec::Kubernetes { context, namespaces, label_selector, labels, backfill } => Built::C(Box::new(wartlog_sources::k8s::KubernetesCollector {
            name,
            context: context.clone(),
            namespaces: namespaces.clone(),
            label_selector: label_selector.clone(),
            labels: labels.clone(),
            backfill: *backfill,
            poll: Duration::from_secs(10),
        })),
        #[cfg(not(feature = "kubernetes"))]
        Spec::Kubernetes { .. } => anyhow::bail!("this wartlogd was built without Kubernetes support"),
    })
}

/// How a program sends to this source.
pub fn hint(c: &SourceCfg) -> String {
    match &c.spec {
        Spec::OtlpGrpc { listen, .. } => format!("OTEL_EXPORTER_OTLP_ENDPOINT=http://{listen}"),
        Spec::Http { listen, otlp, ndjson, .. } => {
            let mut h = Vec::new();
            if *otlp {
                h.push(format!("OTEL_EXPORTER_OTLP_ENDPOINT=http://{listen} OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf"));
            }
            if *ndjson {
                h.push(format!("curl --data-binary @app.ndjson 'http://{listen}/ingest?source=my-app'"));
            }
            h.join("\n")
        }
        Spec::NdjsonUnix { .. } => "my-app 2>&1 | wl send --source my-app".into(),
        Spec::NdjsonTcp { listen, .. } => {
            let (h, p) = listen.rsplit_once(':').unwrap_or((listen, ""));
            format!("my-app 2>&1 | nc {h} {p}")
        }
        Spec::Journald { journal, .. } => format!("reads the {journal} journal"),
        Spec::Kubernetes { context, .. } => format!("reads pod logs from context {}", context.as_deref().unwrap_or("(current)")),
    }
}

impl Manager {
    pub fn new(sink: Sink, cursors: CursorStore, registry: StatusRegistry) -> Manager {
        Manager { running: BTreeMap::new(), disabled: BTreeMap::new(), sink, cursors, registry }
    }

    pub fn apply(&mut self, cfgs: &[SourceCfg]) {
        let wanted: BTreeMap<&str, &SourceCfg> = cfgs.iter().map(|c| (c.name.as_str(), c)).collect();
        let stale: Vec<String> = self.running.iter().filter(|(n, r)| wanted.get(n.as_str()).is_none_or(|c| **c != r.cfg || !c.enabled)).map(|(n, _)| n.clone()).collect();
        for n in stale {
            self.stop(&n);
        }
        self.disabled.clear();
        for c in cfgs {
            if !c.enabled {
                self.disabled.insert(c.name.clone(), c.clone());
                continue;
            }
            if !self.running.contains_key(&c.name) {
                self.start(c.clone());
            }
        }
    }

    fn stop(&mut self, name: &str) {
        if let Some(r) = self.running.remove(name) {
            tracing::info!("stopping source {name}");
            let _ = r.stop.send(true);
            // receivers exit on the signal; give them a moment, then abort
            let h = r.handle;
            tokio::spawn(async move {
                let ab = h.abort_handle();
                if tokio::time::timeout(Duration::from_secs(3), h).await.is_err() {
                    ab.abort();
                }
            });
            self.registry.remove(name);
        }
    }

    fn start(&mut self, cfg: SourceCfg) {
        let status = self.registry.register(&cfg.name, &cfg.kind);
        let state = Arc::new(Mutex::new("starting".to_string()));
        let (stop_tx, stop) = watch::channel(false);
        let sink = self.sink.with_status(status.clone());
        let cursors = self.cursors.clone();
        let (c, st) = (cfg.clone(), state.clone());
        tracing::info!("starting source {} ({})", cfg.name, cfg.kind);
        let handle = tokio::spawn(async move {
            let mut backoff = Duration::from_secs(2);
            loop {
                let res = match build(&c) {
                    Ok(Built::R(r)) => {
                        *st.lock().unwrap() = "running".into();
                        r.run(sink.clone(), stop.clone()).await
                    }
                    Ok(Built::C(col)) => {
                        *st.lock().unwrap() = "running".into();
                        col.run(sink.clone(), cursors.clone(), stop.clone()).await
                    }
                    Err(e) => Err(e),
                };
                if *stop.borrow() {
                    *st.lock().unwrap() = "stopped".into();
                    return;
                }
                let why = match res {
                    Ok(()) => "stopped unexpectedly".to_string(),
                    Err(e) => format!("{e:#}"),
                };
                tracing::warn!("source {} failed: {why}; retrying in {}s", c.name, backoff.as_secs());
                sink.status.error(&why);
                *st.lock().unwrap() = format!("failed: {why}");
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = wartlog_sources::shutdown_signal(stop.clone()) => {}
                }
                if *stop.borrow() {
                    *st.lock().unwrap() = "stopped".into();
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        });
        self.running.insert(cfg.name.clone(), Running { cfg, stop: stop_tx, handle, status, state });
    }

    pub fn stop_all(&mut self) -> Vec<tokio::task::JoinHandle<()>> {
        let names: Vec<String> = self.running.keys().cloned().collect();
        let mut hs = Vec::new();
        for n in names {
            if let Some(r) = self.running.remove(&n) {
                let _ = r.stop.send(true);
                hs.push(r.handle);
            }
        }
        hs
    }

    pub fn infos(&self) -> Vec<SourceInfo> {
        let mut out: Vec<SourceInfo> = self
            .running
            .values()
            .map(|r| {
                let s = &r.status;
                SourceInfo {
                    name: r.cfg.name.clone(),
                    role: r.cfg.role.to_string(),
                    kind: r.cfg.kind.clone(),
                    config: r.cfg.table.clone(),
                    enabled: true,
                    state: r.state.lock().unwrap().clone(),
                    received: s.received.load(Ordering::Relaxed),
                    dropped: s.dropped.load(Ordering::Relaxed),
                    errors: s.errors.load(Ordering::Relaxed),
                    last_seen_ns: s.last_seen_ns.load(Ordering::Relaxed),
                    last_error: s.last_error.lock().unwrap().clone(),
                    hint: hint(&r.cfg),
                }
            })
            .collect();
        out.extend(self.disabled.values().map(|c| SourceInfo {
            name: c.name.clone(),
            role: c.role.to_string(),
            kind: c.kind.clone(),
            config: c.table.clone(),
            enabled: false,
            state: "disabled".into(),
            hint: hint(c),
            ..Default::default()
        }));
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}
