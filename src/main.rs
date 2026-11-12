// Copyright (C) 2026 PSI SOFT Sp. z o.o.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//         http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod config;
mod encoding;
mod service;
mod session;

use axum::{
    extract::State,
    http::StatusCode,
    routing::post,
    Json, Router,
};
use axum_server::tls_rustls::RustlsConfig;
use odbc_api::Environment;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{CliAction, Config};
use crate::session::{
    ExecuteResponse, FetchResponse, QueryResponse, SessionCommand, SessionManager,
};

#[derive(Deserialize)]
struct ConnectRequest {
    dsn: String,
    user: Option<String>,
    password: Option<String>,
    charset: Option<String>,
}

#[derive(Serialize)]
struct ConnectResponse {
    session_id: String,
}

#[derive(Deserialize)]
struct AutoCommitRequest {
    session_id: String,
    auto_commit: bool,
}

#[derive(Deserialize)]
struct SessionOnlyRequest {
    session_id: String,
}

#[derive(Deserialize)]
struct FetchRequest {
    session_id: String,
    cursor_id: String,
    #[serde(default = "default_fetch_size")]
    fetch_size: usize,
}

fn default_fetch_size() -> usize {
    500
}

#[derive(Deserialize)]
struct CloseCursorRequest {
    session_id: String,
    cursor_id: String,
}

#[derive(Deserialize)]
struct SqlQueryRequest {
    session_id: Option<String>,
    dsn: Option<String>,
    user: Option<String>,
    password: Option<String>,
    sql: String,
    #[serde(default)]
    params: Vec<Option<String>>,
    charset: Option<String>,
    fetch_size: Option<usize>,
}

#[derive(Deserialize)]
struct SqlExecuteRequest {
    session_id: Option<String>,
    dsn: Option<String>,
    user: Option<String>,
    password: Option<String>,
    sql: String,
    #[serde(default)]
    params: Vec<Option<String>>,
    charset: Option<String>,
}

#[derive(Serialize)]
struct StatusResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct PingResponse {
    alive: bool,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

struct AppState {
    manager: Arc<SessionManager>,
}

fn main() -> anyhow::Result<()> {
    let (action, config) = match Config::from_env_and_args() {
        Ok(res) => res,
        Err(e) => {
            eprintln!("Błąd konfiguracji:\n{}\n", e);
            std::process::exit(1);
        }
    };

    match action {
        CliAction::ServiceWorker => {
            // Uruchomienie pod kontrolą Windows Service Control Manager (SCM)
            service::run_service()?;
            Ok(())
        }
        CliAction::Install { forward_args } => service::install_service(&forward_args),
        CliAction::Uninstall => service::uninstall_service(),
        CliAction::Start => service::start_service(),
        CliAction::Stop => service::stop_service(),
        CliAction::Status => service::query_service_status(),
        CliAction::Help => {
            Config::print_help();
            Ok(())
        }
        CliAction::Run => {
            // Tryb interaktywny w konsoli (Standalone)
            println!("Uruchamianie serwera w trybie interaktywnym (konsola)...");
            println!("Wciśnij Ctrl+C, aby zatrzymać serwer.\n");

            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;

            rt.block_on(async {
                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    if let Ok(()) = tokio::signal::ctrl_c().await {
                        println!("\nOdebrano sygnał Ctrl+C. Zatrzymywanie serwera...");
                        let _ = shutdown_tx.send(());
                    }
                });

                run_server(shutdown_rx, config).await
            })
        }
    }
}

async fn resolve_socket_addr(addr_str: &str) -> anyhow::Result<SocketAddr> {
    if let Ok(sa) = addr_str.parse::<SocketAddr>() {
        return Ok(sa);
    }

    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(addr_str)
        .await
        .map_err(|e| anyhow::anyhow!("Nie można rozwiązać adresu '{}': {}", addr_str, e))?
        .collect();

    if let Some(ipv4) = addrs.iter().find(|a| a.is_ipv4()) {
        return Ok(*ipv4);
    }

    addrs
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Brak wyników DNS dla adresu '{}'", addr_str))
}

