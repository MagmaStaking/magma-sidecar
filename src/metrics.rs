//! Prometheus metrics + structured `/health` state for the sidecar.
//!
//! All counters/gauges live on a single `Metrics` struct cloned via `Arc` into the
//! IPC loop and the HTTP state. The `/metrics` endpoint serializes the registry in
//! Prometheus text exposition format; `/health` returns a JSON snapshot suitable
//! for human + load-balancer consumption.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use prometheus::{
    register_int_counter_vec_with_registry, register_int_counter_with_registry,
    register_int_gauge_with_registry, Encoder, IntCounter, IntCounterVec, IntGauge, Registry,
    TextEncoder,
};
use serde::Serialize;

/// Connection state for the txpool IPC loop, surfaced in `/health` and as a gauge.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum IpcState {
    /// IPC was not configured (`--txpool-socket` absent).
    Disabled,
    /// Trying to (re)connect.
    Connecting,
    /// Currently connected.
    Connected,
}

impl IpcState {
    fn as_str(&self) -> &'static str {
        match self {
            IpcState::Disabled => "disabled",
            IpcState::Connecting => "connecting",
            IpcState::Connected => "connected",
        }
    }
    fn gauge(&self) -> i64 {
        match self {
            IpcState::Disabled => -1,
            IpcState::Connecting => 0,
            IpcState::Connected => 1,
        }
    }
}

#[derive(Debug)]
pub struct Metrics {
    pub registry: Registry,

    // ---- Txpool IPC ----
    pub txpool_events_total: IntCounterVec, // labels: kind = insert|commit|drop|evict
    pub txpool_inserts_total: IntCounter,
    pub txpool_prioritized_total: IntCounter,
    pub txpool_send_failures_total: IntCounter,
    pub txpool_ipc_reconnects_total: IntCounter,
    pub txpool_ipc_state: IntGauge,
    pub last_event_unix_seconds: IntGauge,
    pub last_send_unix_seconds: IntGauge,

    // ---- HTTP ----
    pub http_requests_total: IntCounterVec, // labels: route, status_class

    // Mirror of last-seen timestamps usable from /health without scraping the registry.
    last_event_ts: AtomicI64,
    last_send_ts: AtomicI64,
    ipc_state: AtomicI64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        let registry = Registry::new_custom(Some("magma_sidecar".into()), None)
            .expect("custom registry creation");

        let txpool_events_total = register_int_counter_vec_with_registry!(
            "txpool_events_total",
            "Number of txpool events received from the node, by kind.",
            &["kind"],
            registry
        )
        .expect("register txpool_events_total");

        let txpool_inserts_total = register_int_counter_with_registry!(
            "txpool_inserts_total",
            "Number of Insert events observed (deduped).",
            registry
        )
        .expect("register txpool_inserts_total");

        let txpool_prioritized_total = register_int_counter_with_registry!(
            "txpool_prioritized_total",
            "Number of transactions re-injected with a sidecar-computed priority.",
            registry
        )
        .expect("register txpool_prioritized_total");

        let txpool_send_failures_total = register_int_counter_with_registry!(
            "txpool_send_failures_total",
            "IPC send failures (each one triggers a reconnect).",
            registry
        )
        .expect("register txpool_send_failures_total");

        let txpool_ipc_reconnects_total = register_int_counter_with_registry!(
            "txpool_ipc_reconnects_total",
            "Number of times the IPC client (re)connected to the node socket.",
            registry
        )
        .expect("register txpool_ipc_reconnects_total");

        let txpool_ipc_state = register_int_gauge_with_registry!(
            "txpool_ipc_state",
            "Txpool IPC state: -1=disabled, 0=connecting, 1=connected.",
            registry
        )
        .expect("register txpool_ipc_state");

        let last_event_unix_seconds = register_int_gauge_with_registry!(
            "last_event_unix_seconds",
            "Unix timestamp of the last txpool event received (0 if none).",
            registry
        )
        .expect("register last_event_unix_seconds");

        let last_send_unix_seconds = register_int_gauge_with_registry!(
            "last_send_unix_seconds",
            "Unix timestamp of the last reinjected tx (0 if none).",
            registry
        )
        .expect("register last_send_unix_seconds");

        let http_requests_total = register_int_counter_vec_with_registry!(
            "http_requests_total",
            "HTTP requests served, by route and status class.",
            &["route", "status_class"],
            registry
        )
        .expect("register http_requests_total");

