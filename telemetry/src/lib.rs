//! Opt-in, bounded service telemetry. Application data is never an export field.
mod budget;
mod config;
pub mod middleware;
mod operation;
mod trace_export;
mod transport;
pub use config::ConfigError;
pub use operation::{Failure, Operation, Outcome};

use opentelemetry::{
    logs::{AnyValue, LogRecord, Logger, LoggerProvider, Severity},
    metrics::{Counter, Histogram, MeterProvider},
    trace::{SpanKind, Status, TraceContextExt, Tracer, TracerProvider},
    Context, KeyValue,
};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::{
    logs::{BatchLogProcessor, SdkLogger, SdkLoggerProvider},
    metrics::{PeriodicReader, SdkMeterProvider},
    trace::{BatchConfigBuilder, BatchSpanProcessor, SdkTracer, SdkTracerProvider},
    Resource,
};
use std::{
    future::Future,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, Copy)]
pub enum Service {
    Api,
    Gateway,
}
impl Service {
    fn name(self) -> &'static str {
        match self {
            Self::Api => "viptv-api",
            Self::Gateway => "playback-gateway",
        }
    }
}

#[derive(Clone, Default)]
pub struct Telemetry {
    inner: Option<Arc<Inner>>,
}
struct Inner {
    traces: SdkTracerProvider,
    metrics: SdkMeterProvider,
    logs: SdkLoggerProvider,
    tracer: SdkTracer,
    logger: SdkLogger,
    requests: Counter<u64>,
    durations: Histogram<f64>,
    active: Arc<AtomicU64>,
    stopped: AtomicBool,
}

#[derive(Clone)]
pub struct RequestContext {
    telemetry: Telemetry,
    context: Context,
}
tokio::task_local! { static CURRENT: RequestContext; }

