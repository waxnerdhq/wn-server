//! Request authentication (`docs/design.md` section 3.7): the rules that turn
//! the parameters of a request into a user, and the [`Authed`] extractor
//! through which a handler receives that user.
//!
//! The router authenticates the request of every method that is not public
//! before it calls the handler (`api::PUBLIC_METHODS`). A handler does not
//! ask for the check, and a handler that leaves [`Authed`] out is checked all
//! the same.
//!
//! No function of this module logs a password, a token or a salt, or puts one
//! in an error. An error names a parameter and never holds a value.

use std::fmt;
use std::future::Future;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::api::envelope::ApiError;
use crate::api::params::Params;
use crate::api::{AppState, log_field};
use crate::auth::{verify_password, verify_token};
use crate::config;
use crate::db::queries::users::{self, User};
use crate::db::{Db, DbError};

/// What the error for a request without a credential names: the parameters of
/// the two mechanisms the server accepts.
const CREDENTIAL_PARAMS: &str = "p, or t and s";

/// A state that names the user of a request. The router asks it before it
/// calls the handler of a method that is not public. [`Authed`] asks it for a
/// request that the router did not authenticate.
pub trait Authenticate {
    /// The user that `params` authenticate, or the error that ends the
    /// request.
    fn authenticate(&self, params: &Params) -> impl Future<Output = Result<User, ApiError>> + Send;
}

impl Authenticate for AppState {
    async fn authenticate(&self, params: &Params) -> Result<User, ApiError> {
        authenticate(&self.config.auth, &self.db, params).await
    }
}

/// The user of a request. A handler takes it as an argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authed(pub User);

impl<S> FromRequestParts<S> for Authed
where
    S: Authenticate + Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        // The router stores the user of a request that it authenticated.
        if let Some(authed) = parts.extensions.get::<Self>() {
            return Ok(authed.clone());
        }
        // The router did not authenticate this request, because its method is
        // public. The rules apply here, so a handler that takes `Authed`
        // never runs without a user.
        let params = Params::from_request_parts(parts, state).await?;
        state.authenticate(&params).await.map(Self)
    }
}

/// The name and the credential that a request presents.
///
/// The `Debug` output leaves the credential out.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Claim<'a> {
    /// The value of `u`.
    name: &'a str,
    credential: Credential<'a>,
}

impl fmt::Debug for Claim<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Claim")
            .field("name", &self.name)
            .field("credential", &self.credential)
            .finish()
    }
}

/// The credential of a request.
///
/// The `Debug` output names the mechanism and leaves the values out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Credential<'a> {
    /// The value of `p`: the password, as it is or hex-encoded after `enc:`.
    Password(&'a str),
    /// The values of `t` and `s`.
    Token { token: &'a str, salt: &'a str },
}

impl fmt::Debug for Credential<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Credential::Password(_) => "Password(..)",
            Credential::Token { .. } => "Token { .. }",
        })
    }
}

/// Why a request gets no user.
enum Failure {
    /// The request breaks a rule of section 3.7. The error is the answer.
    Rejected(ApiError),
    /// The accounts cannot be read.
    Database(DbError),
    /// Authentication is disabled and the database has no owner account.
    NoOwner,
}

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Self {
        Self::Rejected(error)
    }
}

impl From<DbError> for Failure {
    fn from(error: DbError) -> Self {
        Self::Database(error)
    }
}

