//! Provider construction for the external stream: `SdkLoggerProvider` +
//! `SdkMeterProvider`, never registered globally, never sharing anything with
//! the internal `RefreshableSpanExporter` pipeline.
//!
//! The exporters are plain `opentelemetry_otlp` http/protobuf or gRPC/protobuf
//! exporters built with **only** the customer headers from
//! `OTEL_EXPORTER_OTLP_HEADERS` — no code path here can attach
//! `Authorization`/`X-XAI-Token-Auth`/`x-userid`;
//! those constants live in `otel_layer` and are not referenced by this
//! module. No `AuthCredentialProvider` is ever read.

use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, HeaderName, HeaderValue};
use opentelemetry_otlp::{
    Protocol, WithExportConfig, WithHttpConfig, WithTonicConfig, tonic_types::metadata::MetadataMap,
};
use opentelemetry_sdk::logs::{
    BatchConfig, BatchConfigBuilder, BatchLogProcessor as ThreadBatchLogProcessor,
    LoggerProviderBuilder, SdkLoggerProvider,
    log_processor_with_async_runtime::BatchLogProcessor as RuntimeBatchLogProcessor,
};
use opentelemetry_sdk::metrics::{
    MeterProviderBuilder, PeriodicReader as ThreadPeriodicReader, SdkMeterProvider, Temporality,
    periodic_reader_with_async_runtime::PeriodicReader as RuntimePeriodicReader,
};
type BuildResult<T> = Result<T, opentelemetry_otlp::ExporterBuildError>;

type RuntimeCommand = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

#[derive(Clone)]
struct DedicatedRuntime {
    tx: tokio::sync::mpsc::UnboundedSender<RuntimeCommand>,
}

impl std::fmt::Debug for DedicatedRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DedicatedRuntime")
    }
}

impl DedicatedRuntime {
    fn new() -> Option<Self> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RuntimeCommand>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
        let pump = move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => {
                    let _ = ready_tx.send(true);
                    rt
                }
                Err(e) => {
                    tracing::error!(error = %e, "external OTEL gRPC runtime build failed; external telemetry disabled");
                    let _ = ready_tx.send(false);
                    return;
                }
            };
            rt.block_on(async move {
                while let Some(future) = rx.recv().await {
                    tokio::spawn(future);
                }
            });
        };
        let spawned = std::thread::Builder::new()
            .name("otel-external-rt".into())
            .spawn(pump);
        if let Err(e) = spawned {
            tracing::error!(error = %e, "external OTEL gRPC runtime thread spawn failed; external telemetry disabled");
            return None;
        }
        // Bounded: this runs on the startup path, and the host that refuses
        // threads is the one least likely to schedule this one promptly.
        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(true) => Some(Self { tx }),
            _ => None,
        }
    }

    fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> BuildResult<T> + Send + 'static,
    ) -> BuildResult<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.tx
            .send(Box::pin(async move {
                let _ = tx.send(f());
            }))
            .expect("external OTEL gRPC runtime thread must be alive");
        rx.recv()
            .expect("external OTEL gRPC runtime build response")
    }
}

impl opentelemetry_sdk::runtime::Runtime for DedicatedRuntime {
    fn spawn<F>(&self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let _ = self.tx.send(Box::pin(future));
    }

    fn delay(&self, duration: Duration) -> impl std::future::Future<Output = ()> + Send + 'static {
        tokio::time::sleep(duration)
    }
}

impl opentelemetry_sdk::runtime::RuntimeChannel for DedicatedRuntime {
    type Receiver<T: std::fmt::Debug + Send> = tokio_stream::wrappers::ReceiverStream<T>;
    type Sender<T: std::fmt::Debug + Send> = tokio::sync::mpsc::Sender<T>;

    fn batch_message_channel<T: std::fmt::Debug + Send>(
        &self,
        capacity: usize,
    ) -> (Self::Sender<T>, Self::Receiver<T>) {
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        (
            sender,
            tokio_stream::wrappers::ReceiverStream::new(receiver),
        )
    }
}

use super::config::{ExporterSelection, ExternalOtelConfig, OtlpTransport, TemporalityPreference};
use super::redact::{ExportHealth, RedactingLogExporter, SharedGates, ValidatingMetricExporter};

