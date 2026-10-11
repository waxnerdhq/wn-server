//! The HTTP server: the state, the listener and the router.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::api::{self, AppState};
use crate::config::Config;

/// A server that cannot start or that stopped with an error.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// The listener cannot bind its address.
    #[error("cannot listen on {addr}: {source}")]
    Bind { addr: SocketAddr, source: io::Error },
    /// The server stopped with an I/O error.
    #[error("the server stopped: {0}")]
    Serve(#[source] io::Error),
}

/// The address the listener binds: `server.bind` and `server.port`.
pub fn bind_addr(config: &Config) -> SocketAddr {
    SocketAddr::new(config.server.bind, config.server.port)
}

/// Binds the address of `config`, logs it and serves the API until `cancel`
/// is cancelled. Requests that are in progress at that moment are answered
/// before the function returns.
pub async fn run_server(config: Config, cancel: CancellationToken) -> Result<(), ServerError> {
    let addr = bind_addr(&config);
    let bind_error = |source| ServerError::Bind { addr, source };
    let listener = TcpListener::bind(addr).await.map_err(bind_error)?;
    // With port 0 the system chooses the port, so the log names the address
    // of the listener and not the address of the config.
    let local_addr = listener.local_addr().map_err(bind_error)?;
    tracing::info!("listening on {local_addr}");

    // The fields follow the layout rule in the module comment of `api`.
    #[rustfmt::skip]
    let state = AppState {
        config: Arc::new(config),

        // db

        // covers

        // transcoder

        // scan

        // cancel

        started_at: Instant::now(),
    };

    axum::serve(listener, api::build_router(state))
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .map_err(ServerError::Serve)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_binds_every_ipv4_address_on_port_4200() {
        assert_eq!(bind_addr(&Config::default()).to_string(), "0.0.0.0:4200");
    }

    #[test]
    fn bind_addr_follows_the_config() {
        let mut config = Config::default();
        config.server.bind = "::1".parse().expect("an address");
        config.server.port = 8080;
        assert_eq!(bind_addr(&config).to_string(), "[::1]:8080");
    }

    #[test]
    fn bind_error_names_the_address() {
        let error = ServerError::Bind {
            addr: "127.0.0.1:4200".parse().expect("an address"),
            source: io::Error::from(io::ErrorKind::AddrInUse),
        };
        assert!(
            error
                .to_string()
                .starts_with("cannot listen on 127.0.0.1:4200: "),
            "{error}"
        );
    }
}
