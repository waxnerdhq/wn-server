//! The HTTP server: the state, the listener and the router.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::api::{self, AppState};
use crate::config::{Config, DataDir};
use crate::db::queries::users;
use crate::db::{Db, DbError};

/// What [`run_server`] logs at startup when `auth.disabled_unsafe` is `true`.
const AUTH_DISABLED_WARNING: &str = "authentication is disabled (auth.disabled_unsafe = true). \
     This is unsafe: every request acts as the owner without a password";

/// A server that cannot start or that stopped with an error.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// The data directory has no database. The server runs with a config
    /// file, so the message says that `init` keeps it.
    #[error(
        "the database {} does not exist; `wn-server init` creates it and the owner account \
         and keeps config.toml",
        .0.display()
    )]
    NoDatabase(PathBuf),
    /// The database cannot be opened or read.
    #[error("cannot open the database {}: {source}", path.display())]
    Database { path: PathBuf, source: DbError },
    /// The database has no owner account, so no request can have a user.
    #[error(
        "the database {} has no owner account; `wn-server init` creates the account \
         and keeps the database and config.toml",
        .0.display()
    )]
    NoOwner(PathBuf),
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

/// Opens the database of `data_dir`, binds the address of `config`, logs it
/// and serves the API until `cancel` is cancelled. Requests that are in
/// progress at that moment are answered before the function returns.
///
/// The database and its owner account must exist. The function does not
/// create them, and it fails before it binds the address when one is
/// missing. When `auth.disabled_unsafe` is `true`, the function logs a
/// warning first.
pub async fn run_server(
    config: Config,
    data_dir: &DataDir,
    cancel: CancellationToken,
) -> Result<(), ServerError> {
    if config.auth.disabled_unsafe {
        tracing::warn!("{AUTH_DISABLED_WARNING}");
    }
    let db = open_database(data_dir).await?;

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

        db,

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

/// Opens the database of `data_dir` and checks that it has an owner account.
///
/// `Db::open` creates a missing file. Only `init` creates the database, so a
/// missing file is an error here.
async fn open_database(data_dir: &DataDir) -> Result<Db, ServerError> {
    let path = data_dir.db_path();
    if !path.exists() {
        return Err(ServerError::NoDatabase(path));
    }
    let opened = async {
        let db = Db::open(&path)?;
        let owner = db.read(users::get_owner).await?;
        Ok::<_, DbError>((db, owner))
    };
    match opened.await {
        Ok((db, Some(_))) => Ok(db),
        Ok((_, None)) => Err(ServerError::NoOwner(path)),
        Err(source) => Err(ServerError::Database { path, source }),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    /// A data directory in a new temporary directory. The guard keeps the
    /// directory for the life of the test.
    fn data_dir() -> (TempDir, DataDir) {
        let dir = TempDir::new().expect("a temporary directory");
        let data_dir = DataDir::resolve(Some(dir.path())).expect("a data directory");
        (dir, data_dir)
    }

    /// Runs the server with the default config and a token that is already
    /// cancelled. Returns the error that ends the start.
    async fn start_error(data_dir: &DataDir) -> ServerError {
        let cancel = CancellationToken::new();
        cancel.cancel();
        run_server(Config::default(), data_dir, cancel)
            .await
            .expect_err("the server does not start")
    }

    #[tokio::test]
    async fn run_server_fails_without_a_database_and_does_not_create_one() {
        let (_dir, data_dir) = data_dir();
        let error = start_error(&data_dir).await;
        assert!(
            matches!(&error, ServerError::NoDatabase(path) if *path == data_dir.db_path()),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "the database {} does not exist; `wn-server init` creates it and the owner \
                 account and keeps config.toml",
                data_dir.db_path().display()
            )
        );
        assert!(!data_dir.db_path().exists());
    }

    #[tokio::test]
    async fn run_server_fails_without_an_owner_account() {
        let (_dir, data_dir) = data_dir();
        drop(Db::open(data_dir.db_path()).expect("create the database"));
        let error = start_error(&data_dir).await;
        assert!(
            matches!(&error, ServerError::NoOwner(path) if *path == data_dir.db_path()),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "the database {} has no owner account; `wn-server init` creates the account \
                 and keeps the database and config.toml",
                data_dir.db_path().display()
            )
        );
    }

    #[tokio::test]
    async fn run_server_names_a_database_that_it_cannot_open() {
        let (_dir, data_dir) = data_dir();
        std::fs::write(
            data_dir.db_path(),
            "this file is not a database, it is text",
        )
        .expect("write the file");
        let error = start_error(&data_dir).await;
        assert!(
            matches!(&error, ServerError::Database { path, .. } if *path == data_dir.db_path()),
            "{error:?}"
        );
        let prefix = format!(
            "cannot open the database {}: ",
            data_dir.db_path().display()
        );
        assert!(error.to_string().starts_with(&prefix), "{error}");
    }

    #[test]
    fn auth_disabled_warning_names_the_key_and_says_unsafe() {
        assert!(AUTH_DISABLED_WARNING.contains("auth.disabled_unsafe = true"));
        assert!(AUTH_DISABLED_WARNING.contains("unsafe"));
        assert!(!AUTH_DISABLED_WARNING.contains("  "));
    }

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
