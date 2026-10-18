//! CLI/environment configuration for the `corx` binary.

use std::net::SocketAddr;

use axum_client_ip::ClientIpSource;
use clap::{Parser, ValueEnum};

/// Structured log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum LogFormat {
    /// One JSON object per line.
    Json,
    /// Human-readable pretty output.
    Pretty,
}

/// Runtime configuration, sourced from flags or `CORX_*` env vars.
#[derive(Debug, Parser)]
#[command(name = "corx", version, about = "Link-preview metadata service")]
pub(crate) struct Config {
    /// Listen address for the public API.
    #[arg(long, env = "CORX_BIND", default_value = "0.0.0.0:8080")]
    pub(crate) bind: SocketAddr,

    /// Comma-separated Origin allowlist for CORS, or `*`.
    #[arg(long, env = "CORX_ALLOWED_ORIGINS")]
    pub(crate) allowed_origins: String,

    /// `User-Agent` sent to upstream servers.
    #[arg(long, env = "CORX_USER_AGENT",
        default_value = concat!("corx/", env!("CARGO_PKG_VERSION"), " (+https://github.com/qntx/corx)"))]
    pub(crate) user_agent: String,

    /// Source used to derive the client IP for rate limiting.
    #[arg(long, env = "CORX_CLIENT_IP_SOURCE", default_value = "ConnectInfo")]
    pub(crate) client_ip_source: ClientIpSource,

    /// Sustained per-IP request rate per minute.
    #[arg(long, env = "CORX_IP_RATE_PER_MINUTE", default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) ip_rate_per_minute: u32,

    /// Per-IP burst allowance.
    #[arg(long, env = "CORX_IP_BURST", default_value_t = 120, value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) ip_burst: u32,

    /// Per-target-host request rate per minute.
    #[arg(long, env = "CORX_HOST_RATE_PER_MINUTE", default_value_t = 120, value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) host_rate_per_minute: u32,

    /// Maximum concurrently served requests; excess is shed with 503.
    #[arg(long, env = "CORX_MAX_CONCURRENCY", default_value_t = 512)]
    pub(crate) max_concurrency: usize,

    /// Maximum preview cache entries.
    #[arg(long, env = "CORX_CACHE_CAPACITY", default_value_t = 100_000)]
    pub(crate) cache_capacity: u64,

    /// Optional dedicated listen address for the Prometheus metrics exporter.
    /// Never exposed on the public port.
    #[arg(long, env = "CORX_METRICS_BIND")]
    pub(crate) metrics_bind: Option<SocketAddr>,

    /// Log output format; level is controlled by `RUST_LOG`.
    #[arg(long, env = "CORX_LOG_FORMAT", default_value = "json")]
    pub(crate) log_format: LogFormat,
}
