use axum::Router;
use komga_application::operational::StartupTimingState;
use komga_infrastructure_base::close_all_shared_pools;
use komga_interfaces::state::RuntimeSseEventHub;
use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::oneshot;
use tokio::sync::watch;

use super::compose_http_runtime::compose_http_runtime;
use crate::runtime::{
    TaskRouterParts, TaskRuntimeMode, start_task_runtime, start_task_runtime_with_events,
};
use komga_config::env_config::RuntimeConfig;

const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(5);

pub(crate) async fn build_router(
    config: &RuntimeConfig,
    mode: TaskRuntimeMode,
    shutdown_trigger: Option<watch::Sender<bool>>,
    startup_timing: StartupTimingState,
) -> std::io::Result<Router> {
    let router_parts = start_task_runtime(config, mode).await?;
    build_router_from_parts(config, router_parts, shutdown_trigger, startup_timing)
}

pub(crate) async fn build_router_with_runtime_events(
    config: &RuntimeConfig,
    mode: TaskRuntimeMode,
    shutdown_trigger: Option<watch::Sender<bool>>,
    startup_timing: StartupTimingState,
    runtime_events: Arc<RuntimeSseEventHub>,
) -> std::io::Result<Router> {
    let router_parts = start_task_runtime_with_events(config, mode, runtime_events).await?;
    build_router_from_parts(config, router_parts, shutdown_trigger, startup_timing)
}

fn build_router_from_parts(
    config: &RuntimeConfig,
    router_parts: TaskRouterParts,
    shutdown_trigger: Option<watch::Sender<bool>>,
    startup_timing: StartupTimingState,
) -> std::io::Result<Router> {
    let app = compose_http_runtime(
        config,
        router_parts.http,
        shutdown_trigger,
        startup_timing.clone(),
    );
    let router = build_http_router(app);
    let router = router_parts.lifecycle.attach(router);
    Ok(router)
}

pub(crate) async fn build_router_with_config(
    config: &RuntimeConfig,
    startup_timing: StartupTimingState,
) -> std::io::Result<Router> {
    build_router(
        config,
        TaskRuntimeMode::WorkersEnabled { shutdown_rx: None },
        None,
        startup_timing,
    )
    .await
}

pub(crate) async fn build_router_without_runtime_workers(
    config: &RuntimeConfig,
    startup_timing: StartupTimingState,
) -> std::io::Result<Router> {
    build_router(
        config,
        TaskRuntimeMode::WorkersDisabled,
        None,
        startup_timing,
    )
    .await
}

pub(crate) async fn build_router_without_runtime_workers_with_runtime_events(
    config: &RuntimeConfig,
    startup_timing: StartupTimingState,
    runtime_events: Arc<RuntimeSseEventHub>,
) -> std::io::Result<Router> {
    build_router_with_runtime_events(
        config,
        TaskRuntimeMode::WorkersDisabled,
        None,
        startup_timing,
        runtime_events,
    )
    .await
}

pub(crate) async fn serve(
    listener: TcpListener,
    config: RuntimeConfig,
    startup_timing: StartupTimingState,
    startup_started_at: Instant,
) -> std::io::Result<()> {
    crate::bootstrap::emit_startup_banner_and_runtime_event(&config).await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let parts = start_task_runtime(
        &config,
        TaskRuntimeMode::WorkersEnabled {
            shutdown_rx: Some(shutdown_rx.clone()),
        },
    )
    .await?;
    let lifecycle = parts.lifecycle.clone();
    let app = compose_http_runtime(
        &config,
        parts.http,
        Some(shutdown_tx.clone()),
        startup_timing.clone(),
    );
    if config.webui_auto_update && config.webui_dir.is_some() {
        crate::webui_updater::WebuiUpdater::start(
            app.operational.webui_dir.clone(),
            config.config_dir.clone().unwrap_or_default(),
            config.webui_update_interval,
        );
    }
    let router = build_http_router(app);
    let router = lifecycle.clone().attach(router);
    emit_server_bind_event(&listener);

    serve_router_with_shutdown_timeout(
        listener,
        router,
        shutdown_tx,
        shutdown_rx,
        startup_timing,
        startup_started_at,
        lifecycle,
    )
    .await
}