pub async fn run_server(
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    config: Config,
) -> anyhow::Result<()> {
    if cfg!(not(target_pointer_width = "32")) {
        eprintln!("OSTRZEŻENIE: Serwer został skompilowany jako 64-bit!");
        eprintln!("Aby widzieć 32-bitowe źródła ODBC, skompiluj z flagą: cargo build --release --target i686-pc-windows-msvc");
    } else {
        println!("Architektura procesu: 32-bit (x86) - gotowy do obsługi 32-bitowych sterowników ODBC.");
    }

    // Weryfikacja i wczytanie certyfikatów TLS przed inicjalizacją ODBC (Opcja A: Fail-Closed)
    let tls_config = if config.tls_enabled {
        let (cert_path, key_path) = config
            .resolve_tls_paths()
            .map_err(|msg| anyhow::anyhow!("{}", msg))?;

        println!("Wczytywanie certyfikatu TLS: {}", cert_path.display());
        println!("Wczytywanie klucza TLS:      {}", key_path.display());

        let cfg = RustlsConfig::from_pem_file(&cert_path, &key_path)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Błąd wczytywania certyfikatu/klucza TLS (cert: '{}', key: '{}'): {}",
                    cert_path.display(),
                    key_path.display(),
                    e
                )
            })?;
        Some(cfg)
    } else {
        None
    };

    let socket_addr = resolve_socket_addr(&config.socket_addr_str()).await?;

    let env = Environment::new()?;
    let manager = Arc::new(SessionManager::new(env));
    let state = Arc::new(AppState {
        manager: Arc::clone(&manager),
    });

    // Zadanie w tle czyszczące porzucone sesje (konfigurowalny timeout, domyślnie 30 minut)
    let idle_timeout_secs = config.idle_timeout_secs;
    let reaper_mgr = Arc::clone(&manager);
    let reaper_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            reaper_mgr
                .cleanup_idle_sessions(Duration::from_secs(idle_timeout_secs))
                .await;
        }
    });

    let app = Router::new()
        .route("/ping", axum::routing::get(|| async { "pong" }))
        // Endpointy sesyjne (pełne wsparcie JDBC i transakcji)
        .route("/session/connect", post(handle_session_connect))
        .route("/session/autocommit", post(handle_session_autocommit))
        .route("/session/commit", post(handle_session_commit))
        .route("/session/rollback", post(handle_session_rollback))
        .route("/session/ping", post(handle_session_ping))
        .route("/session/close", post(handle_session_close))
        .route("/session/fetch", post(handle_session_fetch))
        .route("/session/close-cursor", post(handle_session_close_cursor))
        // Endpointy wykonawcze (wspierają zarówno sesję, jak i wywołania jednorazowe)
        .route("/query", post(handle_query))
        .route("/execute", post(handle_execute))
        .with_state(state);

    let handle = axum_server::Handle::new();
    let shutdown_handle = handle.clone();
    tokio::spawn(async move {
        let _ = shutdown_rx.await;
        println!("Odebrano sygnał zatrzymania serwera. Rozpoczynanie zamykania połączeń...");
        shutdown_handle.graceful_shutdown(Some(Duration::from_secs(10)));
    });

    match tls_config {
        Some(rustls_cfg) => {
            println!(
                "Serwer Rust ODBC Proxy nasłuchuje na https://{} (TLS / Certificate Pinning)",
                socket_addr
            );
            axum_server::bind_rustls(socket_addr, rustls_cfg)
                .handle(handle)
                .serve(app.into_make_service())
                .await?;
        }
        None => {
            eprintln!("OSTRZEŻENIE: Szyfrowanie TLS zostało jawnie wyłączone (--no-tls / ODBC_TLS_ENABLED=false)!");
            println!("Serwer Rust ODBC Proxy nasłuchuje na http://{}", socket_addr);
            axum_server::bind(socket_addr)
                .handle(handle)
                .serve(app.into_make_service())
                .await?;
        }
    }

    reaper_handle.abort();
    manager.close_all_sessions().await;
    println!("Serwer został bezpiecznie zatrzymany.");

    Ok(())
}

fn err_resp(msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse { error: msg.into() }),
    )
}

async fn handle_session_connect(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ConnectRequest>,
) -> Result<Json<ConnectResponse>, (StatusCode, Json<ErrorResponse>)> {
    let session_id = state
        .manager
        .create_session(
            &req.dsn,
            req.user.as_deref(),
            req.password.as_deref(),
            req.charset,
        )
        .await
        .map_err(err_resp)?;

    Ok(Json(ConnectResponse { session_id }))
}

async fn handle_session_autocommit(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AutoCommitRequest>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let sender = state
        .manager
        .get_sender(&req.session_id)
        .await
        .ok_or_else(|| err_resp(format!("Nie znaleziono sesji: {}", req.session_id)))?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    sender
        .send(SessionCommand::SetAutoCommit {
            auto_commit: req.auto_commit,
            resp: tx,
        })
        .map_err(|e| err_resp(e.to_string()))?;

    rx.await
        .map_err(|_| err_resp("Wątek sesji nie odpowiedział"))?
        .map_err(err_resp)?;

    Ok(Json(StatusResponse { status: "ok" }))
}

async fn handle_session_commit(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SessionOnlyRequest>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let sender = state
        .manager
        .get_sender(&req.session_id)
        .await
        .ok_or_else(|| err_resp(format!("Nie znaleziono sesji: {}", req.session_id)))?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    sender
        .send(SessionCommand::Commit { resp: tx })
        .map_err(|e| err_resp(e.to_string()))?;

    rx.await
        .map_err(|_| err_resp("Wątek sesji nie odpowiedział"))?
        .map_err(err_resp)?;

    Ok(Json(StatusResponse { status: "ok" }))
}

async fn handle_session_rollback(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SessionOnlyRequest>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let sender = state
        .manager
        .get_sender(&req.session_id)
        .await
        .ok_or_else(|| err_resp(format!("Nie znaleziono sesji: {}", req.session_id)))?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    sender
        .send(SessionCommand::Rollback { resp: tx })
        .map_err(|e| err_resp(e.to_string()))?;

    rx.await
        .map_err(|_| err_resp("Wątek sesji nie odpowiedział"))?
        .map_err(err_resp)?;

    Ok(Json(StatusResponse { status: "ok" }))
}