/// Resource shared by both providers. `builder_empty()` (not `builder()`):
/// the default `EnvResourceDetector` would export `OTEL_RESOURCE_ATTRIBUTES`
/// env values, bypassing the schema (same rationale as the internal layer).
fn build_resource(cfg: &ExternalOtelConfig) -> opentelemetry_sdk::Resource {
    let mut attrs = vec![
        opentelemetry::KeyValue::new("service.version", cfg.client.service_version.clone()),
        opentelemetry::KeyValue::new("client.version", cfg.client.client_version.clone()),
        opentelemetry::KeyValue::new("app.entrypoint", cfg.client.app_entrypoint.clone()),
        opentelemetry::KeyValue::new("grok_code.schema.version", super::schema::SCHEMA_VERSION),
    ];
    // terminal.type: emulator brand (TERM_PROGRAM) or terminfo type (TERM).
    if let Some(terminal_type) = std::env::var("TERM_PROGRAM")
        .ok()
        .or_else(|| std::env::var("TERM").ok())
        .filter(|v| !v.is_empty())
    {
        attrs.push(opentelemetry::KeyValue::new("terminal.type", terminal_type));
    }
    opentelemetry_sdk::Resource::builder_empty()
        // RQ6 (final): `grok-cli`, a wire commitment.
        .with_service_name("grok-cli")
        .with_attributes(attrs)
        .build()
}

fn temporality(pref: TemporalityPreference) -> Temporality {
    match pref {
        TemporalityPreference::Delta => Temporality::Delta,
        TemporalityPreference::Cumulative => Temporality::Cumulative,
    }
}

/// Console (stderr) log exporter for local debugging
/// (`OTEL_LOGS_EXPORTER=console`). Writes to **stderr** so stdout protocol
/// channels (headless/stream-JSON) are never corrupted.
#[derive(Debug)]
struct StderrLogExporter;

impl opentelemetry_sdk::logs::LogExporter for StderrLogExporter {
    fn export(
        &self,
        batch: opentelemetry_sdk::logs::LogBatch<'_>,
    ) -> impl std::future::Future<Output = opentelemetry_sdk::error::OTelSdkResult> + Send {
        for (record, _scope) in batch.iter() {
            let attrs: Vec<String> = record
                .attributes_iter()
                .map(|(k, v)| format!("{}={v:?}", k.as_str()))
                .collect();
            eprintln!(
                "[external-otel] event={} {}",
                record.event_name().unwrap_or("?"),
                attrs.join(" ")
            );
        }
        std::future::ready(Ok(()))
    }
}

/// Console (stderr) metric exporter for local debugging.
#[derive(Debug)]
struct StderrMetricExporter {
    temporality: Temporality,
}

impl opentelemetry_sdk::metrics::exporter::PushMetricExporter for StderrMetricExporter {
    fn export(
        &self,
        metrics: &opentelemetry_sdk::metrics::data::ResourceMetrics,
    ) -> impl std::future::Future<Output = opentelemetry_sdk::error::OTelSdkResult> + Send {
        for scope in metrics.scope_metrics() {
            for metric in scope.metrics() {
                eprintln!(
                    "[external-otel] metric={} {:?}",
                    metric.name(),
                    metric.data()
                );
            }
        }
        std::future::ready(Ok(()))
    }

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(
        &self,
        _timeout: std::time::Duration,
    ) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }

    fn temporality(&self) -> Temporality {
        self.temporality
    }
}

/// Customer headers as the HTTP OTLP builder's header map. The **only** headers
/// the external HTTP exporters send (pinned by the header-isolation test below).
fn customer_headers(headers: &[(String, String)]) -> std::collections::HashMap<String, String> {
    headers.iter().cloned().collect()
}

/// Customer headers as gRPC metadata. Invalid metadata keys/values are skipped;
/// this mirrors the HTTP builder's "only customer-supplied headers" invariant
/// without letting one malformed entry disable telemetry entirely.
fn customer_metadata(input: &[(String, String)]) -> MetadataMap {
    let mut headers = HeaderMap::new();
    for (key, value) in input {
        let Ok(header_name) = HeaderName::try_from(key.as_str()) else {
            tracing::warn!(key = %key, "external otel: skipping invalid gRPC metadata key");
            continue;
        };
        let Ok(header_value) = HeaderValue::from_str(value) else {
            tracing::warn!(key = %key, "external otel: skipping invalid gRPC metadata value");
            continue;
        };
        headers.insert(header_name, header_value);
    }
    MetadataMap::from_headers(headers)
}