/// Returns the user that `params` authenticate, by the rules of
/// `docs/design.md` section 3.7.
///
/// - `v` and `c` are required. The error has code 10 and names the parameter.
/// - With `auth.disabled_unsafe` the user is the owner, and no other
///   parameter is read.
/// - `apiKey` alone is error 42. `apiKey` with `u`, `p`, `t` or `s` is
///   error 43. `p` with `t` or `s` is error 43.
/// - `u` is required, and so is `p`, or `t` with `s`. The error has code 10.
/// - `p` is compared with the stored password (`auth::verify_password`), and
///   `t` with the token of the stored password for `s`
///   (`auth::verify_token`). An unknown name and a wrong credential are both
///   error 40.
///
/// A request that breaks a rule is logged at `info` with the values of `u`
/// and `c`, each cut to 64 characters, and with the error. The values of `p`,
/// `t`, `s` and `apiKey` are not logged.
pub async fn authenticate(auth: &config::Auth, db: &Db, params: &Params) -> Result<User, ApiError> {
    match identify(auth, db, params).await {
        Ok(user) => Ok(user),
        Err(Failure::Rejected(error)) => {
            // tracing writes a string field in quotes and escapes control
            // characters, so a line break in a value does not start a log
            // line.
            tracing::info!(
                user = params.opt_str("u").map(log_field),
                client = params.opt_str("c").map(log_field),
                code = error.code(),
                "authentication failed: {error}"
            );
            Err(error)
        }
        Err(Failure::Database(error)) => {
            tracing::error!(%error, "cannot read the user accounts");
            Err(ApiError::Other(
                "the server cannot read its user accounts".to_owned(),
            ))
        }
        Err(Failure::NoOwner) => {
            tracing::error!("authentication is disabled and the database has no owner account");
            Err(ApiError::Other(
                "the server has no owner account".to_owned(),
            ))
        }
    }
}

/// The steps of [`authenticate`], without the logging.
async fn identify(auth: &config::Auth, db: &Db, params: &Params) -> Result<User, Failure> {
    params.req_str("v")?;
    params.req_str("c")?;
    if auth.disabled_unsafe {
        let owner = db.read(users::get_owner).await?;
        return owner.map(|row| row.user).ok_or(Failure::NoOwner);
    }

    let claim = claim(params)?;
    let name = claim.name.to_owned();
    let row = db.read(move |conn| users::get_by_name(conn, &name)).await?;
    // The credential of an unknown name is checked against an empty password,
    // so that an unknown name and a wrong credential take the same steps. The
    // result counts only when the account exists.
    let stored = row.as_ref().map_or("", |row| row.password.as_str());
    let matches = match claim.credential {
        Credential::Password(given) => verify_password(stored, given),
        Credential::Token { token, salt } => verify_token(stored, salt, token),
    };
    match row {
        Some(row) if matches => Ok(row.user),
        _ => Err(ApiError::BadCredentials.into()),
    }
}

/// Reads the name and the credential of a request. No account is read, so
/// the result does not depend on the accounts of the server.
///
/// An empty value is a value: `p=` is a password, and `apiKey=` is an API
/// key.
fn claim(params: &Params) -> Result<Claim<'_>, ApiError> {
    let name = params.opt_str("u");
    let password = params.opt_str("p");
    let token = params.opt_str("t");
    let salt = params.opt_str("s");

    // The server has no API keys. A key next to a parameter of another
    // mechanism is the conflict that the OpenSubsonic API assigns error 43.
    if params.opt_str("apiKey").is_some() {
        let mixed = name.is_some() || password.is_some() || token.is_some() || salt.is_some();
        return Err(if mixed {
            ApiError::AuthConflict
        } else {
            ApiError::AuthUnsupported
        });
    }
    if password.is_some() && (token.is_some() || salt.is_some()) {
        return Err(ApiError::AuthConflict);
    }

    let name = name.ok_or_else(|| missing("u"))?;
    let credential = match (password, token, salt) {
        (Some(password), _, _) => Credential::Password(password),
        (None, Some(token), Some(salt)) => Credential::Token { token, salt },
        (None, Some(_), None) => return Err(missing("s")),
        (None, None, Some(_)) => return Err(missing("t")),
        (None, None, None) => return Err(missing(CREDENTIAL_PARAMS)),
    };
    Ok(Claim { name, credential })
}

/// The error for the absent parameter `name`. `name` is a name that this
/// module writes, never a value of the request.
fn missing(name: &str) -> ApiError {
    ApiError::MissingParam(name.to_owned())
}

#[cfg(test)]
mod tests {
    use axum::http::Request;

    use super::*;

    /// The example of the Subsonic API documentation: the token of the
    /// password `sesame` for the salt `c19b2d`.
    const SESAME_TOKEN: &str = "26719a1196d2a940705a59634eb18eab";

    /// The MD5 digest of the salt `c19b2d` alone, which is the token of an
    /// empty password for that salt.
    const EMPTY_PASSWORD_TOKEN: &str = "ff0792d8c345da74a596a357fe35bd90";

