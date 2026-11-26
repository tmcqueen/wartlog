//! OTLP: conversion to WartLog records, the gRPC services, and forwarding (tee) to other collectors
//! such as the Aspire dashboard.

use opentelemetry_proto::tonic::collector::logs::v1::{
    logs_service_client::LogsServiceClient,
    logs_service_server::{LogsService, LogsServiceServer},
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    metrics_service_client::MetricsServiceClient,
    metrics_service_server::{MetricsService, MetricsServiceServer},
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    trace_service_client::TraceServiceClient,
    trace_service_server::{TraceService, TraceServiceServer},
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{any_value::Value as AV, AnyValue, KeyValue};
use opentelemetry_proto::tonic::resource::v1::Resource;
use std::collections::BTreeMap;
use std::sync::Arc;
use wartlog_model::{template, time::now_ns, Record, Source, SourceKind, SpanData, Value};

use crate::{shutdown_signal, Receiver, Shutdown, Sink};

pub fn any_to_value(v: &AnyValue) -> Value {
    match &v.value {
        None => Value::Null,
        Some(AV::StringValue(s)) => Value::Str(s.clone()),
        Some(AV::BoolValue(b)) => Value::Bool(*b),
        Some(AV::IntValue(i)) => Value::I64(*i),
        Some(AV::DoubleValue(d)) => Value::F64(*d),
        Some(AV::BytesValue(b)) => Value::Bytes(b.clone()),
        Some(AV::ArrayValue(_)) | Some(AV::KvlistValue(_)) => Value::Json(any_to_json(v).to_string()),
    }
}

fn any_to_json(v: &AnyValue) -> serde_json::Value {
    use serde_json::Value as J;
    match &v.value {
        None => J::Null,
        Some(AV::StringValue(s)) => J::String(s.clone()),
        Some(AV::BoolValue(b)) => J::Bool(*b),
        Some(AV::IntValue(i)) => J::from(*i),
        Some(AV::DoubleValue(d)) => serde_json::Number::from_f64(*d).map(J::Number).unwrap_or(J::Null),
        Some(AV::BytesValue(b)) => J::String(wartlog_model::hex(b)),
        Some(AV::ArrayValue(a)) => J::Array(a.values.iter().map(any_to_json).collect()),
        Some(AV::KvlistValue(kv)) => {
            J::Object(kv.values.iter().map(|k| (k.key.clone(), k.value.as_ref().map(any_to_json).unwrap_or(J::Null))).collect())
        }
    }
}

fn kvs(attrs: &[KeyValue]) -> Vec<(String, Value)> {
    attrs.iter().map(|k| (k.key.clone(), k.value.as_ref().map(any_to_value).unwrap_or(Value::Null))).collect()
}

const IDENTIFYING: &[&str] = &["service.namespace", "service.name", "host.name"];

/// Source from resource attributes (identifying ones only) plus the rest as per-record attributes.
pub fn resource_source(r: Option<&Resource>) -> (Arc<Source>, Vec<(String, Value)>) {
    let attrs = r.map(|r| kvs(&r.attributes)).unwrap_or_default();
    let mut id = Vec::new();
    let mut rest = Vec::new();
    for (k, v) in attrs {
        if IDENTIFYING.contains(&k.as_str()) {
            id.push((k, v.to_text()));
        } else {
            rest.push((k, v));
        }
    }
    if !id.iter().any(|(k, _)| k == "service.name") {
        id.push(("service.name".into(), "unknown_service".into()));
    }
    (Arc::new(Source::new(SourceKind::Otlp, id)), rest)
}

fn id16(b: &[u8]) -> Option<[u8; 16]> {
    <[u8; 16]>::try_from(b).ok().filter(|x| x.iter().any(|&c| c != 0))
}
fn id8(b: &[u8]) -> Option<[u8; 8]> {
    <[u8; 8]>::try_from(b).ok().filter(|x| x.iter().any(|&c| c != 0))
}

pub fn logs_to_records(req: &ExportLogsServiceRequest) -> Vec<Record> {
    let now = now_ns();
    let mut out = Vec::new();
    for rl in &req.resource_logs {
        let (src, res_attrs) = resource_source(rl.resource.as_ref());
        for sl in &rl.scope_logs {
            let scope = sl.scope.as_ref().map(|s| s.name.clone()).filter(|n| !n.is_empty());
            for lr in &sl.log_records {
                let mut attrs = kvs(&lr.attributes);
                // .NET's OTLP exporter puts the message template in {OriginalFormat}
                let tmpl = attrs.iter().position(|(k, _)| k == "{OriginalFormat}").map(|i| attrs.remove(i).1.to_text());
                let body_v = lr.body.as_ref().map(any_to_value).unwrap_or(Value::Null);
                let mut args = Vec::new();
                if let Some(t) = &tmpl {
                    for h in template::holes(t) {
                        args.push(attrs.iter().find(|(k, _)| *k == h.name).map(|(_, v)| v.clone()).unwrap_or(Value::Null));
                    }
                }
                let body = if tmpl.is_some() {
                    String::new()
                } else {
                    match &body_v {
                        Value::Str(s) => s.clone(),
                        Value::Null => String::new(),
                        other => other.to_text(),
                    }
                };
                attrs.extend(res_attrs.iter().cloned());
                if let Some(s) = &scope {
                    attrs.push(("otel.scope.name".into(), Value::Str(s.clone())));
                }
                let observed = if lr.observed_time_unix_nano > 0 { lr.observed_time_unix_nano as i64 } else { now };
                let time = if lr.time_unix_nano > 0 { lr.time_unix_nano as i64 } else { observed };
                out.push(Record {
                    seq: 0,
                    time_ns: time,
                    observed_ns: now,
                    severity: lr.severity_number.clamp(0, 24) as u8,
                    severity_text: Some(lr.severity_text.clone()).filter(|s| !s.is_empty()),
                    body,
                    template: tmpl,
                    args,
                    attrs,
                    source: src.clone(),
                    trace_id: id16(&lr.trace_id),
                    span_id: id8(&lr.span_id),
                    span: None,
                });
            }
        }
    }
    out
}

pub fn traces_to_records(req: &ExportTraceServiceRequest) -> Vec<Record> {
    let now = now_ns();
    let mut out = Vec::new();
    for rs in &req.resource_spans {
        let (src, res_attrs) = resource_source(rs.resource.as_ref());
        for ss in &rs.scope_spans {
            for sp in &ss.spans {
                let (status, msg) = sp.status.as_ref().map(|s| (s.code.clamp(0, 2) as u8, s.message.clone())).unwrap_or((0, String::new()));
                let mut attrs = kvs(&sp.attributes);
                attrs.extend(res_attrs.iter().cloned());
                let trace = id16(&sp.trace_id);
                let span_id = id8(&sp.span_id);
                out.push(Record {
                    seq: 0,
                    time_ns: sp.start_time_unix_nano as i64,
                    observed_ns: now,
                    severity: if status == 2 { 17 } else { 9 },
                    severity_text: None,
                    body: sp.name.clone(),
                    template: None,
                    args: vec![],
                    attrs,
                    source: src.clone(),
                    trace_id: trace,
                    span_id,
                    span: Some(SpanData {
                        parent_span_id: id8(&sp.parent_span_id),
                        end_ns: sp.end_time_unix_nano as i64,
                        name: sp.name.clone(),
                        kind: sp.kind.clamp(0, 5) as u8,
                        status,
                        status_message: msg,
                    }),
                });
                // span events become log records linked to the span
                for ev in &sp.events {
                    let mut a = kvs(&ev.attributes);
                    a.push(("wartlog.span_event".into(), Value::Bool(true)));
                    let sev = if ev.name == "exception" { 17 } else { 9 };
                    out.push(Record {
                        seq: 0,
                        time_ns: ev.time_unix_nano as i64,
                        observed_ns: now,
                        severity: sev,
                        severity_text: None,
                        body: ev.name.clone(),
                        template: None,
                        args: vec![],
                        attrs: a,
                        source: src.clone(),
                        trace_id: trace,
                        span_id,
                        span: None,
                    });
                }
            }
        }
    }
    out
}

/// Re-sends what we received to other OTLP collectors, best effort and asynchronously. Each target
/// keeps one lazily connected channel, so a down target costs a failed connect, not a stall.
#[derive(Clone, Default)]
pub struct Forwarder {
    targets: Vec<Target>,
}

#[derive(Clone)]
struct Target {
    endpoint: String,
    channel: tonic::transport::Channel,
    headers: tonic::metadata::MetadataMap,
}

impl Forwarder {
    /// Must be called inside a tokio runtime. Targets with a bad endpoint are skipped with a warning.
    pub fn new(targets: Vec<(String, BTreeMap<String, String>)>) -> Forwarder {
        let mut out = Vec::new();
        for (ep, h) in targets {
            let channel = match tonic::transport::Endpoint::from_shared(ep.clone()) {
                Ok(e) => e.connect_timeout(std::time::Duration::from_secs(2)).timeout(std::time::Duration::from_secs(10)).connect_lazy(),
                Err(e) => {
                    tracing::warn!("ignoring OTLP forward target {ep}: {e}");
                    continue;
                }
            };
            let mut headers = tonic::metadata::MetadataMap::new();
            for (k, v) in &h {
                if let (Ok(k), Ok(v)) = (k.parse::<tonic::metadata::MetadataKey<tonic::metadata::Ascii>>(), v.parse()) {
                    headers.insert(k, v);
                }
            }
            out.push(Target { endpoint: ep, channel, headers });
        }
        Forwarder { targets: out }
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    fn request<T>(t: &Target, msg: T) -> tonic::Request<T> {
        let mut r = tonic::Request::new(msg);
        *r.metadata_mut() = t.headers.clone();
        r
    }

    pub fn logs(&self, req: &ExportLogsServiceRequest) {
        for t in &self.targets {
            let (t, req) = (t.clone(), req.clone());
            tokio::spawn(async move {
                if let Err(e) = LogsServiceClient::new(t.channel.clone()).export(Self::request(&t, req)).await {
                    tracing::debug!("forward logs to {}: {e}", t.endpoint);
                }
            });
        }
    }

    pub fn traces(&self, req: &ExportTraceServiceRequest) {
        for t in &self.targets {
            let (t, req) = (t.clone(), req.clone());
            tokio::spawn(async move {
                if let Err(e) = TraceServiceClient::new(t.channel.clone()).export(Self::request(&t, req)).await {
                    tracing::debug!("forward traces to {}: {e}", t.endpoint);
                }
            });
        }
    }

    pub fn metrics(&self, req: &ExportMetricsServiceRequest) {
        for t in &self.targets {
            let (t, req) = (t.clone(), req.clone());
            tokio::spawn(async move {
                if let Err(e) = MetricsServiceClient::new(t.channel.clone()).export(Self::request(&t, req)).await {
                    tracing::debug!("forward metrics to {}: {e}", t.endpoint);
                }
            });
        }
    }
}

#[derive(Clone)]
struct Svc {
    sink: Sink,
    fwd: Forwarder,
}

fn backpressure(sink: &Sink) -> Result<(), tonic::Status> {
    if sink.is_full() {
        // retryable: OTLP exporters back off and retry on UNAVAILABLE
        Err(tonic::Status::unavailable("WartLog is busy writing; retry"))
    } else {
        Ok(())
    }
}

#[tonic::async_trait]
impl LogsService for Svc {
    async fn export(&self, req: tonic::Request<ExportLogsServiceRequest>) -> Result<tonic::Response<ExportLogsServiceResponse>, tonic::Status> {
        backpressure(&self.sink)?;
        let req = req.into_inner();
        self.fwd.logs(&req);
        self.sink.send(logs_to_records(&req)).await.map_err(|e| tonic::Status::unavailable(e.to_string()))?;
        Ok(tonic::Response::new(ExportLogsServiceResponse { partial_success: None }))
    }
}

#[tonic::async_trait]
impl TraceService for Svc {
    async fn export(&self, req: tonic::Request<ExportTraceServiceRequest>) -> Result<tonic::Response<ExportTraceServiceResponse>, tonic::Status> {
        backpressure(&self.sink)?;
        let req = req.into_inner();
        self.fwd.traces(&req);
        self.sink.send(traces_to_records(&req)).await.map_err(|e| tonic::Status::unavailable(e.to_string()))?;
        Ok(tonic::Response::new(ExportTraceServiceResponse { partial_success: None }))
    }
}

#[tonic::async_trait]
impl MetricsService for Svc {
    async fn export(&self, req: tonic::Request<ExportMetricsServiceRequest>) -> Result<tonic::Response<ExportMetricsServiceResponse>, tonic::Status> {
        // metrics are accepted and dropped (MVP non-goal), but still forwarded
        self.fwd.metrics(&req.into_inner());
        Ok(tonic::Response::new(ExportMetricsServiceResponse { partial_success: None }))
    }
}

pub struct GrpcReceiver {
    pub name: String,
    pub listen: String,
    pub forward: Forwarder,
}

#[async_trait::async_trait]
impl Receiver for GrpcReceiver {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(self: Box<Self>, sink: Sink, shutdown: Shutdown) -> anyhow::Result<()> {
        let addr: std::net::SocketAddr = tokio::net::lookup_host(&self.listen).await?.next().ok_or_else(|| anyhow::anyhow!("cannot resolve {}", self.listen))?;
        crate::listeners::warn_if_public(&self.listen);
        let svc = Svc { sink, fwd: self.forward };
        let incoming = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
            anyhow::anyhow!("cannot listen on {addr} for OTLP/gRPC: {e} (another collector may hold the port; change it in wartlog.toml)")
        })?;
        tracing::info!("OTLP/gRPC receiver listening on {addr}");
        tonic::transport::Server::builder()
            .add_service(LogsServiceServer::new(svc.clone()).accept_compressed(tonic::codec::CompressionEncoding::Gzip))
            .add_service(TraceServiceServer::new(svc.clone()).accept_compressed(tonic::codec::CompressionEncoding::Gzip))
            .add_service(MetricsServiceServer::new(svc).accept_compressed(tonic::codec::CompressionEncoding::Gzip))
            .serve_with_incoming_shutdown(tokio_stream::wrappers::TcpListenerStream::new(incoming), shutdown_signal(shutdown))
            .await?;
        Ok(())
    }
}
