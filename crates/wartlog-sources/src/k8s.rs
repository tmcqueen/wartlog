//! Kubernetes collector (spec "Kubernetes"): reads container logs through the API server with the
//! user's kubeconfig, like `kubectl logs -f`, for every container of every matching pod.
//!
//! Pods are listed every `poll` interval. Each running container gets a follow stream
//! (`follow=true`, `timestamps=true`). The resume position per container is the last durable line's
//! timestamp plus how many lines carried exactly that timestamp, so a restart re-requests from one
//! second earlier (the API only takes whole seconds) and skips what it already has.
//! Nothing is installed in the cluster.

use anyhow::{Context, Result};
use futures::{AsyncBufReadExt, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, LogParams};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use wartlog_model::{time, Record, Source, SourceKind, Value};

use crate::{ndjson, shutdown_signal, Collector, CursorStore, Shutdown, Sink};

pub struct KubernetesCollector {
    pub name: String,
    /// kubeconfig context; None = current context
    pub context: Option<String>,
    /// namespaces to read; empty = all namespaces the user can list
    pub namespaces: Vec<String>,
    pub label_selector: Option<String>,
    /// pod labels copied onto each record as `k8s.pod.label.<key>`
    pub labels: Vec<String>,
    /// how far back to read a container seen for the first time
    pub backfill: Duration,
    pub poll: Duration,
}

/// Where a container stream resumes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Resume {
    pub ts: i64,
    /// lines already taken with exactly `ts`
    pub same: u32,
}

impl Resume {
    pub fn parse(s: &str) -> Option<Resume> {
        let (a, b) = s.split_once('+')?;
        Some(Resume { ts: a.parse().ok()?, same: b.parse().ok()? })
    }

    pub fn encode(&self) -> String {
        format!("{}+{}", self.ts, self.same)
    }

    /// True if a line with timestamp `ts` was already taken; updates the position otherwise.
    /// `seen_at_ts` counts lines at the current `ts` in this stream (reset when the ts moves).
    pub fn skip(&mut self, ts: i64, seen_at_ts: &mut u32) -> bool {
        if ts < self.ts {
            return true;
        }
        if ts == self.ts {
            *seen_at_ts += 1;
            if *seen_at_ts <= self.same {
                return true;
            }
            self.same = *seen_at_ts;
            return false;
        }
        self.ts = ts;
        self.same = 1;
        *seen_at_ts = 1;
        false
    }
}

/// Splits `2026-11-26T06:00:00.123456789Z message` into (ns, message).
pub fn split_timestamp(line: &str) -> Option<(i64, &str)> {
    let (ts, rest) = line.split_once(' ').unwrap_or((line, ""));
    Some((time::parse_datetime(ts)?, rest))
}

/// The owning workload: Deployment (through its ReplicaSet), StatefulSet, DaemonSet, Job or CronJob
/// (through its Job); else the pod name.
pub fn workload_name(pod: &Pod) -> (String, &'static str) {
    let name = pod.metadata.name.clone().unwrap_or_default();
    let owner = pod.metadata.owner_references.as_ref().and_then(|o| o.iter().find(|r| r.controller == Some(true)).or_else(|| o.first()));
    let Some(o) = owner else { return (name, "Pod") };
    let strip = |s: &str| -> String {
        // ReplicaSet `web-5d8f7c9b4` → `web`; Job from a CronJob `backup-29123456` → `backup`
        match s.rsplit_once('-') {
            Some((base, suffix)) if !base.is_empty() && suffix.len() >= 5 && suffix.chars().all(|c| c.is_ascii_alphanumeric()) => base.to_string(),
            _ => s.to_string(),
        }
    };
    match o.kind.as_str() {
        "ReplicaSet" => {
            // only strip when the pod carries the pod-template-hash the Deployment controller adds
            let hash = pod.metadata.labels.as_ref().and_then(|l| l.get("pod-template-hash"));
            match hash {
                Some(h) if o.name.ends_with(&format!("-{h}")) => (o.name[..o.name.len() - h.len() - 1].to_string(), "Deployment"),
                _ => (o.name.clone(), "ReplicaSet"),
            }
        }
        "Job" if o.name.rsplit_once('-').is_some_and(|(_, s)| s.len() >= 8 && s.chars().all(|c| c.is_ascii_digit())) => (strip(&o.name), "CronJob"),
        "StatefulSet" | "DaemonSet" | "Job" => (o.name.clone(), if o.kind == "Job" { "Job" } else if o.kind == "StatefulSet" { "StatefulSet" } else { "DaemonSet" }),
        _ => (o.name.clone(), "Pod"),
    }
}