    /// The parameters of `query`, with `v` and `c`.
    fn params(query: &str) -> Params {
        Params::from_query(&format!("v=1.16.1&c=test&{query}"))
    }

    fn enabled() -> config::Auth {
        config::Auth {
            disabled_unsafe: false,
        }
    }

    fn disabled() -> config::Auth {
        config::Auth {
            disabled_unsafe: true,
        }
    }

    fn owner() -> User {
        User {
            id: 1,
            name: "owner".to_owned(),
            role: "owner".to_owned(),
        }
    }

    /// A memory database with the owner `owner` and the password `sesame`.
    async fn db() -> Db {
        let db = Db::open_memory().expect("a memory database");
        let inserted = db
            .write(|conn| users::insert_owner(conn, "owner", "sesame"))
            .await
            .expect("insert the owner");
        assert_eq!(inserted, Some(owner()));
        db
    }

    /// The code and the message of the error that `query` gets.
    async fn rejected(auth: &config::Auth, db: &Db, query: &str) -> (u32, String) {
        let error = authenticate(auth, db, &Params::from_query(query))
            .await
            .expect_err(query);
        (error.code(), error.message())
    }

    fn code_10(name: &str) -> (u32, String) {
        (10, format!("required parameter is missing: {name}"))
    }

    fn code_40() -> (u32, String) {
        (40, "wrong username or password".to_owned())
    }

    #[test]
    fn claim_reads_a_password() {
        assert_eq!(
            claim(&params("u=owner&p=sesame")),
            Ok(Claim {
                name: "owner",
                credential: Credential::Password("sesame"),
            })
        );
        // The `enc:` form is decoded when the password is compared.
        assert_eq!(
            claim(&params("u=owner&p=enc:736573616d65")),
            Ok(Claim {
                name: "owner",
                credential: Credential::Password("enc:736573616d65"),
            })
        );
    }

    #[test]
    fn claim_reads_a_token_and_its_salt() {
        assert_eq!(
            claim(&params("u=owner&t=abc&s=def")),
            Ok(Claim {
                name: "owner",
                credential: Credential::Token {
                    token: "abc",
                    salt: "def",
                },
            })
        );
    }

    #[test]
    fn claim_takes_an_empty_value_as_a_value() {
        assert_eq!(
            claim(&params("u=&p=")),
            Ok(Claim {
                name: "",
                credential: Credential::Password(""),
            })
        );
        assert_eq!(
            claim(&params("u=owner&t=&s=")),
            Ok(Claim {
                name: "owner",
                credential: Credential::Token {
                    token: "",
                    salt: "",
                },
            })
        );
    }

    #[test]
    fn claim_with_an_api_key_alone_is_error_42() {
        for query in ["apiKey=k", "apiKey="] {
            assert_eq!(
                claim(&params(query)),
                Err(ApiError::AuthUnsupported),
                "{query}"
            );
        }
        assert_eq!(ApiError::AuthUnsupported.code(), 42);
    }

    #[test]
    fn claim_with_an_api_key_and_another_mechanism_is_error_43() {
        for query in [
            "apiKey=k&u=owner",
            "apiKey=k&u=",
            "apiKey=k&p=sesame",
            "apiKey=k&t=abc",
            "apiKey=k&s=def",
            "apiKey=k&u=owner&p=sesame",
            "apiKey=k&u=owner&t=abc&s=def",
        ] {
            assert_eq!(
                claim(&params(query)),
                Err(ApiError::AuthConflict),
                "{query}"
            );
        }
        assert_eq!(ApiError::AuthConflict.code(), 43);
    }

    #[test]
    fn claim_with_a_password_and_a_token_parameter_is_error_43() {
        for query in [
            "u=owner&p=sesame&t=abc",
            "u=owner&p=sesame&s=def",
            "u=owner&p=sesame&t=abc&s=def",
            "u=owner&p=&t=",
            // The conflict does not depend on the name.
            "p=sesame&t=abc",
        ] {
            assert_eq!(
                claim(&params(query)),
                Err(ApiError::AuthConflict),
                "{query}"
            );
        }
    }