pub(crate) struct BuiltProviders {
    pub logger_provider: Option<SdkLoggerProvider>,
    pub meter_provider: Option<SdkMeterProvider>,
}

enum OtlpExportTransport<'a> {
    HttpProtobuf(&'a crate::otlp_http::BlockingOtlpClient),
    Grpc(&'a DedicatedRuntime),
}

trait OtlpExportFactory {
    type Exporter;

    fn export(&self, transport: OtlpExportTransport<'_>) -> BuildResult<Self::Exporter>;
}

struct OtlpLogExporterBuilder<'a> {
    cfg: &'a ExternalOtelConfig,
}

impl OtlpExportFactory for OtlpLogExporterBuilder<'_> {
    type Exporter = opentelemetry_otlp::LogExporter;

    fn export(&self, transport: OtlpExportTransport<'_>) -> BuildResult<Self::Exporter> {
        match transport {
            OtlpExportTransport::HttpProtobuf(http_client) => {
                opentelemetry_otlp::LogExporter::builder()
                    .with_http()
                    // Pin http/protobuf. opentelemetry-otlp's default protocol
                    // is compile-time, feature-gated: `http-json` (if unified
                    // into the build, as it is under Bazel) flips the default to
                    // JSON, while a pure-cargo build of this crate defaults to
                    // protobuf. Pin explicitly so the contract holds on every
                    // build when HTTP transport is selected.
                    .with_protocol(Protocol::HttpBinary)
                    .with_http_client(http_client.clone())
                    .with_endpoint(&self.cfg.logs_endpoint)
                    .with_headers(customer_headers(&self.cfg.logs_headers))
                    .build()
            }
            OtlpExportTransport::Grpc(runtime) => {
                let endpoint = self.cfg.logs_endpoint.clone();
                let timeout = self.cfg.timeout;
                let metadata = customer_metadata(&self.cfg.logs_headers);
                runtime.run(move || {
                    opentelemetry_otlp::LogExporter::builder()
                        .with_tonic()
                        .with_endpoint(endpoint)
                        .with_timeout(timeout)
                        .with_metadata(metadata)
                        .build()
                })
            }
        }
    }
}

struct OtlpMetricExporterBuilder<'a> {
    cfg: &'a ExternalOtelConfig,
    temporality: Temporality,
}

impl OtlpExportFactory for OtlpMetricExporterBuilder<'_> {
    type Exporter = opentelemetry_otlp::MetricExporter;

    fn export(&self, transport: OtlpExportTransport<'_>) -> BuildResult<Self::Exporter> {
        match transport {
            OtlpExportTransport::HttpProtobuf(http_client) => {
                opentelemetry_otlp::MetricExporter::builder()
                    .with_http()
                    // Pin http/protobuf (see the logs exporter above for the
                    // feature-unification rationale).
                    .with_protocol(Protocol::HttpBinary)
                    .with_http_client(http_client.clone())
                    .with_endpoint(&self.cfg.metrics_endpoint)
                    .with_headers(customer_headers(&self.cfg.metrics_headers))
                    .with_temporality(self.temporality)
                    .build()
            }
            OtlpExportTransport::Grpc(runtime) => {
                let endpoint = self.cfg.metrics_endpoint.clone();
                let timeout = self.cfg.timeout;
                let metadata = customer_metadata(&self.cfg.metrics_headers);
                let temporality = self.temporality;
                runtime.run(move || {
                    opentelemetry_otlp::MetricExporter::builder()
                        .with_tonic()
                        .with_endpoint(endpoint)
                        .with_timeout(timeout)
                        .with_metadata(metadata)
                        .with_temporality(temporality)
                        .build()
                })
            }
        }
    }
}