impl Telemetry {
    pub async fn initialize(service: Service, budget_path: PathBuf) -> Result<Self, ConfigError> {
        let Some(settings) = config::Settings::read(|name| std::env::var(name).ok(), budget_path)?
        else {
            return Ok(Self::default());
        };
        tokio::task::spawn_blocking(move || Self::build(service, settings))
            .await
            .map_err(|_| ConfigError::ExporterUnavailable)?
    }
    fn build(service: Service, settings: config::Settings) -> Result<Self, ConfigError> {
        let stats = Arc::new(transport::ExportStats::default());
        let transport: Arc<dyn opentelemetry_http::HttpClient> =
            Arc::new(transport::Transport::new(&settings, stats.clone())?);
        let resource = Resource::builder_empty()
            .with_service_name(service.name())
            .with_attributes([KeyValue::new("service.version", env!("CARGO_PKG_VERSION"))])
            .build();
        let trace_export = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(format!("{}/v1/traces", settings.endpoint))
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_timeout(Duration::from_secs(2))
            .with_retry_policy(opentelemetry_otlp::RetryPolicy::disabled())
            .with_max_request_body_size(256 * 1024)
            .with_shared_http_client(transport.clone())
            .build()
            .map_err(|_| ConfigError::ExporterUnavailable)?;
        let processor = BatchSpanProcessor::builder(trace_export::SampledExporter::new(
            trace_export,
            settings.sample_rate,
            stats.clone(),
        ))
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_queue_size(1024)
                .with_max_export_batch_size(128)
                .with_scheduled_delay(Duration::from_secs(5))
                .build(),
        )
        .build();
        let traces = SdkTracerProvider::builder()
            .with_resource(resource.clone())
            .with_sampler(opentelemetry_sdk::trace::Sampler::AlwaysOn)
            .with_max_attributes_per_span(16)
            .with_max_events_per_span(4)
            .with_max_links_per_span(0)
            .with_span_processor(processor)
            .build();
        let metric_export = opentelemetry_otlp::MetricExporter::builder()
            .with_temporality(opentelemetry_sdk::metrics::Temporality::Delta)
            .with_http()
            .with_endpoint(format!("{}/v1/metrics", settings.endpoint))
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_timeout(Duration::from_secs(2))
            .with_retry_policy(opentelemetry_otlp::RetryPolicy::disabled())
            .with_max_request_body_size(256 * 1024)
            .with_shared_http_client(transport.clone())
            .build()
            .map_err(|_| ConfigError::ExporterUnavailable)?;
        let reader = PeriodicReader::builder(metric_export)
            .with_interval(settings.interval)
            .build();
        let metrics = SdkMeterProvider::builder()
            .with_resource(resource.clone())
            .with_reader(reader)
            .with_view(|instrument: &opentelemetry_sdk::metrics::Instrument| {
                let stream =
                    opentelemetry_sdk::metrics::Stream::builder().with_cardinality_limit(256);
                let stream = if instrument.name() == "service.operation.duration" {
                    stream.with_aggregation(
                        opentelemetry_sdk::metrics::Aggregation::Base2ExponentialHistogram {
                            max_size: 160,
                            max_scale: 20,
                            record_min_max: true,
                        },
                    )
                } else {
                    stream
                };
                stream.build().ok()
            })
            .build();
        let log_export = opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_endpoint(format!("{}/v1/logs", settings.endpoint))
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_timeout(Duration::from_secs(2))
            .with_retry_policy(opentelemetry_otlp::RetryPolicy::disabled())
            .with_max_request_body_size(256 * 1024)
            .with_shared_http_client(transport)
            .build()
            .map_err(|_| ConfigError::ExporterUnavailable)?;
        let processor = BatchLogProcessor::builder(log_export)
            .with_batch_config(
                opentelemetry_sdk::logs::BatchConfigBuilder::default()
                    .with_max_queue_size(512)
                    .with_max_export_batch_size(64)
                    .with_scheduled_delay(Duration::from_secs(5))
                    .build(),
            )
            .build();
        let logs = SdkLoggerProvider::builder()
            .with_resource(resource)
            .with_log_processor(processor)
            .build();
        let meter = metrics.meter("service-telemetry");
        let requests = meter
            .u64_counter("service.operations")
            .with_description("All measured operations, including failures and cancellations")
            .build();
        let durations = meter
            .f64_histogram("service.operation.duration")
            .with_unit("s")
            .build();
        let active = Arc::new(AtomicU64::new(0));
        let active_count = active.clone();
        meter
            .u64_observable_gauge("service.operations.active")
            .with_callback(move |observer| {
                observer.observe(active_count.load(Ordering::Relaxed), &[]);
            })
            .build();
        let export_stats = stats.clone();
        meter
            .u64_observable_gauge("telemetry.export.attempts")
            .with_callback(move |observer| {
                observer.observe(export_stats.attempts.load(Ordering::Relaxed), &[]);
            })
            .build();
        let export_stats = stats.clone();
        meter
            .u64_observable_gauge("telemetry.export.bytes")
            .with_unit("By")
            .with_callback(move |observer| {
                observer.observe(export_stats.bytes.load(Ordering::Relaxed), &[]);
            })
            .build();
        let export_stats = stats.clone();
        meter
            .u64_observable_gauge("telemetry.export.failures")
            .with_callback(move |observer| {
                observer.observe(export_stats.failures.load(Ordering::Relaxed), &[]);
            })
            .build();
        let export_stats = stats.clone();
        meter
            .u64_observable_gauge("telemetry.export.budget_drops")
            .with_callback(move |observer| {
                observer.observe(export_stats.budget_drops.load(Ordering::Relaxed), &[]);
            })
            .build();
        meter
            .u64_observable_gauge("telemetry.trace.sampled_out")
            .with_callback(move |observer| {
                observer.observe(stats.sampled_out.load(Ordering::Relaxed), &[]);
            })
            .build();
        #[cfg(target_os = "linux")]
        meter
            .u64_observable_gauge("process.memory.resident")
            .with_unit("By")
            .with_callback(|observer| {
                if let Ok(text) = std::fs::read_to_string("/proc/self/status") {
                    if let Some(bytes) = text.lines().find_map(|line| {
                        line.strip_prefix("VmRSS:")?
                            .split_whitespace()
                            .next()?
                            .parse::<u64>()
                            .ok()
                    }) {
                        observer.observe(bytes.saturating_mul(1024), &[]);
                    }
                }
            })
            .build();
        #[cfg(unix)]
        meter
            .f64_observable_gauge("process.cpu.time")
            .with_unit("s")
            .with_callback(|observer| {
                let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
                // getrusage initializes the structure on success; no process enumeration.
                if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } == 0 {
                    let usage = unsafe { usage.assume_init() };
                    let seconds = usage.ru_utime.tv_sec as f64
                        + usage.ru_stime.tv_sec as f64
                        + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1_000_000.0;
                    observer.observe(seconds, &[]);
                }
            })
            .build();
        let tracer = traces.tracer("service-telemetry");
        let logger = logs.logger("service-telemetry");
        Ok(Self {
            inner: Some(Arc::new(Inner {
                traces,
                metrics,
                logs,
                tracer,
                logger,
                requests,
                durations,
                active,
                stopped: AtomicBool::new(false),
            })),
        })
    }
    pub fn enabled(&self) -> bool {
        self.inner.is_some()
    }
    pub fn background_context(&self) -> RequestContext {
        RequestContext {
            telemetry: self.clone(),
            context: Context::new(),
        }
    }
    pub(crate) fn start(
        &self,
        operation: Operation,
        parent: Context,
        kind: SpanKind,
    ) -> Observation {
        let Some(inner) = &self.inner else {
            return Observation { record: None };
        };
        if inner.stopped.load(Ordering::Relaxed) {
            return Observation { record: None };
        }
        let span = inner
            .tracer
            .span_builder(operation.name())
            .with_kind(kind)
            .with_attributes([KeyValue::new("operation.name", operation.name())])
            .start_with_context(&inner.tracer, &parent);
        inner.active.fetch_add(1, Ordering::Relaxed);
        Observation {
            record: Some(Record {
                telemetry: self.clone(),
                context: Context::new().with_span(span),
                operation,
                started: Instant::now(),
                status: None,
                bytes: 0,
                method: "INTERNAL",
            }),
        }
    }
    pub async fn shutdown(self) {
        let Some(inner) = self.inner else {
            return;
        };
        if inner.stopped.swap(true, Ordering::Relaxed) {
            return;
        }
        let task = tokio::task::spawn_blocking(move || {
            let failed = inner
                .traces
                .shutdown_with_timeout(Duration::from_secs(2))
                .is_err()
                | inner
                    .logs
                    .shutdown_with_timeout(Duration::from_secs(2))
                    .is_err()
                | inner
                    .metrics
                    .shutdown_with_timeout(Duration::from_secs(2))
                    .is_err();
            if failed {
                tracing::warn!("Optional telemetry shutdown dropped pending data");
            }
        });
        if tokio::time::timeout(Duration::from_secs(7), task)
            .await
            .is_err()
        {
            tracing::warn!("Optional telemetry shutdown deadline reached");
        }
    }
}