    #[test]
    fn claim_without_a_name_or_a_credential_is_error_10_naming_the_parameter() {
        for (query, name) in [
            ("", "u"),
            ("p=sesame", "u"),
            ("t=abc&s=def", "u"),
            ("u=owner", CREDENTIAL_PARAMS),
            ("u=owner&t=abc", "s"),
            ("u=owner&s=def", "t"),
        ] {
            assert_eq!(
                claim(&params(query)),
                Err(ApiError::MissingParam(name.to_owned())),
                "{query}"
            );
        }
        assert_eq!(missing("u").code(), 10);
    }

    #[test]
    fn claim_reads_the_first_value_of_a_parameter() {
        let mut params = params("u=owner&p=first");
        params.append_form(b"u=other&p=second");
        assert_eq!(
            claim(&params),
            Ok(Claim {
                name: "owner",
                credential: Credential::Password("first"),
            })
        );
    }

    #[test]
    fn debug_output_of_a_claim_leaves_the_credential_out() {
        let params = params("u=owner&p=secret-password");
        let text = format!("{:?}", claim(&params));
        assert_eq!(
            text,
            r#"Ok(Claim { name: "owner", credential: Password(..) })"#
        );

        let params = self::params("u=owner&t=secret-token&s=secret-salt");
        let text = format!("{:?}", claim(&params));
        assert_eq!(
            text,
            r#"Ok(Claim { name: "owner", credential: Token { .. } })"#
        );
    }

    #[tokio::test]
    async fn authenticate_accepts_the_three_credential_forms() {
        let db = db().await;
        for query in [
            "u=owner&p=sesame".to_owned(),
            "u=owner&p=enc:736573616d65".to_owned(),
            format!("u=owner&t={SESAME_TOKEN}&s=c19b2d"),
        ] {
            let user = authenticate(&enabled(), &db, &params(&query)).await;
            assert_eq!(user, Ok(owner()), "{query}");
        }
    }

    #[tokio::test]
    async fn authenticate_finds_the_name_without_regard_to_ascii_case() {
        let db = db().await;
        let user = authenticate(&enabled(), &db, &params("u=OWNER&p=sesame")).await;
        assert_eq!(user, Ok(owner()));
    }

    #[tokio::test]
    async fn authenticate_rejects_a_wrong_credential_with_error_40() {
        let db = db().await;
        let upper = SESAME_TOKEN.to_uppercase();
        for query in [
            "u=owner&p=wrong".to_owned(),
            "u=owner&p=".to_owned(),
            "u=owner&p=Sesame".to_owned(),
            // The hex encoding of `sesamf`.
            "u=owner&p=enc:736573616d66".to_owned(),
            // Not hex.
            "u=owner&p=enc:sesame".to_owned(),
            format!("u=owner&t={SESAME_TOKEN}&s=c19b2e"),
            format!("u=owner&t={upper}&s=c19b2d"),
            "u=owner&t=&s=".to_owned(),
            // The password is not a token.
            "u=owner&t=sesame&s=c19b2d".to_owned(),
        ] {
            let query = format!("v=1.16.1&c=test&{query}");
            assert_eq!(
                rejected(&enabled(), &db, &query).await,
                code_40(),
                "{query}"
            );
        }
    }

    #[tokio::test]
    async fn authenticate_rejects_an_unknown_name_with_error_40() {
        let db = db().await;
        for query in [
            "u=nobody&p=sesame".to_owned(),
            format!("u=nobody&t={SESAME_TOKEN}&s=c19b2d"),
            "u=&p=sesame".to_owned(),
            // The credentials that match an empty password.
            "u=nobody&p=".to_owned(),
            "u=nobody&p=enc:".to_owned(),
            format!("u=nobody&t={EMPTY_PASSWORD_TOKEN}&s=c19b2d"),
        ] {
            let query = format!("v=1.16.1&c=test&{query}");
            assert_eq!(
                rejected(&enabled(), &db, &query).await,
                code_40(),
                "{query}"
            );
        }
    }

    #[tokio::test]
    async fn the_token_of_an_empty_password_matches_an_empty_password_only() {
        // The test above depends on this token being the one that the check
        // of an unknown name computes.
        assert!(verify_token("", "c19b2d", EMPTY_PASSWORD_TOKEN));
        assert!(!verify_token("sesame", "c19b2d", EMPTY_PASSWORD_TOKEN));
    }