fn build_log_otlp_provider(
    builder: LoggerProviderBuilder,
    cfg: &ExternalOtelConfig,
    batch_config: BatchConfig,
    http_client: Option<&crate::otlp_http::BlockingOtlpClient>,
    gates: SharedGates,
    health: Arc<ExportHealth>,
) -> BuildResult<LoggerProviderBuilder> {
    let exporter_builder = OtlpLogExporterBuilder { cfg };
    Ok(match cfg.transport {
        OtlpTransport::HttpProtobuf => {
            let exporter = exporter_builder.export(OtlpExportTransport::HttpProtobuf(
                http_client.expect("client built for http/protobuf selection"),
            ))?;
            builder.with_log_processor(
                ThreadBatchLogProcessor::builder(RedactingLogExporter::new(
                    exporter, gates, health,
                ))
                .with_batch_config(batch_config)
                .build(),
            )
        }
        OtlpTransport::Grpc => {
            let Some(runtime) = DedicatedRuntime::new() else {
                return Err(opentelemetry_otlp::ExporterBuildError::ThreadSpawnFailed);
            };
            let exporter = exporter_builder.export(OtlpExportTransport::Grpc(&runtime))?;
            builder.with_log_processor(
                RuntimeBatchLogProcessor::builder(
                    RedactingLogExporter::new(exporter, gates, health),
                    runtime,
                )
                .with_batch_config(batch_config)
                .build(),
            )
        }
    })
}

fn build_metric_otlp_provider(
    builder: MeterProviderBuilder,
    cfg: &ExternalOtelConfig,
    http_client: Option<&crate::otlp_http::BlockingOtlpClient>,
    health: Arc<ExportHealth>,
) -> BuildResult<MeterProviderBuilder> {
    let exporter_builder = OtlpMetricExporterBuilder {
        cfg,
        temporality: temporality(cfg.temporality),
    };
    Ok(match cfg.transport {
        OtlpTransport::HttpProtobuf => {
            let exporter = exporter_builder.export(OtlpExportTransport::HttpProtobuf(
                http_client.expect("client built for http/protobuf selection"),
            ))?;
            builder.with_reader(
                ThreadPeriodicReader::builder(ValidatingMetricExporter::new(exporter, health))
                    .with_interval(cfg.metric_export_interval)
                    .build(),
            )
        }
        OtlpTransport::Grpc => {
            let Some(runtime) = DedicatedRuntime::new() else {
                return Err(opentelemetry_otlp::ExporterBuildError::ThreadSpawnFailed);
            };
            let exporter = exporter_builder.export(OtlpExportTransport::Grpc(&runtime))?;
            builder.with_reader(
                RuntimePeriodicReader::builder(
                    ValidatingMetricExporter::new(exporter, health),
                    runtime,
                )
                .with_interval(cfg.metric_export_interval)
                .build(),
            )
        }
    })
}

fn wrap_console_log_exporter(
    builder: LoggerProviderBuilder,
    batch_config: BatchConfig,
    gates: SharedGates,
    health: Arc<ExportHealth>,
) -> LoggerProviderBuilder {
    builder.with_log_processor(
        ThreadBatchLogProcessor::builder(RedactingLogExporter::new(
            StderrLogExporter,
            gates,
            health,
        ))
        .with_batch_config(batch_config)
        .build(),
    )
}

fn wrap_console_metric_exporter(
    builder: MeterProviderBuilder,
    cfg: &ExternalOtelConfig,
    health: Arc<ExportHealth>,
) -> MeterProviderBuilder {
    builder.with_reader(
        ThreadPeriodicReader::builder(ValidatingMetricExporter::new(
            StderrMetricExporter {
                temporality: temporality(cfg.temporality),
            },
            health,
        ))
        .with_interval(cfg.metric_export_interval)
        .build(),
    )
}

