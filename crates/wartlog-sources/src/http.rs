//! HTTP receiver: OTLP/HTTP (`/v1/logs`, `/v1/traces`, `/v1/metrics`, protobuf or JSON) and
//! NDJSON (`POST /ingest`, optional `?source=name`) on one listener.

#![allow(clippy::result_large_err)] // handlers return axum Responses as errors by design

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use std::collections::HashMap;
use std::io::Read;
use wartlog_model::time::now_ns;

use crate::{ndjson, shutdown_signal, Receiver, Shutdown, Sink};

#[derive(Clone)]
struct AppState {
    sink: Sink,
    default_source: String,
    #[cfg(feature = "otlp")]
    fwd: crate::otlp::Forwarder,
}

pub struct HttpReceiver {
    pub name: String,
    pub listen: String,
    /// serve /v1/* (OTLP/HTTP)
    pub otlp: bool,
    /// serve /ingest (NDJSON)
    pub ndjson: bool,
    pub default_source: String,
    #[cfg(feature = "otlp")]
    pub forward: crate::otlp::Forwarder,
}

/// Request bodies above this are refused.
const MAX_BODY: usize = 64 << 20;

fn body_bytes(headers: &HeaderMap, body: Bytes) -> Result<Vec<u8>, Response> {
    let gz = headers.get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
    if !gz {
        return Ok(body.to_vec());
    }
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&body[..]).take(MAX_BODY as u64 + 1).read_to_end(&mut out).map_err(|e| (StatusCode::BAD_REQUEST, format!("bad gzip body: {e}")).into_response())?;
    if out.len() > MAX_BODY {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response());
    }
    Ok(out)
}

fn busy() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "1")], "WartLog is busy writing; retry").into_response()
}

async fn ingest(State(st): State<AppState>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap, body: Bytes) -> Response {
    if st.sink.is_full() {
        return busy();
    }
    let b = match body_bytes(&headers, body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let src = q.get("source").cloned().unwrap_or_else(|| st.default_source.clone());
    let now = now_ns();
    let recs: Vec<_> = String::from_utf8_lossy(&b).lines().filter(|l| !l.trim().is_empty()).map(|l| ndjson::normalize(l, &src, now)).collect();
    let n = recs.len();
    match st.sink.send(recs).await {
        Ok(()) => (StatusCode::OK, axum::Json(serde_json::json!({ "accepted": n }))).into_response(),
        Err(_) => busy(),
    }
}

#[cfg(feature = "otlp")]
mod otlp_http {
    use super::*;
    use opentelemetry_proto::tonic::collector::logs::v1::{ExportLogsServiceRequest, ExportLogsServiceResponse};
    use opentelemetry_proto::tonic::collector::metrics::v1::{ExportMetricsServiceRequest, ExportMetricsServiceResponse};
    use opentelemetry_proto::tonic::collector::trace::v1::{ExportTraceServiceRequest, ExportTraceServiceResponse};
    use prost::Message;

    fn is_json(h: &HeaderMap) -> bool {
        h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/json"))
    }

    fn decode<T: Message + Default + serde::de::DeserializeOwned>(h: &HeaderMap, b: &[u8]) -> Result<T, Response> {
        if is_json(h) {
            serde_json::from_slice(b).map_err(|e| (StatusCode::BAD_REQUEST, format!("bad OTLP JSON: {e}")).into_response())
        } else {
            T::decode(b).map_err(|e| (StatusCode::BAD_REQUEST, format!("bad OTLP protobuf: {e}")).into_response())
        }
    }

    fn reply<T: Message + serde::Serialize>(h: &HeaderMap, m: T) -> Response {
        if is_json(h) {
            (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], serde_json::to_vec(&m).unwrap_or_else(|_| b"{}".to_vec())).into_response()
        } else {
            (StatusCode::OK, [(header::CONTENT_TYPE, "application/x-protobuf")], m.encode_to_vec()).into_response()
        }
    }

    pub async fn logs(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
        if st.sink.is_full() {
            return busy();
        }
        let b = match body_bytes(&headers, body) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let req: ExportLogsServiceRequest = match decode(&headers, &b) {
            Ok(r) => r,
            Err(r) => return r,
        };
        st.fwd.logs(&req);
        if st.sink.send(crate::otlp::logs_to_records(&req)).await.is_err() {
            return busy();
        }
        reply(&headers, ExportLogsServiceResponse { partial_success: None })
    }

    pub async fn traces(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
        if st.sink.is_full() {
            return busy();
        }
        let b = match body_bytes(&headers, body) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let req: ExportTraceServiceRequest = match decode(&headers, &b) {
            Ok(r) => r,
            Err(r) => return r,
        };
        st.fwd.traces(&req);
        if st.sink.send(crate::otlp::traces_to_records(&req)).await.is_err() {
            return busy();
        }
        reply(&headers, ExportTraceServiceResponse { partial_success: None })
    }

    pub async fn metrics(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
        let b = match body_bytes(&headers, body) {
            Ok(b) => b,
            Err(r) => return r,
        };
        if let Ok(req) = decode::<ExportMetricsServiceRequest>(&headers, &b) {
            st.fwd.metrics(&req);
        }
        reply(&headers, ExportMetricsServiceResponse { partial_success: None })
    }
}

pub fn router(r: &HttpReceiver, sink: Sink) -> Router {
    let st = AppState {
        sink,
        default_source: r.default_source.clone(),
        #[cfg(feature = "otlp")]
        fwd: r.forward.clone(),
    };
    let mut app = Router::new().route("/health", get(|| async { "ok" }));
    if r.ndjson {
        app = app.route("/ingest", post(ingest));
    }
    #[cfg(feature = "otlp")]
    if r.otlp {
        app = app.route("/v1/logs", post(otlp_http::logs)).route("/v1/traces", post(otlp_http::traces)).route("/v1/metrics", post(otlp_http::metrics));
    }
    app.layer(axum::extract::DefaultBodyLimit::max(MAX_BODY)).with_state(st)
}

#[async_trait::async_trait]
impl Receiver for HttpReceiver {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(self: Box<Self>, sink: Sink, shutdown: Shutdown) -> anyhow::Result<()> {
        let l = tokio::net::TcpListener::bind(&self.listen)
            .await
            .map_err(|e| anyhow::anyhow!("cannot listen on {} for HTTP: {e} (another collector may hold the port; change it in wartlog.toml)", self.listen))?;
        crate::listeners::warn_if_public(&self.listen);
        tracing::info!("HTTP receiver listening on {}", self.listen);
        let app = router(&self, sink);
        axum::serve(l, app).with_graceful_shutdown(shutdown_signal(shutdown)).await?;
        Ok(())
    }
}
