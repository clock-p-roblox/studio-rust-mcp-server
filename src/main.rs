use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use color_eyre::eyre::{eyre, Result, WrapErr};
use helper_ws::HELPER_WS_PATH;
use rbx_studio_server::*;
use rmcp::{
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ServiceExt,
};
use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing_subscriber::{self, EnvFilter};
mod error;
mod helper_ws;
mod install;
mod rbx_studio_server;

/// Simple MCP proxy for Roblox Studio
/// Run without arguments to install the plugin
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// Run as MCP server on stdio.
    #[arg(short, long)]
    stdio: bool,

    /// Run as MCP streamable HTTP server on /mcp.
    #[arg(long)]
    http: bool,

    /// Disable bearer authentication for HTTP mode.
    #[arg(long, default_value_t = false)]
    no_auth: bool,

    /// Bearer token used for HTTP mode. Accepts both raw token and "Bearer <token>".
    #[arg(long)]
    bearer_token: Option<String>,

    /// Path to bearer token file for HTTP mode.
    #[arg(long)]
    bearer_token_file: Option<PathBuf>,

    /// Local port for Studio plugin bridge server.
    #[arg(long, default_value_t = STUDIO_PLUGIN_PORT)]
    plugin_port: u16,

    /// Local port for HTTP MCP endpoint (/mcp).
    #[arg(long, default_value_t = 44756)]
    http_port: u16,

    /// Write the bundled plugin file and exit.
    #[arg(long)]
    write_plugin: Option<PathBuf>,

    /// Optional task_id metadata exposed via /status and artifact metadata.
    #[arg(long)]
    task_id: Option<String>,

    /// Optional task generation metadata exposed via /status and helper fencing.
    #[arg(long)]
    task_generation: Option<u32>,

    /// Explicit workspace path metadata. Defaults to current directory.
    #[arg(long)]
    workspace_path: Option<PathBuf>,

    /// Optional public base URL metadata exposed via /status.
    #[arg(long)]
    public_base_url: Option<String>,

    /// Optional hub base URL used to validate helper launch fencing.
    #[arg(long)]
    hub_base_url: Option<String>,

    /// Optional bearer token used for hub requests.
    #[arg(long)]
    hub_bearer_token: Option<String>,

    /// Optional bearer token file used for hub requests.
    #[arg(long)]
    hub_bearer_token_file: Option<PathBuf>,
}

#[derive(Clone)]
struct HttpAuthState {
    bearer_token: Option<String>,
}

fn normalize_bearer_token(token: &str) -> Option<String> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return None;
    }

    let raw = trimmed.strip_prefix("Bearer ").unwrap_or(trimmed).trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_owned())
    }
}

fn default_feishu_token_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".dev.clock-p.com").join("feishu-token"))
}

fn load_token_from_file(path: &PathBuf) -> Result<Option<String>> {
    let content = fs::read_to_string(path)
        .wrap_err_with(|| format!("Could not read token file at {}", path.display()))?;
    Ok(normalize_bearer_token(&content))
}

fn resolve_http_bearer_token(args: &Args) -> Result<Option<String>> {
    if args.no_auth {
        return Ok(None);
    }

    if let Some(token) = args
        .bearer_token
        .as_deref()
        .and_then(normalize_bearer_token)
    {
        return Ok(Some(token));
    }

    if let Some(path) = args.bearer_token_file.as_ref() {
        return load_token_from_file(path);
    }

    if let Some(path) = default_feishu_token_path() {
        if path.exists() {
            return load_token_from_file(&path);
        }
    }

    Ok(None)
}

fn resolve_optional_bearer_token(
    token: Option<&str>,
    token_file: Option<&PathBuf>,
) -> Result<Option<String>> {
    if let Some(token) = token.and_then(normalize_bearer_token) {
        return Ok(Some(token));
    }
    if let Some(path) = token_file {
        return load_token_from_file(path);
    }
    if let Some(path) = default_feishu_token_path() {
        if path.exists() {
            return load_token_from_file(&path);
        }
    }
    Ok(None)
}

