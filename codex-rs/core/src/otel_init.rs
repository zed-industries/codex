use crate::config::Config;
use crate::config_types::OtelExporterKind as Kind;
use crate::config_types::OtelHttpProtocol as Protocol;
use crate::default_client::originator;
use codex_otel::config::OtelExporter;
use codex_otel::config::OtelHttpProtocol;
use codex_otel::config::OtelSettings;
use codex_otel::otel_provider::OtelProvider;
use std::error::Error;

/// Build an OpenTelemetry provider from the app Config.
///
/// Returns `None` when OTEL export is disabled.
pub fn build_provider(
    config: &Config,
    service_version: &str,
) -> Result<Option<OtelProvider>, Box<dyn Error>> {
    let exporter = match &config.otel.exporter {
        Kind::None => OtelExporter::None,
        Kind::OtlpHttp {
            endpoint,
            headers,
            protocol,
        } => {
            let protocol = match protocol {
                Protocol::Json => OtelHttpProtocol::Json,
                Protocol::Binary => OtelHttpProtocol::Binary,
            };

            OtelExporter::OtlpHttp {
                endpoint: endpoint.clone(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                protocol,
            }
        }
        Kind::OtlpGrpc { endpoint, headers } => OtelExporter::OtlpGrpc {
            endpoint: endpoint.clone(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        },
    };

    OtelProvider::from(&OtelSettings {
        service_name: originator().value.to_owned(),
        service_version: service_version.to_string(),
        codex_home: config.codex_home.clone(),
        environment: config.otel.environment.to_string(),
        exporter,
    })
}

/// Filter predicate for exporting only Codex-owned events via OTEL.
/// Keeps events that originated from codex_otel module
pub fn codex_export_filter(meta: &tracing::Metadata<'_>) -> bool {
    meta.target().starts_with("codex_otel")
}
