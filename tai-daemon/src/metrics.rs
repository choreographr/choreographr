use prometheus::{
    Encoder, HistogramVec, IntCounter, IntCounterVec, IntGauge,
};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tiny_http::{Method, Response, Server};
use tracing::{error, info};

struct Metrics {
    sessions_active: IntGauge,
    connections_active: IntGauge,
    requests_total: IntCounterVec,
    tool_executions_total: IntCounterVec,
    api_calls_total: IntCounterVec,
    api_errors_total: IntCounterVec,
    connections_total: IntCounter,
    turns_total: IntCounterVec,
    request_duration_seconds: HistogramVec,
    tool_execution_duration_seconds: HistogramVec,
    api_call_duration_seconds: HistogramVec,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

/// Pre-parsed Content-Type header for `/metrics` responses.
/// Parsed once during `init()` to avoid repeating the parse on every request.
static METRICS_CONTENT_TYPE: OnceLock<tiny_http::Header> = OnceLock::new();

/// Register all metrics with the global prometheus registry.
/// Must be called once before any `record_*` function is used.
/// Returns an error if any metric name conflicts with an already-registered metric.
pub fn init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let metrics = Metrics {
        sessions_active: prometheus::register_int_gauge!(
            "tai_sessions_active",
            "Number of active sessions"
        )?,
        connections_active: prometheus::register_int_gauge!(
            "tai_connections_active",
            "Number of active client connections"
        )?,
        requests_total: prometheus::register_int_counter_vec!(
            "tai_requests_total",
            "Total number of requests by status",
            &["status"]
        )?,
        tool_executions_total: prometheus::register_int_counter_vec!(
            "tai_tool_executions_total",
            "Total number of tool executions by tool and status",
            &["tool", "status"]
        )?,
        api_calls_total: prometheus::register_int_counter_vec!(
            "tai_api_calls_total",
            "Total number of API calls by model and endpoint",
            &["model", "endpoint"]
        )?,
        api_errors_total: prometheus::register_int_counter_vec!(
            "tai_api_errors_total",
            "Total number of API errors by model and error type",
            &["model", "error_type"]
        )?,
        connections_total: prometheus::register_int_counter!(
            "tai_connections_total",
            "Total number of connections accepted"
        )?,
        turns_total: prometheus::register_int_counter_vec!(
            "tai_turns_total",
            "Total number of agent loop turns by model",
            &["model"]
        )?,
        request_duration_seconds: prometheus::register_histogram_vec!(
            "tai_request_duration_seconds",
            "Request latency in seconds by status",
            &["status"],
            vec![0.01, 0.05, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0]
        )?,
        tool_execution_duration_seconds: prometheus::register_histogram_vec!(
            "tai_tool_execution_duration_seconds",
            "Tool execution time in seconds by tool",
            &["tool"],
            vec![0.01, 0.05, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0]
        )?,
        api_call_duration_seconds: prometheus::register_histogram_vec!(
            "tai_api_call_duration_seconds",
            "API call round-trip time in seconds by model and endpoint",
            &["model", "endpoint"],
            vec![0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0]
        )?,
    };
    METRICS
        .set(metrics)
        .unwrap_or_else(|_| error!("metrics already initialized — this is a bug"));

    // Pre-parse the content-type header so serve_metrics doesn't need to
    // parse it on every request. The string is hardcoded, so this is
    // guaranteed to succeed.
    let content_type: tiny_http::Header =
        "Content-Type: text/plain; version=0.0.4; charset=utf-8"
            .parse()
            .expect("hardcoded content-type header is valid");
    METRICS_CONTENT_TYPE
        .set(content_type)
        .unwrap_or_else(|_| error!("metrics content-type header already set — this is a bug"));

    Ok(())
}

pub fn record_session_created() {
    if let Some(m) = METRICS.get() {
        m.sessions_active.inc();
    }
}

pub fn record_session_exited() {
    if let Some(m) = METRICS.get() {
        m.sessions_active.dec();
    }
}

pub fn record_client_connected() {
    if let Some(m) = METRICS.get() {
        m.connections_active.inc();
    }
}

pub fn record_client_disconnected() {
    if let Some(m) = METRICS.get() {
        m.connections_active.dec();
    }
}

pub fn record_connection_accepted() {
    if let Some(m) = METRICS.get() {
        m.connections_total.inc();
    }
}

pub fn record_request_total(status: &str) {
    if let Some(m) = METRICS.get() {
        m.requests_total.with_label_values(&[status]).inc();
    }
}

pub fn record_request_duration(status: &str, secs: f64) {
    if let Some(m) = METRICS.get() {
        m.request_duration_seconds
            .with_label_values(&[status])
            .observe(secs);
    }
}

pub fn record_tool_execution(tool: &str, secs: f64, is_error: bool) {
    if let Some(m) = METRICS.get() {
        let status = if is_error { "error" } else { "ok" };
        m.tool_executions_total
            .with_label_values(&[tool, status])
            .inc();
        m.tool_execution_duration_seconds
            .with_label_values(&[tool])
            .observe(secs);
    }
}

pub fn record_turn(model: &str) {
    if let Some(m) = METRICS.get() {
        m.turns_total.with_label_values(&[model]).inc();
    }
}

pub fn record_api_call(model: &str, endpoint: &str, secs: f64) {
    if let Some(m) = METRICS.get() {
        m.api_calls_total
            .with_label_values(&[model, endpoint])
            .inc();
        m.api_call_duration_seconds
            .with_label_values(&[model, endpoint])
            .observe(secs);
    }
}