async fn require_http_auth(
    State(auth): State<HttpAuthState>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();
    let Some(expected_token) = auth.bearer_token.as_ref() else {
        return next.run(request).await;
    };

    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(normalize_bearer_token)
        .map(|provided| provided == *expected_token)
        .unwrap_or(false);

    if !authorized {
        tracing::warn!(path, "rejected unauthorized HTTP request");
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    tracing::info!(path, "accepted authorized HTTP request");
    next.run(request).await
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(io::stderr)
        .with_target(false)
        .with_thread_ids(true)
        .init();

    let args = Args::parse();
    tracing::info!(?args.stdio, ?args.http, plugin_port = args.plugin_port, http_port = args.http_port, no_auth = args.no_auth, "starting rbx-studio-mcp");

    if let Some(path) = args.write_plugin.as_ref() {
        install::write_plugin_to_path(path)?;
        tracing::info!(path = %path.display(), "wrote plugin bundle");
        println!("Wrote plugin to {}", path.display());
        return Ok(());
    }

    if !args.stdio && !args.http {
        return install::install().await;
    }

    tracing::info!("initialized server state");

    let workspace = match args.workspace_path.as_ref() {
        Some(path) => path.clone(),
        None => std::env::current_dir()?,
    };
    let hub_bearer_token = resolve_optional_bearer_token(
        args.hub_bearer_token.as_deref(),
        args.hub_bearer_token_file.as_ref(),
    )?;
    let server_state = Arc::new(Mutex::new(AppState::new(
        workspace,
        args.task_id.clone(),
        args.task_generation,
        args.public_base_url.clone(),
        args.hub_base_url.clone(),
        hub_bearer_token,
    )));
    tokio::spawn(helper_health_loop(Arc::clone(&server_state)));

    let (close_tx, close_rx) = tokio::sync::oneshot::channel();

    let listener =
        tokio::net::TcpListener::bind((Ipv4Addr::new(127, 0, 0, 1), args.plugin_port)).await;

    let server_state_clone = Arc::clone(&server_state);
    let server_handle = if let Ok(listener) = listener {
        let app = axum::Router::new()
            .route("/request", get(request_handler))
            .route("/response", post(response_handler))
            .route("/proxy", post(proxy_handler))
            .route("/status", get(status_handler))
            .with_state(server_state_clone);
        tracing::info!(
            "Studio plugin bridge HTTP server listening on 127.0.0.1:{}",
            args.plugin_port
        );
        tokio::spawn(async {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    _ = close_rx.await;
                })
                .await
                .unwrap();
        })
    } else {
        tracing::warn!(
            plugin_port = args.plugin_port,
            "plugin bridge port busy; falling back to proxy mode"
        );
        let plugin_port = args.plugin_port;
        tokio::spawn(async move {
            dud_proxy_loop(server_state_clone, close_rx, plugin_port).await;
        })
    };

    let mut http_close_tx = None;
    let mut http_handle = None;

    if args.http {
        let token = resolve_http_bearer_token(&args)?;
        if token.is_none() && !args.no_auth {
            return Err(eyre!(
                "HTTP mode requires a bearer token. Pass --bearer-token, --bearer-token-file, or use --no-auth for local testing."
            ));
        }
        tracing::info!(
            auth_enabled = token.is_some(),
            source = if args.bearer_token.is_some() {
                "cli-token"
            } else if args.bearer_token_file.is_some() {
                "token-file"
            } else {
                "default-token-path"
            },
            "resolved HTTP auth configuration"
        );

        let auth_state = HttpAuthState {
            bearer_token: token,
        };

        let streamable_state = Arc::clone(&server_state);
        let streamable_http_service: StreamableHttpService<RBXStudioServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(RBXStudioServer::new(Arc::clone(&streamable_state))),
                Default::default(),
                StreamableHttpServerConfig::default(),
            );

        let http_app = axum::Router::new()
            .route("/request", get(request_handler))
            .route("/response", post(response_handler))
            .route("/proxy", post(proxy_handler))
            .route("/status", get(status_handler))
            .route(HELPER_WS_PATH, get(helper_ws_handler))
            .nest_service("/mcp", streamable_http_service)
            .with_state(Arc::clone(&server_state))
            .layer(middleware::from_fn_with_state(
                auth_state,
                require_http_auth,
            ));

        let listener =
            tokio::net::TcpListener::bind((Ipv4Addr::new(127, 0, 0, 1), args.http_port)).await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        http_close_tx = Some(tx);

        tracing::info!(
            "HTTP MCP endpoint listening on http://127.0.0.1:{}/mcp",
            args.http_port
        );

        http_handle = Some(tokio::spawn(async move {
            axum::serve(listener, http_app)
                .with_graceful_shutdown(async move {
                    _ = rx.await;
                })
                .await
                .unwrap();
        }));
    }

    if args.stdio {
        tracing::info!("starting stdio MCP transport");
        let service = RBXStudioServer::new(Arc::clone(&server_state))
            .serve(rmcp::transport::stdio())
            .await
            .inspect_err(|e| {
                tracing::error!("serving error: {:?}", e);
            })?;
        service.waiting().await?;
    } else {
        tracing::info!("HTTP mode running. Press Ctrl+C to stop.");
        tokio::signal::ctrl_c().await?;
    }

    close_tx.send(()).ok();
    if let Some(tx) = http_close_tx {
        tx.send(()).ok();
    }

    tracing::info!("Waiting for web server to gracefully shutdown");
    server_handle.await.ok();
    if let Some(handle) = http_handle {
        handle.await.ok();
    }
    tracing::info!("Bye!");
    Ok(())
}