        let m = Self {
            registry,
            txpool_events_total,
            txpool_inserts_total,
            txpool_prioritized_total,
            txpool_send_failures_total,
            txpool_ipc_reconnects_total,
            txpool_ipc_state,
            last_event_unix_seconds,
            last_send_unix_seconds,
            http_requests_total,
            last_event_ts: AtomicI64::new(0),
            last_send_ts: AtomicI64::new(0),
            ipc_state: AtomicI64::new(IpcState::Disabled.gauge()),
        };
        m.set_ipc_state(IpcState::Disabled);
        Arc::new(m)
    }

    pub fn set_ipc_state(&self, state: IpcState) {
        self.txpool_ipc_state.set(state.gauge());
        self.ipc_state.store(state.gauge(), Ordering::Relaxed);
    }

    pub fn record_event(&self, kind: &'static str) {
        self.txpool_events_total.with_label_values(&[kind]).inc();
        let now = unix_now();
        self.last_event_unix_seconds.set(now);
        self.last_event_ts.store(now, Ordering::Relaxed);
    }

    pub fn record_insert(&self) {
        self.txpool_inserts_total.inc();
    }

    pub fn record_prioritized(&self) {
        self.txpool_prioritized_total.inc();
        let now = unix_now();
        self.last_send_unix_seconds.set(now);
        self.last_send_ts.store(now, Ordering::Relaxed);
    }

    pub fn record_send_failure(&self) {
        self.txpool_send_failures_total.inc();
    }

    pub fn record_reconnect(&self) {
        self.txpool_ipc_reconnects_total.inc();
    }

    pub fn record_http(&self, route: &str, status: u16) {
        let class = match status {
            100..=199 => "1xx",
            200..=299 => "2xx",
            300..=399 => "3xx",
            400..=499 => "4xx",
            _ => "5xx",
        };
        self.http_requests_total
            .with_label_values(&[route, class])
            .inc();
    }

    pub fn render_prometheus(&self) -> Result<String, prometheus::Error> {
        let metric_families = self.registry.gather();
        let mut buf = Vec::with_capacity(2048);
        TextEncoder::new().encode(&metric_families, &mut buf)?;
        String::from_utf8(buf).map_err(|e| prometheus::Error::Msg(e.to_string()))
    }

    pub fn snapshot(&self) -> HealthSnapshot {
        HealthSnapshot {
            status: "ok",
            service: "magma-sidecar",
            ipc_state: ipc_state_from_gauge(self.ipc_state.load(Ordering::Relaxed)).as_str(),
            tx_inserts_observed: self.txpool_inserts_total.get(),
            tx_prioritized: self.txpool_prioritized_total.get(),
            ipc_send_failures: self.txpool_send_failures_total.get(),
            ipc_reconnects: self.txpool_ipc_reconnects_total.get(),
            last_event_unix_seconds: nonzero(self.last_event_ts.load(Ordering::Relaxed)),
            last_send_unix_seconds: nonzero(self.last_send_ts.load(Ordering::Relaxed)),
        }
    }
}

fn ipc_state_from_gauge(g: i64) -> IpcState {
    match g {
        1 => IpcState::Connected,
        0 => IpcState::Connecting,
        _ => IpcState::Disabled,
    }
}

fn nonzero(t: i64) -> Option<i64> {
    if t == 0 {
        None
    } else {
        Some(t)
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Serialize)]
pub struct HealthSnapshot {
    pub status: &'static str,
    pub service: &'static str,
    pub ipc_state: &'static str,
    pub tx_inserts_observed: u64,
    pub tx_prioritized: u64,
    pub ipc_send_failures: u64,
    pub ipc_reconnects: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_unix_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_send_unix_seconds: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trip() {
        let m = Metrics::new();
        m.set_ipc_state(IpcState::Connected);
        m.record_event("insert");
        m.record_insert();
        m.record_prioritized();
        let snap = m.snapshot();
        assert_eq!(snap.ipc_state, "connected");
        assert_eq!(snap.tx_inserts_observed, 1);
        assert_eq!(snap.tx_prioritized, 1);
        assert!(snap.last_event_unix_seconds.is_some());
        assert!(snap.last_send_unix_seconds.is_some());
    }

    #[test]
    fn render_prometheus_includes_namespace() {
        let m = Metrics::new();
        m.record_event("commit");
        m.record_http("/health", 200);
        let txt = m.render_prometheus().expect("render");
        assert!(txt.contains("magma_sidecar_txpool_events_total"));
        assert!(txt.contains(r#"kind="commit""#));
        assert!(txt.contains("magma_sidecar_http_requests_total"));
        assert!(txt.contains(r#"route="/health""#));
        assert!(txt.contains(r#"status_class="2xx""#));
    }

    #[test]
    fn ipc_state_gauge_values() {
        let m = Metrics::new();
        assert_eq!(m.txpool_ipc_state.get(), -1);
        m.set_ipc_state(IpcState::Connecting);
        assert_eq!(m.txpool_ipc_state.get(), 0);
        m.set_ipc_state(IpcState::Connected);
        assert_eq!(m.txpool_ipc_state.get(), 1);
    }
}