/// One container to follow.
#[derive(Clone, Debug)]
struct Target {
    namespace: String,
    pod: String,
    uid: String,
    container: String,
    source: Arc<Source>,
    attrs: Vec<(String, Value)>,
}

impl Target {
    fn key(&self, cluster: &str) -> String {
        format!("k8s:{cluster}:{}/{}/{}/{}", self.namespace, self.pod, self.container, self.uid)
    }
}

fn targets(pod: &Pod, cluster: &str, labels: &[String]) -> Vec<Target> {
    let ns = pod.metadata.namespace.clone().unwrap_or_default();
    let name = pod.metadata.name.clone().unwrap_or_default();
    let uid = pod.metadata.uid.clone().unwrap_or_default();
    let (workload, wkind) = workload_name(pod);
    let node = pod.spec.as_ref().and_then(|s| s.node_name.clone()).unwrap_or_default();
    let statuses = pod.status.as_ref().and_then(|s| s.container_statuses.clone()).unwrap_or_default();
    let mut out = Vec::new();
    for cs in statuses {
        // a container that never started has no logs yet; one that terminated still does
        let started = cs.state.as_ref().is_some_and(|s| s.running.is_some() || s.terminated.is_some());
        if !started {
            continue;
        }
        let source = Arc::new(Source::new(
            SourceKind::K8s,
            vec![
                ("k8s.cluster.name".into(), cluster.to_string()),
                ("k8s.namespace.name".into(), ns.clone()),
                ("k8s.workload.name".into(), workload.clone()),
                ("k8s.container.name".into(), cs.name.clone()),
            ],
        ));
        let mut attrs = vec![
            ("k8s.pod.name".to_string(), Value::Str(name.clone())),
            ("k8s.workload.kind".to_string(), Value::Str(wkind.to_string())),
        ];
        if !node.is_empty() {
            attrs.push(("k8s.node.name".into(), Value::Str(node.clone())));
        }
        if let Some(l) = &pod.metadata.labels {
            for k in labels {
                if let Some(v) = l.get(k) {
                    attrs.push((format!("k8s.pod.label.{k}"), Value::Str(v.clone())));
                }
            }
        }
        out.push(Target { namespace: ns.clone(), pod: name.clone(), uid: uid.clone(), container: cs.name, source, attrs });
    }
    out
}

/// Turns one log line (already split from its timestamp) into a record.
pub fn line_to_record(msg: &str, ts: i64, source: &Arc<Source>, attrs: &[(String, Value)]) -> Record {
    let now = time::now_ns();
    let mut r = ndjson::normalize(msg, "", ts);
    // the line's own timestamp (JSON `time` and friends) wins; otherwise the kubelet's
    r.observed_ns = now;
    r.source = source.clone();
    r.attrs.extend(attrs.iter().cloned());
    r
}