async fn serve_router_with_shutdown_timeout(
    listener: TcpListener,
    router: Router,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    startup_timing: StartupTimingState,
    startup_started_at: Instant,
    lifecycle: crate::runtime::RouterRuntimeLifecycle,
) -> std::io::Result<()> {
    let fallback_deadline = || Instant::now() + SHUTDOWN_GRACE_PERIOD;
    let (shutdown_started_tx, mut shutdown_started_rx) = oneshot::channel::<Instant>();
    let (server_ready_tx, mut server_ready_rx) = oneshot::channel();
    startup_timing.record_application_started(startup_started_at.elapsed());
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move {
        let server = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal(
            shutdown_tx,
            shutdown_rx,
            shutdown_started_tx,
        ));
        let server = server.into_future();
        tokio::pin!(server);
        let mut server_ready_tx = Some(server_ready_tx);
        // Readiness is tied to the serve future entering its accept loop, not just to router
        // construction or task spawning.
        std::future::poll_fn(move |cx| {
            let result = Future::poll(server.as_mut(), cx);
            if matches!(&result, Poll::Pending)
                && let Some(server_ready_tx) = server_ready_tx.take()
            {
                let _ = server_ready_tx.send(());
            }
            result
        })
        .await
    });

    let mut ready = false;
    let (deadline, server_result) = loop {
        tokio::select! {
            _ = &mut server_ready_rx, if !ready => {
                ready = true;
                startup_timing.record_application_ready(startup_started_at.elapsed());
            },
            result = server.join_next() => {
                let deadline = shutdown_started_rx.try_recv().unwrap_or_else(|_| fallback_deadline());
                break (deadline, Some(flatten_server_task_result(result)));
            },
            deadline = &mut shutdown_started_rx => {
                break (deadline.unwrap_or_else(|_| fallback_deadline()), None);
            },
        }
    };
    let shutdown = async {
        let result = match server_result {
            Some(result) => result,
            None => flatten_server_task_result(server.join_next().await),
        };
        lifecycle.shutdown().await;
        complete_shutdown_lifecycle().await;
        result
    };
    match tokio::time::timeout_at(deadline.into(), shutdown).await {
        Ok(result) => result,
        Err(_) => {
            server.abort_all();
            while server.join_next().await.is_some() {}
            tracing::error!(
                event = "server_shutdown_timeout",
                outcome = "forced",
                shutdown_grace_period_ms = SHUTDOWN_GRACE_PERIOD.as_millis() as u64,
                "Shutdown deadline exceeded; unfinished tasks will recover on restart"
            );
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "server shutdown deadline exceeded",
            ))
        }
    }
}

fn build_http_router(app: komga_interfaces::state::HttpAppState) -> Router {
    komga_interfaces::router::build_router(app)
}

async fn shutdown_signal(
    shutdown_tx: watch::Sender<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
    shutdown_started_tx: oneshot::Sender<Instant>,
) {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};

        if let Ok(mut stream) = signal(SignalKind::terminate()) {
            let _ = stream.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    let shutdown_request = async move {
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            if shutdown_rx.changed().await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
        _ = shutdown_request => {},
    }

    let _ = shutdown_started_tx.send(Instant::now() + SHUTDOWN_GRACE_PERIOD);
    let _ = shutdown_tx.send(true);
}

fn flatten_server_task_result(
    result: Option<Result<std::io::Result<()>, tokio::task::JoinError>>,
) -> std::io::Result<()> {
    match result.expect("server task should be registered") {
        Ok(result) => result,
        Err(error) => Err(std::io::Error::other(format!(
            "server task failed to join: {error}"
        ))),
    }
}

fn emit_server_bind_event(listener: &TcpListener) {
    let bind_address = listener
        .local_addr()
        .map(|address| address.to_string())
        .unwrap_or_default();

    tracing::info!(
        event = "server_bind",
        outcome = "ready",
        bind_address = bind_address.as_str(),
        "Server listener ready",
    );
}

async fn complete_shutdown_lifecycle() {
    tracing::info!(
        event = "server_shutdown",
        outcome = "graceful",
        "Server shutdown requested",
    );
    close_all_shared_pools().await;
    tracing::info!(
        event = "shared_pool_close",
        outcome = "closed",
        "Closed shared sqlite pools",
    );
}
