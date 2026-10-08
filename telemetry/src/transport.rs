use crate::{
    budget::Budget,
    config::{ConfigError, Settings},
};
use async_trait::async_trait;
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use std::{
    fmt,
    io::Read,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Default, Debug)]
pub(crate) struct ExportStats {
    pub attempts: AtomicU64,
    pub bytes: AtomicU64,
    pub failures: AtomicU64,
    pub budget_drops: AtomicU64,
    pub sampled_out: AtomicU64,
    warning_at: AtomicU64,
}

pub(crate) struct Transport {
    client: reqwest::blocking::Client,
    endpoint: String,
    key: http::HeaderValue,
    budget: Arc<Budget>,
    pub stats: Arc<ExportStats>,
}
impl fmt::Debug for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BoundedTelemetryTransport(<redacted>)")
    }
}
impl Transport {
    pub fn new(settings: &Settings, stats: Arc<ExportStats>) -> Result<Self, ConfigError> {
        let budget = Budget::open(&settings.budget, settings.daily_bytes)
            .map_err(|_| ConfigError::BudgetUnavailable)?;
        let mut key = http::HeaderValue::from_str(&settings.key)
            .map_err(|_| ConfigError::InvalidLicenseKey)?;
        key.set_sensitive(true);
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(1))
            .timeout(Duration::from_secs(2))
            .pool_max_idle_per_host(2)
            .build()
            .map_err(|_| ConfigError::ExporterUnavailable)?;
        Ok(Self {
            client,
            endpoint: settings.endpoint.clone(),
            key,
            budget: Arc::new(budget),
            stats,
        })
    }
    fn warning(&self, reason: &'static str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let previous = self.stats.warning_at.load(Ordering::Relaxed);
        if now.saturating_sub(previous) >= 60
            && self
                .stats
                .warning_at
                .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::warn!(
                reason,
                "Optional telemetry export dropped; playback is unaffected"
            );
        }
    }
}

fn safe_error(reason: &'static str) -> HttpError {
    Box::new(std::io::Error::other(reason))
}

#[async_trait]
impl HttpClient for Transport {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let uri = request.uri().to_string();
        let allowed = ["traces", "metrics", "logs"]
            .iter()
            .any(|signal| uri == format!("{}/v1/{signal}", self.endpoint));
        if !allowed || request.method() != http::Method::POST || request.body().len() > 256 * 1024 {
            return Err(safe_error("telemetry request rejected"));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| safe_error("telemetry clock unavailable"))?
            .as_secs();
        if !self
            .budget
            .reserve(now / 86400, request.body().len() as u64)
        {
            self.stats.budget_drops.fetch_add(1, Ordering::Relaxed);
            self.warning("budget_or_ledger");
            return Err(safe_error("telemetry daily budget unavailable"));
        }
        self.stats.attempts.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes
            .fetch_add(request.body().len() as u64, Ordering::Relaxed);
        // Only our explicit authentication and protobuf headers are forwarded.
        // OTEL_*_HEADERS and ambient HTTP proxy settings cannot redirect secrets.
        let result = self
            .client
            .post(uri)
            .header("api-key", self.key.clone())
            .header(http::header::CONTENT_TYPE, "application/x-protobuf")
            .body(request.into_body())
            .send();
        let mut response = match result {
            Ok(response) => response,
            Err(_) => {
                self.stats.failures.fetch_add(1, Ordering::Relaxed);
                self.warning("collector_transport");
                return Err(safe_error("telemetry collector unavailable"));
            }
        };
        let status = response.status();
        if !status.is_success() {
            self.stats.failures.fetch_add(1, Ordering::Relaxed);
            self.warning("collector_http");
            return Ok(Response::builder().status(status).body(Bytes::new())?);
        }
        let mut data = Vec::new();
        if (&mut response).take(65537).read_to_end(&mut data).is_err() || data.len() > 65536 {
            self.stats.failures.fetch_add(1, Ordering::Relaxed);
            self.warning("collector_response");
            return Err(safe_error("telemetry response invalid"));
        }
        Ok(Response::builder().status(status).body(Bytes::from(data))?)
    }
}