pub fn record_api_error(model: &str, error_type: &str) {
    if let Some(m) = METRICS.get() {
        m.api_errors_total
            .with_label_values(&[model, error_type])
            .inc();
    }
}

/// HTTP server loop that serves `/metrics` on the given address.
/// Checks an `AtomicBool` shutdown flag every 1-second poll and exits when set.
pub fn serve_metrics(addr: SocketAddr, shutdown: Arc<AtomicBool>) {
    let server = match Server::http(addr) {
        Ok(s) => s,
        Err(e) => {
            error!(%addr, error = %e, "failed to bind metrics HTTP server");
            return;
        }
    };
    info!(%addr, "metrics HTTP server started");

    // The content-type header is pre-parsed during init(). If it's not set,
    // the caller skipped init() — log an error but keep serving.
    let content_type = match METRICS_CONTENT_TYPE.get() {
        Some(h) => h,
        None => {
            error!("metrics not initialized — call metrics::init() before serve_metrics()");
            return;
        }
    };

    loop {
        if shutdown.load(Ordering::SeqCst) {
            info!("metrics HTTP server shutting down");
            break;
        }

        match server.recv_timeout(Duration::from_secs(1)) {
            Ok(Some(request)) => {
                if request.method() == &Method::Get && request.url() == "/metrics" {
                    let metric_families = prometheus::gather();
                    let encoder = prometheus::TextEncoder::new();
                    let mut buffer = Vec::new();
                    if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
                        error!(error = %e, "failed to encode metrics");
                        let _ = request
                            .respond(Response::from_string("internal error").with_status_code(500));
                        continue;
                    }
                    let response =
                        Response::from_data(buffer).with_header(content_type.clone());
                    let _ = request.respond(response);
                } else {
                    let _ = request
                        .respond(Response::from_string("not found").with_status_code(404));
                }
            }
            Ok(None) => {
                // Timeout — loop back and check shutdown flag
                continue;
            }
            Err(e) => {
                error!(error = %e, "metrics HTTP server error");
                continue;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::TextEncoder;
    use serial_test::serial;

    /// Initialize the metrics singleton exactly once for all unit tests.
    /// Safe to call multiple times — subsequent calls are no-ops.
    fn ensure_init() {
        if METRICS.get().is_none() {
            init().unwrap();
        }
    }

    /// All unit tests in this module share the global prometheus registry
    /// and the singleton `Metrics` struct.  The `#[serial(metrics)]` attribute
    /// ensures they never run concurrently, which prevents interference.
    #[serial(metrics)]
    #[test]
    fn test_session_created_increments_gauge() {
        ensure_init();
        let m = METRICS.get().unwrap();
        let before = m.sessions_active.get();
        record_session_created();
        assert_eq!(m.sessions_active.get(), before + 1);
    }

    #[serial(metrics)]
    #[test]
    fn test_session_created_and_exited_balance() {
        ensure_init();
        let m = METRICS.get().unwrap();
        let before = m.sessions_active.get();
        record_session_created();
        record_session_exited();
        assert_eq!(m.sessions_active.get(), before);
    }

    #[serial(metrics)]
    #[test]
    fn test_tool_execution_ok_increments_counter() {
        ensure_init();
        let m = METRICS.get().unwrap();
        let before_ok = m
            .tool_executions_total
            .with_label_values(&["my_tool", "ok"])
            .get();
        record_tool_execution("my_tool", 0.5, false);
        assert_eq!(
            m.tool_executions_total
                .with_label_values(&["my_tool", "ok"])
                .get(),
            before_ok + 1
        );
    }

    #[serial(metrics)]
    #[test]
    fn test_tool_execution_error_increments_error_counter() {
        ensure_init();
        let m = METRICS.get().unwrap();
        let before_err = m
            .tool_executions_total
            .with_label_values(&["my_tool", "error"])
            .get();
        record_tool_execution("my_tool", 0.5, true);
        assert_eq!(
            m.tool_executions_total
                .with_label_values(&["my_tool", "error"])
                .get(),
            before_err + 1
        );
    }

    #[serial(metrics)]
    #[test]
    fn test_metrics_output_contains_help_and_type_lines() {
        ensure_init();
        // Call each metric function at least once to seed label values.
        // CounterVec/HistogramVec families only appear in gather() output
        // after a with_label_values() call has created a child metric.
        record_session_created();
        record_request_total("done");
        record_request_duration("done", 0.5);
        record_tool_execution("test_tool", 0.5, false);
        record_api_call("test_model", "chat/completions", 0.5);
        record_api_error("test_model", "other");
        record_turn("test_model");
        record_client_connected();
        record_connection_accepted();
        // Gather and encode all metrics via the text encoder, verify
        // that the output contains expected HELP/TYPE lines.
        let metric_families = prometheus::gather();
        let encoder = TextEncoder::new();
        let mut buffer = Vec::new();
        encoder.encode(&metric_families, &mut buffer).unwrap();
        let output = String::from_utf8(buffer).unwrap();

        assert!(output.contains("# HELP tai_sessions_active"));
        assert!(output.contains("# TYPE tai_sessions_active gauge"));
        assert!(output.contains("# HELP tai_requests_total"));
        assert!(output.contains("# TYPE tai_requests_total counter"));
        assert!(output.contains("# HELP tai_request_duration_seconds"));
        assert!(output.contains("# TYPE tai_request_duration_seconds histogram"));
    }

}
