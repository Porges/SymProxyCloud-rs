use anyhow::Context;
use axum::{
    body::Body,
    extract::{FromRef, Path, State},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use azure_core::auth::TokenCredential;
use azure_storage::StorageCredentials;
use azure_storage_blobs::blob::{BlobBlockType, BlockList};
use clap::Parser;
use clap_verbosity_flag::{InfoLevel, LevelFilter, Verbosity};
use futures::{Stream, StreamExt, TryStreamExt};
use reqwest::StatusCode;
use serde::Deserialize;
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    pin::Pin,
    sync::Arc,
};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::trace::TraceLayer;
use tracing::trace;
use url::Url;
use uuid::Uuid;

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
struct ConfigAuth {
    /// The scope of the authentication token
    scope: String,
}

#[derive(Deserialize, Debug, Clone)]
struct ConfigCache {
    /// The Azure storage account to use
    storage_account: String,
    /// The container within the storage account to use
    storage_container: String,
    /// Access key
    key: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct ConfigServer {
    /// The URL of the upstream server
    url: Url,
    /// Authentication settings
    auth: Option<ConfigAuth>,
}

#[derive(Deserialize, Debug, Clone)]
struct Config {
    listen_address: Option<SocketAddr>,
    i_am_not_an_idiot: bool,
    cache: Option<ConfigCache>,
    servers: Vec<ConfigServer>,
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
    // Attempt the storage account first, if one is set.
    if let Some(cache) = &config.cache {
        let cred = if let Some(key) = &cache.key {
            StorageCredentials::access_key(&cache.storage_account, key.clone())
        } else {
            StorageCredentials::token_credential(token.clone())
        };

        let client = azure_storage_blobs::prelude::ClientBuilder::new(&cache.storage_account, cred)
            .blob_client(&cache.storage_container, format!("{name1}/{hash}/{name2}"));

        if let Ok(props) = client.get_properties().await {
            let body = client.get().into_stream().map_ok(|r| r.data).try_flatten();

            return Ok(Response::builder()
                .header("Content-Type", "application/octet-stream")
                .header(
                    "Content-Length",
                    &props.blob.properties.content_length.to_string(),
                )
                .status(StatusCode::OK)
                .body(Body::from_stream(body))
                .context("failed to build response body")?);
        }
    }

    for server in &config.servers {
        let url = server
            .url
            .join(&format!("{name1}/{hash}/{name2}"))
            .context("failed to build request url")?;

        // Dispatch a reqwest request to upstream, and serve the response.
        // https://github.com/tokio-rs/axum/blob/680cdcba7cfa0b4fb37aba0c129ab6e4379bae3b/examples/reqwest-response/src/main.rs#L53-L68
        let req_builder = reqwest::Client::new().get(url.clone());

        // If there is a scope attached to this server, attempt to authenticate.
        let req_builder = if let Some(auth) = &server.auth {
            req_builder.bearer_auth(
                token
                    .get_token(&[&auth.scope])
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
        trace!("{}: {}", url, req.status());
        if !req.status().is_success() {
            continue;
        }

        // Forward out the full response from the upstream server, including headers and status code.
        let mut response_builder = Response::builder().status(req.status());
        *response_builder.headers_mut().unwrap() = req.headers().clone();

        // Now, we'll want to do one of two things depending on if caching is enabled:
        // If enabled, we should split the response stream into two and direct one end to the storage account,
        // and the other end to the requesting user. This also has the side effect of throttling the download
        // speed if upload is slower, but this is the cost to pay to keep things out of memory.
        //
        // If disabled, we can simply direct the response stream back out to the requester directly.
        let stream: Pin<Box<dyn Stream<Item = _> + Send>> = if let Some(cache) = &config.cache {
            let mut stream = req.bytes_stream();
            let (tx, rx) = tokio::sync::mpsc::channel(32);

            // Clone the cache into the task below.
            let cache = cache.clone();

            tokio::spawn(async move {
                let cred = if let Some(key) = &cache.key {
                    StorageCredentials::access_key(&cache.storage_account, key.clone())
                } else {
                    StorageCredentials::token_credential(token.clone())
                };

                // TODO: Check to see if another instance of the server is actively uploading the blob to the storage account.
                // If so, simply direct the response stream back to the user as if caching was disabled.
                let client =
                    azure_storage_blobs::prelude::ClientBuilder::new(&cache.storage_account, cred)
                        .blob_client(&cache.storage_container, format!("{name1}/{hash}/{name2}"));

                let mut block_list = BlockList::default();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.context("failed to read chunk")?;

                    // N.B: `block_id` must be <= 64 bytes in size.
                    // Use a randomly generated ID to avoid conflicts.
                    let block_id = format!("{}", Uuid::new_v4());

                    client
                        .put_block(block_id.clone(), chunk.clone())
                        .await
                        .unwrap();
                    block_list
                        .blocks
                        .push(BlobBlockType::new_uncommitted(block_id));

                    // Forward the data on to the original requesting client.
                    // Ignore errors since we want mirroring to continue even if the client
                    // closes their connection.
                    let _ = tx.send(Ok(chunk)).await;
                }

                client.put_block_list(block_list).await.unwrap();
                Ok::<(), anyhow::Error>(())
            });

            Box::pin(ReceiverStream::new(rx))
        } else {
            Box::pin(req.bytes_stream())
        };

        // Stream out the response from the upstream server as we receive it.
        return Ok(response_builder
            .body(Body::from_stream(stream))
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

    // Validation.
    if config.servers.is_empty() {
        anyhow::bail!("You must provide at least one upstream server in your configuration file.");
    }

    // Authenticate.
    //
    // N.B: We are _not_ going to add support for secret-based authentication.
    // It is insecure and strongly discouraged, so to encourage best practices
    // we should just not support it :)
    let token =
        azure_identity::create_default_credential().context("failed to create Azure credential")?;

    // Attempt to acquire a token upon startup just to surface any configuration errors early.
    for server in &config.servers {
        if let Some(auth) = &server.auth {
            let _tok = token
                .get_token(&[&auth.scope])
                .await
                .context("failed to get token")?;
        }
    }

    let addr = config
        .listen_address
        .unwrap_or(SocketAddr::from((Ipv4Addr::LOCALHOST, 5000)));

    let has_auth = config.servers.iter().any(|s| s.auth.is_some());
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
