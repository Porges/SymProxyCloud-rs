use anyhow::Context;
use axum::{
    body::Body,
    extract::{FromRef, Path, State},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use azure_core::auth::TokenCredential;
use clap::Parser;
use clap_verbosity_flag::{InfoLevel, LevelFilter, Verbosity};
use reqwest::StatusCode;
use serde::Deserialize;
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};
use thiserror::Error;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use url::Url;

/// `axum`-compatible error handler.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] anyhow::Error);

impl IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        tracing::error!("{:?}", self.0);

        // N.B: Normally returning the error in the response is not secure for
        // a production server, but since this server is only intended for local
        // use this is fine.
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", self.0)).into_response()
    }
}

#[derive(Deserialize, Debug, Clone)]
struct ConfigServer {
    /// The URL of the upstream server.
    url: Url,
    /// The scope of the authentication token.
    scope: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct Config {
    servers: Vec<ConfigServer>,
    listen_address: Option<SocketAddr>,
    i_am_not_an_idiot: bool,
}

#[derive(Parser, Debug, Clone)]
struct Args {
    #[command(flatten)]
    verbosity: Verbosity<InfoLevel>,

    /// Path to the configuration file
    #[arg(short, long, default_value = "default.toml")]
    config: PathBuf,
}

#[derive(Clone, FromRef)]
struct AppState {
    config: Config,
    token: Arc<dyn TokenCredential>,
}

/// Primary endpoint used to proxy a symbol file from the configured upstream server.
async fn symbol(
    State(token): State<Arc<dyn TokenCredential>>,
    State(config): State<Config>,
    Path((name1, hash, name2)): Path<(String, String, String)>,
) -> Result<Response, Error> {
    for server in &config.servers {
        // Dispatch a reqwest request to upstream, and serve the response.
        // https://github.com/tokio-rs/axum/blob/680cdcba7cfa0b4fb37aba0c129ab6e4379bae3b/examples/reqwest-response/src/main.rs#L53-L68
        let req_builder = reqwest::Client::new().get(
            server
                .url
                .join(&format!("{}/{}/{}", name1, hash, name2))
                .context("failed to build request url")?,
        );

        // If there is a scope attached to this server, attempt to authenticate.
        let req_builder = if let Some(scope) = &server.scope {
            req_builder.bearer_auth(
                token
                    .get_token(&[scope])
                    .await
                    .context("failed to get token")?
                    .token
                    .secret(),
            )
        } else {
            req_builder
        };

        let req = req_builder.send().await.context("failed to send request")?;

        // Check to see if the server returned a successful status code. If it didn't, continue on to the next server.
        if !req.status().is_success() {
            continue;
        }

        // Forward out the full response from the upstream server, including headers and status code.
        let mut response_builder = Response::builder().status(req.status());
        *response_builder.headers_mut().unwrap() = req.headers().clone();

        // Stream out the response from the upstream server as we receive it.
        return Ok(response_builder
            .body(Body::from_stream(req.bytes_stream()))
            .context("failed to build response body")?);
    }

    Ok(Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .unwrap())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Set up trace logging to console and account for the user-provided verbosity flag.
    if args.verbosity.log_level_filter() != LevelFilter::Off {
        let var_name = match args.verbosity.log_level_filter() {
            LevelFilter::Off => tracing::Level::INFO,
            LevelFilter::Error => tracing::Level::ERROR,
            LevelFilter::Warn => tracing::Level::WARN,
            LevelFilter::Info => tracing::Level::INFO,
            LevelFilter::Debug => tracing::Level::DEBUG,
            LevelFilter::Trace => tracing::Level::TRACE,
        };
        tracing_subscriber::fmt().with_max_level(var_name).init();
    }

    // Read and parse the user-provided configuration.
    let config = std::fs::read_to_string(&args.config).context("failed to read config file")?;
    let config: Config = toml::from_str(&config).context("failed to parse config file")?;

    // Authenticate.
    let token =
        azure_identity::create_default_credential().context("failed to create Azure credential")?;

    // Attempt to acquire a token upon startup just to surface any configuration errors early.
    for server in &config.servers {
        if let Some(scope) = &server.scope {
            let _tok = token
                .get_token(&[&scope])
                .await
                .context("failed to get token")?;
        }
    }

    let addr = config
        .listen_address
        .unwrap_or(SocketAddr::from((Ipv4Addr::LOCALHOST, 5000)));

    let has_auth = config.servers.iter().any(|s| s.scope.is_some());
    if has_auth && !config.i_am_not_an_idiot && !addr.ip().is_loopback() {
        anyhow::bail!("You have configured the proxy to listen on a routable IP address with an upstream server that requires authentication, but `i_am_not_an_idiot` is still `false` in your configuration file. Read the documentation carefully before enabling the setting.");
    }

    let listener = TcpListener::bind(&addr)
        .await
        .context("failed to bind address")?;

    // Set up the `axum` application with a single endpoint to handle symbol server requests.
    let app = Router::new()
        .route("/:name1/:hash/:name2", get(symbol))
        .layer(TraceLayer::new_for_http())
        .with_state(AppState { config, token });

    tracing::info!("listening on {addr}");

    // Serve the application :)
    axum::serve(listener, app.into_make_service())
        .await
        .context("failed to start server")
}