pub struct Observation {
    record: Option<Record>,
}
struct Record {
    telemetry: Telemetry,
    context: Context,
    operation: Operation,
    started: Instant,
    status: Option<u16>,
    bytes: u64,
    method: &'static str,
}
impl Observation {
    pub fn processing(&self, video: &str, audio: &str) {
        let mode = |value: &str| match value {
            "copy" => "copy",
            "encode" => "encode",
            "none" => "none",
            _ => "unknown",
        };
        if let Some(record) = &self.record {
            let span = record.context.span();
            span.set_attribute(KeyValue::new("media.video.mode", mode(video)));
            span.set_attribute(KeyValue::new("media.audio.mode", mode(audio)));
        }
    }
    pub fn context(&self) -> RequestContext {
        match &self.record {
            Some(record) => RequestContext {
                telemetry: record.telemetry.clone(),
                context: record.context.clone(),
            },
            None => RequestContext {
                telemetry: Telemetry::default(),
                context: Context::new(),
            },
        }
    }
    pub(crate) fn response_status(&mut self, status: u16) {
        if let Some(record) = &mut self.record {
            record.status = Some(status);
        }
    }
    pub(crate) fn method(&mut self, method: &str) {
        if let Some(record) = &mut self.record {
            record.method = match method {
                "GET" => "GET",
                "POST" => "POST",
                "PUT" => "PUT",
                "PATCH" => "PATCH",
                "DELETE" => "DELETE",
                "HEAD" => "HEAD",
                "OPTIONS" => "OPTIONS",
                _ => "OTHER",
            };
        }
    }
    pub(crate) fn bytes(&mut self, bytes: usize) {
        if let Some(record) = &mut self.record {
            record.bytes = record.bytes.saturating_add(bytes as u64);
        }
    }
    pub fn finish(mut self, outcome: Outcome) {
        if let Some(record) = self.record.take() {
            record.finish(outcome);
        }
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        if let Some(record) = self.record.take() {
            record.finish(Outcome::Failed(Failure::Cancelled));
        }
    }
}
impl Record {
    fn finish(self, outcome: Outcome) {
        let Some(inner) = &self.telemetry.inner else {
            return;
        };
        let elapsed = self.started.elapsed().as_secs_f64();
        let failure = outcome.failure();
        let attributes = [
            KeyValue::new("operation.name", self.operation.name()),
            KeyValue::new("outcome", failure.map(Failure::name).unwrap_or("success")),
            KeyValue::new("http.request.method", self.method),
        ];
        inner.requests.add(1, &attributes);
        inner.durations.record(elapsed, &attributes);
        inner.active.fetch_sub(1, Ordering::Relaxed);
        let span = self.context.span();
        span.set_attribute(KeyValue::new("http.request.method", self.method));
        let status = self.status.or(match outcome {
            Outcome::Http(status) => Some(status),
            _ => None,
        });
        if let Some(status) = status {
            span.set_attribute(KeyValue::new("http.response.status_code", status as i64));
        }
        if self.bytes > 0 {
            span.set_attribute(KeyValue::new("media.response.bytes", self.bytes as i64));
        }
        if let Some(failure) = failure {
            span.set_status(Status::error(failure.name()));
            span.set_attribute(KeyValue::new("error.type", failure.name()));
            let mut log = inner.logger.create_log_record();
            log.set_event_name("service.operation.failed");
            log.set_body(AnyValue::from("Service operation failed"));
            log.set_timestamp(SystemTime::now());
            log.set_severity_number(if failure == Failure::Cancelled {
                Severity::Warn
            } else {
                Severity::Error
            });
            let correlation = span.span_context();
            log.set_trace_context(
                correlation.trace_id(),
                correlation.span_id(),
                Some(correlation.trace_flags()),
            );
            log.add_attribute("operation.name", self.operation.name());
            log.add_attribute("error.type", failure.name());
            log.add_attribute("duration_ms", elapsed * 1000.0);
            if let Some(status) = status {
                log.add_attribute("http.response.status_code", status as i64);
            }
            inner.logger.emit(log);
        }
        span.end();
    }
}

pub fn observe(operation: Operation) -> Observation {
    CURRENT
        .try_with(|current| {
            current
                .telemetry
                .start(operation, current.context.clone(), SpanKind::Client)
        })
        .unwrap_or(Observation { record: None })
}
pub fn current_context() -> RequestContext {
    CURRENT.try_with(Clone::clone).unwrap_or(RequestContext {
        telemetry: Telemetry::default(),
        context: Context::new(),
    })
}
pub async fn in_context<T>(context: RequestContext, future: impl Future<Output = T>) -> T {
    CURRENT.scope(context, future).await
}
pub fn traceparent() -> Option<String> {
    CURRENT
        .try_with(|current| {
            let span = current.context.span();
            let context = span.span_context();
            context.is_valid().then(|| {
                format!(
                    "00-{}-{}-{:02x}",
                    context.trace_id(),
                    context.span_id(),
                    context.trace_flags().to_u8()
                )
            })
        })
        .ok()
        .flatten()
}

#[cfg(test)]
mod export_tests;