async fn handle_session_ping(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SessionOnlyRequest>,
) -> Result<Json<PingResponse>, (StatusCode, Json<ErrorResponse>)> {
    let sender = state
        .manager
        .get_sender(&req.session_id)
        .await
        .ok_or_else(|| err_resp(format!("Nie znaleziono sesji: {}", req.session_id)))?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    sender
        .send(SessionCommand::Ping { resp: tx })
        .map_err(|e| err_resp(e.to_string()))?;

    let alive = rx
        .await
        .map_err(|_| err_resp("Wątek sesji nie odpowiedział"))?
        .map_err(err_resp)?;

    Ok(Json(PingResponse { alive }))
}

async fn handle_session_close(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SessionOnlyRequest>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .manager
        .close_session(&req.session_id)
        .await
        .map_err(err_resp)?;

    Ok(Json(StatusResponse { status: "ok" }))
}

async fn handle_session_fetch(
    State(state): State<Arc<AppState>>,
    Json(req): Json<FetchRequest>,
) -> Result<Json<FetchResponse>, (StatusCode, Json<ErrorResponse>)> {
    let sender = state
        .manager
        .get_sender(&req.session_id)
        .await
        .ok_or_else(|| err_resp(format!("Nie znaleziono sesji: {}", req.session_id)))?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    sender
        .send(SessionCommand::FetchMore {
            cursor_id: req.cursor_id,
            fetch_size: req.fetch_size,
            resp: tx,
        })
        .map_err(|e| err_resp(e.to_string()))?;

    let response = rx
        .await
        .map_err(|_| err_resp("Wątek sesji nie odpowiedział"))?
        .map_err(err_resp)?;

    Ok(Json(response))
}

async fn handle_session_close_cursor(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CloseCursorRequest>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let sender = state
        .manager
        .get_sender(&req.session_id)
        .await
        .ok_or_else(|| err_resp(format!("Nie znaleziono sesji: {}", req.session_id)))?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    sender
        .send(SessionCommand::CloseCursor {
            cursor_id: req.cursor_id,
            resp: tx,
        })
        .map_err(|e| err_resp(e.to_string()))?;

    rx.await
        .map_err(|_| err_resp("Wątek sesji nie odpowiedział"))?
        .map_err(err_resp)?;

    Ok(Json(StatusResponse { status: "ok" }))
}

async fn handle_query(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SqlQueryRequest>,
) -> Result<Json<QueryResponse>, (StatusCode, Json<ErrorResponse>)> {
    if let Some(session_id) = &req.session_id {
        let sender = state
            .manager
            .get_sender(session_id)
            .await
            .ok_or_else(|| err_resp(format!("Nie znaleziono sesji: {}", session_id)))?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        sender
            .send(SessionCommand::Query {
                sql: req.sql,
                params: req.params,
                fetch_size: req.fetch_size,
                charset: req.charset,
                resp: tx,
            })
            .map_err(|e| err_resp(e.to_string()))?;

        let response = rx
            .await
            .map_err(|_| err_resp("Wątek sesji nie odpowiedział"))?
            .map_err(err_resp)?;

        Ok(Json(response))
    } else {
        // Wykonanie jednorazowe bez trwałej sesji (fallback)
        let dsn = req
            .dsn
            .ok_or_else(|| err_resp("Brak parametru 'session_id' lub 'dsn' w zapytaniu"))?;

        let response = state
            .manager
            .execute_ephemeral_query(
                &dsn,
                req.user.as_deref(),
                req.password.as_deref(),
                req.sql,
                req.params,
                req.fetch_size,
                req.charset,
            )
            .await
            .map_err(err_resp)?;

        Ok(Json(response))
    }
}

async fn handle_execute(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SqlExecuteRequest>,
) -> Result<Json<ExecuteResponse>, (StatusCode, Json<ErrorResponse>)> {
    if let Some(session_id) = &req.session_id {
        let sender = state
            .manager
            .get_sender(session_id)
            .await
            .ok_or_else(|| err_resp(format!("Nie znaleziono sesji: {}", session_id)))?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        sender
            .send(SessionCommand::Execute {
                sql: req.sql,
                params: req.params,
                resp: tx,
            })
            .map_err(|e| err_resp(e.to_string()))?;

        let response = rx
            .await
            .map_err(|_| err_resp("Wątek sesji nie odpowiedział"))?
            .map_err(err_resp)?;

        Ok(Json(response))
    } else {
        // Wykonanie jednorazowe bez trwałej sesji (fallback)
        let dsn = req
            .dsn
            .ok_or_else(|| err_resp("Brak parametru 'session_id' lub 'dsn' w zapytaniu"))?;

        let response = state
            .manager
            .execute_ephemeral_execute(
                &dsn,
                req.user.as_deref(),
                req.password.as_deref(),
                req.sql,
                req.params,
                req.charset,
            )
            .await
            .map_err(err_resp)?;

        Ok(Json(response))
    }
}