    #[tokio::test]
    async fn authenticate_requires_v_and_c_before_it_reads_a_credential() {
        let db = db().await;
        for auth in [enabled(), disabled()] {
            for (query, name) in [
                ("", "v"),
                ("u=owner&p=sesame", "v"),
                ("c=test&u=owner&p=sesame", "v"),
                ("v=1.16.1&u=owner&p=sesame", "c"),
                // A wrong credential and an API key do not change the answer.
                ("c=test&u=owner&p=wrong", "v"),
                ("v=1.16.1&apiKey=k", "c"),
            ] {
                assert_eq!(
                    rejected(&auth, &db, query).await,
                    code_10(name),
                    "{query} {auth:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn authenticate_does_not_compare_the_value_of_v() {
        let db = db().await;
        for query in [
            "v=&c=&u=owner&p=sesame",
            "v=0.0.1&c=x&u=owner&p=sesame",
            "v=99&c=x&u=owner&p=sesame",
        ] {
            let user = authenticate(&enabled(), &db, &Params::from_query(query)).await;
            assert_eq!(user, Ok(owner()), "{query}");
        }
    }

    #[tokio::test]
    async fn authenticate_reports_the_mechanism_errors_for_any_account() {
        let db = db().await;
        for (query, code) in [
            ("apiKey=k", 42),
            ("apiKey=k&u=owner", 43),
            ("apiKey=k&u=nobody", 43),
            ("u=owner&p=sesame&t=abc", 43),
            ("u=nobody&p=sesame&t=abc&s=def", 43),
        ] {
            let query = format!("v=1.16.1&c=test&{query}");
            let (found, _) = rejected(&enabled(), &db, &query).await;
            assert_eq!(found, code, "{query}");
        }
    }

    #[tokio::test]
    async fn authenticate_without_a_credential_is_error_10() {
        let db = db().await;
        for (query, name) in [
            ("", "u"),
            ("u=owner", CREDENTIAL_PARAMS),
            ("u=nobody", CREDENTIAL_PARAMS),
            ("u=owner&t=abc", "s"),
            ("u=owner&s=def", "t"),
        ] {
            let query = format!("v=1.16.1&c=test&{query}");
            assert_eq!(
                rejected(&enabled(), &db, &query).await,
                code_10(name),
                "{query}"
            );
        }
    }

    #[tokio::test]
    async fn disabled_auth_returns_the_owner_without_reading_a_credential() {
        let db = db().await;
        for query in [
            "",
            "u=owner&p=sesame",
            "u=owner&p=wrong",
            "u=nobody&p=wrong",
            "u=owner",
            "apiKey=k",
            "apiKey=k&u=owner",
            "u=owner&p=sesame&t=abc&s=def",
        ] {
            let user = authenticate(&disabled(), &db, &params(query)).await;
            assert_eq!(user, Ok(owner()), "{query}");
        }
    }

    #[tokio::test]
    async fn disabled_auth_without_an_owner_account_is_error_0() {
        let db = Db::open_memory().expect("a memory database");
        assert_eq!(
            rejected(&disabled(), &db, "v=1.16.1&c=test").await,
            (0, "the server has no owner account".to_owned())
        );
    }

    #[tokio::test]
    async fn no_account_authenticates_without_an_owner_account() {
        let db = Db::open_memory().expect("a memory database");
        for query in [
            "u=owner&p=sesame".to_owned(),
            "u=owner&p=".to_owned(),
            "u=&p=".to_owned(),
            format!("u=owner&t={EMPTY_PASSWORD_TOKEN}&s=c19b2d"),
        ] {
            let query = format!("v=1.16.1&c=test&{query}");
            assert_eq!(
                rejected(&enabled(), &db, &query).await,
                code_40(),
                "{query}"
            );
        }
    }

    #[tokio::test]
    async fn a_database_failure_is_error_0_without_the_credential() {
        let db = db().await;
        db.write(|conn| Ok(conn.execute_batch("DROP TABLE users")?))
            .await
            .expect("drop the table");
        for auth in [enabled(), disabled()] {
            let (code, message) = rejected(
                &auth,
                &db,
                "v=1.16.1&c=test&u=owner&p=failure-secret-password",
            )
            .await;
            assert_eq!(code, 0, "{auth:?}");
            assert_eq!(
                message, "the server cannot read its user accounts",
                "{auth:?}"
            );
        }
    }

    #[tokio::test]
    async fn no_error_holds_a_value_of_the_request() {
        let db = db().await;
        let secrets = ["zq-name", "zq-password", "zq-token", "zq-salt", "zq-key"];
        for query in [
            "",
            "v=1.16.1",
            "v=1.16.1&c=zq-client",
            "v=1.16.1&c=zq-client&u=zq-name",
            "v=1.16.1&c=zq-client&u=zq-name&p=zq-password",
            "v=1.16.1&c=zq-client&u=zq-name&p=enc:zq-password",
            "v=1.16.1&c=zq-client&u=zq-name&t=zq-token",
            "v=1.16.1&c=zq-client&u=zq-name&s=zq-salt",
            "v=1.16.1&c=zq-client&u=zq-name&t=zq-token&s=zq-salt",
            "v=1.16.1&c=zq-client&u=zq-name&p=zq-password&t=zq-token&s=zq-salt",
            "v=1.16.1&c=zq-client&apiKey=zq-key",
            "v=1.16.1&c=zq-client&apiKey=zq-key&u=zq-name",
            "v=1.16.1&c=zq-client&u=owner&p=zq-password",
            "v=1.16.1&c=zq-client&u=owner&t=zq-token&s=zq-salt",
        ] {
            let (_, message) = rejected(&enabled(), &db, query).await;
            for secret in secrets {
                assert!(!message.contains(secret), "{query}: {message}");
            }
            assert!(!message.contains("zq-client"), "{query}: {message}");
        }
    }

    /// A state with the accounts of a database, as the server authenticates.
    struct Accounts(Db);

    impl Authenticate for Accounts {
        async fn authenticate(&self, params: &Params) -> Result<User, ApiError> {
            authenticate(&enabled(), &self.0, params).await
        }
    }

    /// A state that authenticates no request.
    struct Nobody;

    impl Authenticate for Nobody {
        async fn authenticate(&self, _params: &Params) -> Result<User, ApiError> {
            Err(ApiError::BadCredentials)
        }
    }

    /// The parts of a request that carries `params` as the router stores
    /// them.
    fn parts_with(params: Option<Params>) -> Parts {
        let (mut parts, ()) = Request::new(()).into_parts();
        if let Some(params) = params {
            parts.extensions.insert(params);
        }
        parts
    }

    #[tokio::test]
    async fn authed_is_the_user_that_the_router_stored() {
        let mut parts = parts_with(Some(params("")));
        parts.extensions.insert(Authed(owner()));
        // The state is not asked again.
        let authed = Authed::from_request_parts(&mut parts, &Nobody).await;
        assert_eq!(authed, Ok(Authed(owner())));
    }

    #[tokio::test]
    async fn authed_authenticates_a_request_that_the_router_did_not() {
        let accounts = Accounts(db().await);

        let mut parts = parts_with(Some(params("u=owner&p=sesame")));
        let authed = Authed::from_request_parts(&mut parts, &accounts).await;
        assert_eq!(authed, Ok(Authed(owner())));

        let mut parts = parts_with(Some(params("u=owner&p=wrong")));
        let authed = Authed::from_request_parts(&mut parts, &accounts).await;
        assert_eq!(authed, Err(ApiError::BadCredentials));

        let mut parts = parts_with(Some(params("")));
        let authed = Authed::from_request_parts(&mut parts, &accounts).await;
        assert_eq!(authed, Err(ApiError::MissingParam("u".to_owned())));
    }

    #[tokio::test]
    async fn authed_fails_for_a_request_without_parsed_parameters() {
        let accounts = Accounts(db().await);
        let mut parts = parts_with(None);
        let error = Authed::from_request_parts(&mut parts, &accounts)
            .await
            .expect_err("the request has no parameters");
        assert_eq!(error.code(), 0);
    }
}