async fn follow(client: kube::Client, t: Target, cluster: String, sink: Sink, cursors: CursorStore, backfill: Duration, mut shutdown: Shutdown) -> Result<()> {
    let key = t.key(&cluster);
    let mut resume = cursors.get(&key).and_then(|s| Resume::parse(&s)).unwrap_or_default();
    let api: Api<Pod> = Api::namespaced(client, &t.namespace);
    let mut lp = LogParams { container: Some(t.container.clone()), follow: true, timestamps: true, ..Default::default() };
    if resume.ts > 0 {
        let secs = resume.ts.div_euclid(time::NS) - 1;
        lp.since_time = k8s_openapi::chrono::DateTime::from_timestamp(secs, 0);
    } else {
        lp.since_seconds = Some(backfill.as_secs().max(1) as i64);
    }
    let stream = api.log_stream(&t.pod, &lp).await.with_context(|| format!("log stream {}/{}/{}", t.namespace, t.pod, t.container))?;
    let mut lines = stream.lines();
    let mut seen_at_ts = 0u32;
    let mut batch = Vec::new();
    loop {
        let next = tokio::select! {
            _ = shutdown.changed() => None,
            n = tokio::time::timeout(Duration::from_millis(200), lines.next()) => Some(n),
        };
        let line = match next {
            None => break,
            Some(Ok(Some(l))) => Some(l?),
            Some(Ok(None)) => {
                // the stream ended (container exited or the API closed it)
                if !batch.is_empty() {
                    sink.send_durable(std::mem::take(&mut batch)).await?;
                    cursors.set(&key, &resume.encode())?;
                }
                return Ok(());
            }
            Some(Err(_)) => None, // idle: flush below
        };
        if let Some(l) = line {
            if let Some((ts, msg)) = split_timestamp(&l) {
                if resume.skip(ts, &mut seen_at_ts) {
                    continue;
                }
                batch.push(line_to_record(msg, ts, &t.source, &t.attrs));
            }
            if batch.len() < 512 {
                continue;
            }
        }
        if !batch.is_empty() {
            sink.send_durable(std::mem::take(&mut batch)).await?;
            cursors.set(&key, &resume.encode())?;
        }
    }
    if !batch.is_empty() {
        sink.send_durable(batch).await?;
        cursors.set(&key, &resume.encode())?;
    }
    Ok(())
}

impl KubernetesCollector {
    async fn client(&self) -> Result<(kube::Client, String)> {
        let opts = kube::config::KubeConfigOptions { context: self.context.clone(), ..Default::default() };
        let cfg = kube::Config::from_kubeconfig(&opts).await.context("reading kubeconfig")?;
        let cluster = match &self.context {
            Some(c) => c.clone(),
            None => kube::config::Kubeconfig::read().ok().and_then(|k| k.current_context).unwrap_or_else(|| "default".into()),
        };
        Ok((kube::Client::try_from(cfg)?, cluster))
    }

    async fn list(&self, client: &kube::Client) -> Result<Vec<Pod>> {
        let mut lp = ListParams::default();
        if let Some(s) = &self.label_selector {
            lp = lp.labels(s);
        }
        let mut pods = Vec::new();
        if self.namespaces.is_empty() {
            pods.extend(Api::<Pod>::all(client.clone()).list(&lp).await?.items);
        } else {
            for ns in &self.namespaces {
                pods.extend(Api::<Pod>::namespaced(client.clone(), ns).list(&lp).await.with_context(|| format!("listing pods in {ns}"))?.items);
            }
        }
        Ok(pods)
    }
}

