//! `corx` — native link-preview server entrypoint.

mod app;
mod config;
mod fetch;
mod runtime;

use std::process::ExitCode;

use axum::Router;
use clap::Parser;
use corx::http::Origins;
use tokio::net::TcpListener;

use crate::app::{Limits, router};
use crate::config::{Config, LogFormat};
use crate::fetch::NativeFetch;

fn init_tracing(format: LogFormat) {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt::SubscriberBuilder;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = SubscriberBuilder::default().with_env_filter(filter);
    match format {
        LogFormat::Json => {
            builder.json().with_ansi(false).init();
        }
        LogFormat::Pretty => {
            builder.pretty().init();
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                res = ctrl_c => { drop(res); }
                _sig = term.recv() => {}
            }
        }
        Err(_) => drop(ctrl_c.await),
    }
    #[cfg(not(unix))]
    drop(ctrl_c.await);
}

/// Installs the Prometheus exporter when `--metrics-bind` is set; the
/// explicit bind must never be silently ignored.
fn install_metrics(bind: Option<std::net::SocketAddr>) -> bool {
    let Some(addr) = bind else { return true };
    if let Err(e) = metrics_exporter_prometheus::PrometheusBuilder::new()
        .with_http_listener(addr)
        .install()
    {
        tracing::error!(error = %e, bind = %addr, "failed to install metrics exporter");
        return false;
    }
    true
}

fn build_app(cfg: &Config) -> Option<Router> {
    let origins = match Origins::parse(&cfg.allowed_origins) {
        Ok(origins) => origins,
        Err(e) => {
            tracing::error!(error = %e, "invalid --allowed-origins");
            return None;
        }
    };
    let fetch = match NativeFetch::new(&cfg.user_agent) {
        Ok(fetch) => fetch,
        Err(e) => {
            tracing::error!(error = %e, "failed to build HTTP client");
            return None;
        }
    };
    let limits = Limits::from(cfg);
    Some(router(origins, fetch, &limits))
}

async fn bind_listener(bind: std::net::SocketAddr) -> Option<TcpListener> {
    match TcpListener::bind(bind).await {
        Ok(listener) => Some(listener),
        Err(e) => {
            tracing::error!(error = %e, bind = %bind, "failed to bind");
            None
        }
    }
}

async fn serve(app: Router, bind: std::net::SocketAddr) -> ExitCode {
    let Some(listener) = bind_listener(bind).await else {
        return ExitCode::FAILURE;
    };
    tracing::info!(bind = %bind, "listening");
    let make = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, make)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_or_else(
            |e| {
                tracing::error!(error = %e, "server error");
                ExitCode::FAILURE
            },
            |()| ExitCode::SUCCESS,
        )
}

async fn run(cfg: Config) -> ExitCode {
    if !install_metrics(cfg.metrics_bind) {
        return ExitCode::FAILURE;
    }
    let Some(app) = build_app(&cfg) else {
        return ExitCode::FAILURE;
    };
    serve(app, cfg.bind).await
}

fn main() -> ExitCode {
    let cfg = Config::parse();
    init_tracing(cfg.log_format);
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(run(cfg)),
        Err(e) => {
            tracing::error!(error = %e, "tokio runtime init failed");
            ExitCode::FAILURE
        }
    }
}
