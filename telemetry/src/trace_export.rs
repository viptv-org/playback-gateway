use crate::transport::ExportStats;
use opentelemetry::trace::Status;
use opentelemetry_sdk::{
    error::OTelSdkResult,
    trace::{SpanData, SpanExporter},
    Resource,
};
use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

#[derive(Debug)]
pub(crate) struct SampledExporter<E> {
    inner: E,
    rate: f64,
    stats: Arc<ExportStats>,
}
impl<E> SampledExporter<E> {
    pub fn new(inner: E, rate: f64, stats: Arc<ExportStats>) -> Self {
        Self { inner, rate, stats }
    }
}
impl<E: SpanExporter> SpanExporter for SampledExporter<E> {
    async fn export(&self, mut spans: Vec<SpanData>) -> OTelSdkResult {
        let count = spans.len();
        spans.retain(|span| {
            if matches!(span.status, Status::Error { .. }) {
                return true;
            }
            let bytes = span.span_context.trace_id().to_bytes();
            let value = u64::from_be_bytes(bytes[..8].try_into().expect("trace ID width"));
            self.rate == 1.0 || (self.rate > 0.0 && (value as f64 / u64::MAX as f64) < self.rate)
        });
        self.stats
            .sampled_out
            .fetch_add((count - spans.len()) as u64, Ordering::Relaxed);
        if spans.is_empty() {
            return Ok(());
        }
        self.inner.export(spans).await
    }
    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }
    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }
}