#[async_trait::async_trait]
impl Collector for KubernetesCollector {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(self: Box<Self>, sink: Sink, cursors: CursorStore, shutdown: Shutdown) -> Result<()> {
        let (client, cluster) = self.client().await?;
        tracing::info!("kubernetes collector {} reading context {cluster}", self.name);
        // running streams by cursor key; a finished stream is restarted on the next poll if its
        // container is still running (for example after the API server closed it)
        let mut running: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
        let stop = shutdown_signal(shutdown.clone());
        tokio::pin!(stop);
        loop {
            match self.list(&client).await {
                Ok(pods) => {
                    let mut live = BTreeMap::new();
                    for p in &pods {
                        let phase = p.status.as_ref().and_then(|s| s.phase.clone()).unwrap_or_default();
                        for t in targets(p, &cluster, &self.labels) {
                            live.insert(t.key(&cluster), (t, phase == "Running"));
                        }
                    }
                    running.retain(|k, h| {
                        let keep = live.contains_key(k) && !h.is_finished();
                        if !keep {
                            h.abort();
                        }
                        keep
                    });
                    for (k, (t, is_running)) in live.iter() {
                        if running.contains_key(k) {
                            continue;
                        }
                        // a finished container is read once (to its end) and then left alone
                        let done_once = !is_running && cursors.get(k).is_some();
                        if done_once {
                            continue;
                        }
                        let (c, t, cl, s, cu, sd) = (client.clone(), t.clone(), cluster.clone(), sink.clone(), cursors.clone(), shutdown.clone());
                        let bf = self.backfill;
                        running.insert(
                            k.clone(),
                            tokio::spawn(async move {
                                if let Err(e) = follow(c, t, cl, s.clone(), cu, bf, sd).await {
                                    tracing::debug!("{e:#}");
                                    s.status.error(format!("{e:#}"));
                                }
                            }),
                        );
                    }
                    // forget cursors of pods that are gone
                    let prefix = format!("k8s:{cluster}:");
                    let _ = cursors.retain_prefix(&prefix, &|k| live.contains_key(k));
                }
                Err(e) => sink.status.error(format!("{e:#}")),
            }
            tokio::select! {
                _ = &mut stop => break,
                _ = tokio::time::sleep(self.poll) => {}
            }
        }
        for (_, h) in running {
            h.abort();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

    fn pod(owner: Option<(&str, &str)>, hash: Option<&str>) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some("web-5d8f7c9b4-x2k9q".into()),
                owner_references: owner.map(|(k, n)| vec![OwnerReference { kind: k.into(), name: n.into(), controller: Some(true), ..Default::default() }]),
                labels: hash.map(|h| [("pod-template-hash".to_string(), h.to_string())].into()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn workloads() {
        assert_eq!(workload_name(&pod(Some(("ReplicaSet", "web-5d8f7c9b4")), Some("5d8f7c9b4"))), ("web".into(), "Deployment"));
        assert_eq!(workload_name(&pod(Some(("ReplicaSet", "web-5d8f7c9b4")), None)), ("web-5d8f7c9b4".into(), "ReplicaSet"));
        assert_eq!(workload_name(&pod(Some(("StatefulSet", "db")), None)), ("db".into(), "StatefulSet"));
        assert_eq!(workload_name(&pod(Some(("Job", "backup-29123456")), None)), ("backup".into(), "CronJob"));
        assert_eq!(workload_name(&pod(Some(("Job", "migrate")), None)), ("migrate".into(), "Job"));
        assert_eq!(workload_name(&pod(None, None)), ("web-5d8f7c9b4-x2k9q".into(), "Pod"));
    }

    #[test]
    fn resume_skips_overlap() {
        // first run took lines at t=10 (x2) and t=11 (x1)
        let mut r = Resume::default();
        let mut n = 0;
        let taken: Vec<bool> = [10, 10, 11].iter().map(|&t| !r.skip(t, &mut n)).collect();
        assert_eq!(taken, vec![true, true, true]);
        assert_eq!(r, Resume { ts: 11, same: 1 });
        // the restart re-reads from a second earlier: 9, 10, 10, 11, then new 11, 12
        let mut r = Resume::parse(&r.encode()).unwrap();
        let mut n = 0;
        let taken: Vec<bool> = [9, 10, 10, 11, 11, 12].iter().map(|&t| !r.skip(t, &mut n)).collect();
        assert_eq!(taken, vec![false, false, false, false, true, true]);
    }

    #[test]
    fn lines() {
        let (ts, msg) = split_timestamp("2026-11-26T06:00:00.123456789Z {\"level\":\"error\",\"msg\":\"db down\"}").unwrap();
        assert_eq!(ts % time::NS, 123_456_789);
        let src = Arc::new(Source::new(SourceKind::K8s, vec![("k8s.namespace.name".into(), "orders".into()), ("k8s.workload.name".into(), "api".into()), ("k8s.container.name".into(), "app".into())]));
        let r = line_to_record(msg, ts, &src, &[("k8s.pod.name".into(), Value::Str("api-1".into()))]);
        assert_eq!(r.severity, 17);
        assert_eq!(r.body, "db down");
        assert_eq!(r.time_ns, ts);
        assert_eq!(r.source.display_name(), "orders/api/app");
        let r = line_to_record("GET /health 200", ts, &src, &[]);
        assert_eq!(r.body, "GET /health 200");
        assert_eq!(r.time_ns, ts);
    }
}