/// Build the providers per the resolved config. Returns `None` providers for
/// signals whose exporter selection is `none`.
pub(crate) fn build(
    cfg: &ExternalOtelConfig,
    gates: SharedGates,
    health: Arc<ExportHealth>,
) -> Result<BuiltProviders, opentelemetry_otlp::ExporterBuildError> {
    // Build the shared blocking client only when the HTTP transport is
    // selected (`otlp_http` handles the dedicated-thread construction). A
    // build failure disables the external stream (caller warns) — it must
    // never panic the process.
    let needs_http_client = cfg.transport == OtlpTransport::HttpProtobuf
        && (cfg.logs_exporter == ExporterSelection::Otlp
            || cfg.metrics_exporter == ExporterSelection::Otlp);
    let http_client = needs_http_client
        .then(|| crate::otlp_http::build_blocking_client(cfg.timeout))
        .transpose()
        .map_err(opentelemetry_otlp::ExporterBuildError::InternalFailure)?;

    // Console output is suppressed in the agent/headless entrypoints:
    // wrapping harnesses routinely capture stderr for diagnostics, and
    // interleaving periodic telemetry dumps there degrades those logs.
    let console_ok = !matches!(cfg.client.app_entrypoint.as_str(), "agent" | "headless");

    let logger_provider = match cfg.logs_exporter {
        ExporterSelection::None => None,
        ExporterSelection::Console if !console_ok => {
            tracing::debug!(
                "external otel: console logs exporter suppressed in agent/headless entrypoint"
            );
            None
        }
        selection => {
            let batch_config = BatchConfigBuilder::default()
                .with_scheduled_delay(cfg.logs_export_interval)
                .with_max_export_batch_size(64)
                .build();
            let builder = SdkLoggerProvider::builder().with_resource(build_resource(cfg));
            let provider = match selection {
                ExporterSelection::Otlp => build_log_otlp_provider(
                    builder,
                    cfg,
                    batch_config,
                    http_client.as_ref(),
                    gates.clone(),
                    health.clone(),
                )?,
                _ => {
                    wrap_console_log_exporter(builder, batch_config, gates.clone(), health.clone())
                }
            };
            Some(provider.build())
        }
    };

    let meter_provider = match cfg.metrics_exporter {
        ExporterSelection::None => None,
        ExporterSelection::Console if !console_ok => {
            tracing::debug!(
                "external otel: console metrics exporter suppressed in agent/headless entrypoint"
            );
            None
        }
        selection => {
            let builder = SdkMeterProvider::builder().with_resource(build_resource(cfg));
            let provider = match selection {
                ExporterSelection::Otlp => {
                    build_metric_otlp_provider(builder, cfg, http_client.as_ref(), health.clone())?
                }
                _ => wrap_console_metric_exporter(builder, cfg, health.clone()),
            };
            Some(provider.build())
        }
    };

    Ok(BuiltProviders {
        logger_provider,
        meter_provider,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::config::ExternalOtelConfig;
    use std::collections::HashMap;

    fn cfg_with_headers(headers: Vec<(String, String)>) -> ExternalOtelConfig {
        let mut cfg = ExternalOtelConfig::resolve_with(
            |name| match name {
                "GROK_EXTERNAL_OTEL" => Some("1".into()),
                "OTEL_LOGS_EXPORTER" => Some("otlp".into()),
                _ => None,
            },
            None,
        )
        .expect("test config must resolve");
        cfg.logs_headers = headers.clone();
        cfg.metrics_headers = headers;
        cfg
    }

    /// Header-isolation invariant (T2): the outgoing header map equals
    /// exactly the parsed `OTEL_EXPORTER_OTLP_HEADERS` — no `Authorization`,
    /// `X-XAI-Token-Auth`, `x-userid`, or `x-teamid` unless customer-supplied
    /// (complement of the internal pipeline's
    /// `extra_headers_override_bearer_but_keep_static_identity`).
    #[test]
    fn exporter_headers_are_exactly_customer_headers() {
        let cfg = cfg_with_headers(vec![("x-collector-token".into(), "abc".into())]);
        let headers = customer_headers(&cfg.logs_headers);
        let expected: HashMap<String, String> =
            [("x-collector-token".to_string(), "abc".to_string())].into();
        assert_eq!(headers, expected);
        for forbidden in ["Authorization", "X-XAI-Token-Auth", "x-userid", "x-teamid"] {
            assert!(
                !headers.contains_key(forbidden),
                "{forbidden} must never be auto-attached to external exports"
            );
        }
    }

    #[test]
    fn customer_supplied_authorization_passes_through() {
        // The customer may auth their own collector however they want.
        let cfg = cfg_with_headers(vec![("Authorization".into(), "Bearer customer".into())]);
        assert_eq!(
            customer_headers(&cfg.logs_headers)
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer customer")
        );
    }

    #[test]
    fn exporter_metadata_is_customer_headers_only() {
        let cfg = cfg_with_headers(vec![
            ("x-collector-token".into(), "abc".into()),
            ("bad header".into(), "skip".into()),
        ]);
        let metadata = customer_metadata(&cfg.logs_headers);
        assert_eq!(
            metadata
                .get("x-collector-token")
                .and_then(|v| v.to_str().ok()),
            Some("abc")
        );
        for forbidden in ["x-xai-token-auth", "x-userid", "x-teamid"] {
            assert!(metadata.get(forbidden).is_none());
        }
    }
}
