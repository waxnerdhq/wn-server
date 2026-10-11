//! The wn-server library. The binary and the integration tests use this crate.

pub mod api;

/// The version of this crate, taken from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn version_equals_cargo_package_version() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }
}
