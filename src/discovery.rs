//! The mDNS/DNS-SD advertisement of the server on the local network
//! (`docs/design.md` section 3.12).
//!
//! The first-party mobile app browses [`SERVICE_TYPE`] and reads the TXT
//! record of each instance it finds. The service type and the keys and values
//! that [`TxtRecord::build`] returns are a contract with that app, and the
//! unit tests of this module pin them. The functions that build the record use
//! no network. [`Advertiser`] is the only part of the module that does.
//!
//! The server advertises under a host name of its own, which [`host_name`]
//! builds from the instance ID. Nothing in this module reads the host name of
//! the machine, and the server publishes no record for it. The responder of
//! the operating system owns that name. A second responder that publishes
//! records for it makes the operating system rename the machine.
//!
//! The `mdns-sd` calls used here are `ServiceDaemon::new`,
//! `disable_interface`, `monitor`, `register`, `unregister`, `shutdown`,
//! `ServiceInfo::new` and `enable_addr_auto`. The crate documents support for
//! Linux, Windows and macOS and gates none of these calls on the OS, so this
//! module has no `#[cfg]`.

use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use mdns_sd::{
    DaemonEvent, IfKind, Receiver, RecvTimeoutError, ServiceDaemon, ServiceInfo, UnregisterStatus,
};
use thiserror::Error;

use crate::config::{Config, LABEL_MAX_BYTES};

/// The DNS-SD service type that the server registers and the mobile app
/// browses.
pub const SERVICE_TYPE: &str = "_waxnerd._tcp.local.";

/// The start of the label of the server's mDNS host name. The first
/// [`HOST_ID_DIGITS`] hex digits of the instance ID follow it.
const HOST_LABEL_PREFIX: &str = "wn-server-";

/// The number of hex digits of the instance ID in the server's mDNS host
/// name.
const HOST_ID_DIGITS: usize = 8;

/// The time the reporting thread waits for the first announcement. The daemon
/// probes for its names for 750 ms to one second before it announces them
/// (RFC 6762 section 8.1), and a name conflict adds about a second for each
/// new name the daemon tries.
const ANNOUNCE_WAIT: Duration = Duration::from_secs(10);

/// The time [`Advertiser::stop`] waits for one reply of the daemon.
const REPLY_WAIT: Duration = Duration::from_secs(1);

/// The time [`Advertiser::stop`] waits between `unregister` and `shutdown`.
/// The daemon sends the goodbye packet a second time 120 ms after
/// `unregister`, and `shutdown` discards a repeat that is still pending.
const GOODBYE_REPEAT_WAIT: Duration = Duration::from_millis(200);

/// An advertisement that cannot start.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// `instance.id` cannot be read.
    #[error("cannot read {}: {source}", path.display())]
    ReadInstanceId { path: PathBuf, source: io::Error },
    /// `instance.id` does not hold a UUID in the form that `init` writes.
    #[error(
        "{} does not hold a UUID in lower-case hex groups of 8, 4, 4, 4 and 12 characters",
        .0.display()
    )]
    InstanceId(PathBuf),
    /// The text given as the instance ID is not a UUID in the form that `init`
    /// writes, so no host name is built from it.
    #[error(
        "the instance ID is not a UUID in lower-case hex groups of 8, 4, 4, 4 and 12 characters"
    )]
    NotAnInstanceId,
    /// A name does not fit in one DNS label.
    #[error("{what} is {bytes} bytes long; a DNS-SD name holds 1 to {LABEL_MAX_BYTES} bytes")]
    Label { what: &'static str, bytes: usize },
    /// The thread that reports the first announcement cannot start.
    #[error("cannot start the thread that reports the announcement: {0}")]
    Thread(io::Error),
    /// `mdns-sd` rejected the registration or cannot start its daemon.
    #[error(transparent)]
    Mdns(#[from] mdns_sd::Error),
}

/// The TXT record of the service instance.
pub struct TxtRecord;

impl TxtRecord {
    /// Returns the keys and values of the TXT record, in the order the record
    /// holds them. DNS-SD asks for the version key first.
    ///
    /// | Key | Value |
    /// |---|---|
    /// | `txtvers` | `1`, the version of this set of keys |
    /// | `api` | `opensubsonic` |
    /// | `path` | `/rest`, the path prefix of the API |
    /// | `id` | `instance_id`, the UUID in `instance.id`: 36 characters |
    /// | `name` | `server.name` |
    /// | `auth` | `password`, or `none` when `auth.disabled_unsafe` is set |
    /// | `tls` | `0`; the server speaks plain HTTP |
    /// | `ver` | `version` unchanged, the Cargo version of the server |
    ///
    /// The record holds no other key. Every device on the local network can
    /// read it, so it carries no credential, no account name and no folder
    /// path. `docs/design.md` section 3.12 states the format of each value
    /// and what a client does with it.
    pub fn build(config: &Config, version: &str, instance_id: &str) -> Vec<(String, String)> {
        let auth = if config.auth.disabled_unsafe {
            "none"
        } else {
            "password"
        };
        [
            ("txtvers", "1"),
            ("api", "opensubsonic"),
            ("path", "/rest"),
            ("id", instance_id),
            ("name", config.server.name.as_str()),
            ("auth", auth),
            ("tls", "0"),
            ("ver", version),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
    }
}

/// Returns the interface kinds that the mDNS daemon must not use for a server
/// that binds `bind`.
///
/// A server that binds an IPv4 address does not listen on IPv6, so the IPv6
/// interfaces are disabled and no AAAA record is published. A server that
/// binds an IPv6 address keeps the interfaces of both families. [`addresses`]
/// then decides which addresses the daemon publishes on them.
///
/// Windows: a listener on `::` accepts IPv6 connections only, because Windows
/// sets `IPV6_V6ONLY` on a new socket. Linux and macOS accept both families
/// on that listener. The result for `::` is the same on every OS, so on
/// Windows the A records name addresses that the listener does not accept.
pub fn interface_kinds(bind: IpAddr) -> Vec<IfKind> {
    match bind {
        IpAddr::V4(_) => vec![IfKind::IPv6],
        IpAddr::V6(_) => Vec::new(),
    }
}

/// Returns the addresses that the registration names for a server that binds
/// `bind`.
///
/// A server that binds `0.0.0.0` or `::` listens on every address of the
/// machine. The result is empty then: the daemon publishes the addresses of
/// the interfaces it uses and follows their changes. A server that binds one
/// address listens on that address only, so the result is that address. The
/// daemon announces an address on the interface whose subnet holds it and on
/// no other. A server on `127.0.0.1` is therefore announced on the loopback
/// interface only, and a server on one LAN address is not announced on
/// another link.
pub fn addresses(bind: IpAddr) -> Vec<IpAddr> {
    if bind.is_unspecified() {
        Vec::new()
    } else {
        vec![bind]
    }
}

/// Returns the mDNS host name of the server whose instance ID is
/// `instance_id`: `wn-server-<hex>.local.`, where `<hex>` is the first 8 hex
/// digits of the ID. The name is the SRV target of the registration and the
/// name of the A and AAAA records that the server publishes.
///
/// The function reads `instance_id` and nothing else, so the result is never
/// taken from the host name of the machine. An `instance_id` that is not a
/// UUID in the form that `init` writes is an error. Every name that the
/// function returns is therefore `wn-server-`, 8 lower-case hex digits and
/// `.local.`: one label of 18 bytes in the local domain.
pub fn host_name(instance_id: &str) -> Result<String, DiscoveryError> {
    match instance_id.get(..HOST_ID_DIGITS) {
        Some(digits) if is_uuid(instance_id) => Ok(format!("{HOST_LABEL_PREFIX}{digits}.local.")),
        _ => Err(DiscoveryError::NotAnInstanceId),
    }
}

/// Checks that `label` fits in one DNS label. `what` names the value in the
/// error. `mdns-sd` accepts a longer name at registration and then leaves
/// every record that holds it out of its packets, so the check runs before
/// the registration.
///
/// `Config::load` rejects a `server.name` that is not 1 to 63 bytes long, so
/// `run` does not get here with one. The check of the name stays for a
/// `Config` that was not loaded from a file.
fn check_label(what: &'static str, label: &str) -> Result<(), DiscoveryError> {
    if label.is_empty() || label.len() > LABEL_MAX_BYTES {
        return Err(DiscoveryError::Label {
            what,
            bytes: label.len(),
        });
    }
    Ok(())
}

/// Builds the registration of the server: the service type
/// [`SERVICE_TYPE`], the instance name `server.name`, the host name that
/// [`host_name`] returns for `instance_id`, the port `server.port`, the TXT
/// record `txt` and the addresses that [`addresses`] returns for
/// `server.bind`. The function uses no network.
///
/// The function takes no host name of the machine. The host name of the
/// registration comes from `instance_id` alone, so neither `server.name` nor
/// `server.bind` changes it.
///
/// A registration without an address lets the daemon choose the addresses.
/// A registration with an address keeps that address.
fn service_info(
    config: &Config,
    instance_id: &str,
    txt: &[(String, String)],
) -> Result<ServiceInfo, DiscoveryError> {
    let name = config.server.name.as_str();
    check_label("server.name", name)?;
    let host = host_name(instance_id)?;
    let addresses = addresses(config.server.bind);
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        name,
        &host,
        addresses.as_slice(),
        config.server.port,
        txt,
    )?;
    if addresses.is_empty() {
        Ok(info.enable_addr_auto())
    } else {
        Ok(info)
    }
}

/// A running advertisement: the thread of the mDNS daemon and the instance
/// that it registered. Dropping the value withdraws the advertisement.
pub struct Advertiser {
    daemon: ServiceDaemon,
    /// The full name of the instance, `<server.name>._waxnerd._tcp.local.`.
    fullname: String,
    /// Whether the daemon has announced the instance. The reporting thread
    /// sets it.
    announced: Arc<AtomicBool>,
    /// The moment before the registration was sent to the daemon.
    registered_at: Instant,
}

impl Advertiser {
    /// Starts the mDNS daemon and registers the server with the TXT record
    /// `txt` under the host name that [`host_name`] returns for
    /// `instance_id`. The daemon runs on its own thread, disables the
    /// interface kinds that [`interface_kinds`] returns for `server.bind`,
    /// probes for the names and then announces the instance.
    ///
    /// An error means that no daemon is left running. The daemon opens its
    /// multicast sockets on its own thread and reports a socket that it
    /// cannot open in its debug log only. A second thread therefore waits for
    /// the first announcement and logs it, or logs one warning when the
    /// daemon announces nothing within ten seconds.
    pub fn start(
        config: &Config,
        instance_id: &str,
        txt: &[(String, String)],
    ) -> Result<Self, DiscoveryError> {
        let info = service_info(config, instance_id, txt)?;
        let fullname = info.get_fullname().to_owned();
        // An error after this statement drops `advertiser`, which shuts the
        // daemon down.
        let advertiser = Self {
            daemon: ServiceDaemon::new()?,
            fullname,
            announced: Arc::new(AtomicBool::new(false)),
            registered_at: Instant::now(),
        };
        let disabled = interface_kinds(config.server.bind);
        if !disabled.is_empty() {
            advertiser.daemon.disable_interface(disabled)?;
        }
        let events = advertiser.daemon.monitor()?;
        advertiser.daemon.register(info)?;

        let fullname = advertiser.fullname.clone();
        let announced = Arc::clone(&advertiser.announced);
        thread::Builder::new()
            .name("mdns-report".to_owned())
            .spawn(move || report(&fullname, &events, &announced))
            .map_err(DiscoveryError::Thread)?;
        Ok(advertiser)
    }

    /// Withdraws the advertisement: unregisters the instance, which sends a
    /// goodbye packet on every interface, and shuts the daemon down. The call
    /// blocks for about 200 ms, and for up to one second longer for each of
    /// the two replies when the daemon does not answer.
    ///
    /// A stop before the first announcement sends no goodbye packet and does
    /// not wait, because there is nothing to withdraw.
    ///
    /// Dropping the value has the same effect. This method names the step.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        let announced = self.announced.load(Ordering::Acquire);
        if sends_goodbye(announced, self.registered_at.elapsed()) {
            match self.daemon.unregister(&self.fullname) {
                Ok(reply) => {
                    if let Ok(UnregisterStatus::OK) = reply.recv_timeout(REPLY_WAIT) {
                        thread::sleep(GOODBYE_REPEAT_WAIT);
                    }
                }
                Err(error) => tracing::debug!("cannot unregister {}: {error}", self.fullname),
            }
        } else if let Err(error) = self.daemon.disable_interface(IfKind::All) {
            // The daemon sends a goodbye packet for every registered service
            // when it shuts down. After `disable_interface` it has no
            // interface to send one on.
            tracing::debug!("cannot disable the mDNS interfaces: {error}");
        }
        match self.daemon.shutdown() {
            Ok(reply) => {
                let _ = reply.recv_timeout(REPLY_WAIT);
            }
            Err(error) => tracing::debug!("cannot shut the mDNS daemon down: {error}"),
        }
    }
}

/// Whether a stop withdraws the instance with a goodbye packet. `announced`
/// says whether the reporting thread saw an announcement, and
/// `since_registration` is the time since the registration.
///
/// An instance that was not announced has nothing to withdraw. A goodbye
/// packet would name records that another running server with the same
/// instance name may own, and clients would drop that server. That is the
/// case when `run` is started twice and the second server cannot bind its
/// port.
///
/// The reporting thread watches for [`ANNOUNCE_WAIT`]. An announcement after
/// that time is not seen, so a stop after it always sends the goodbye packet.
fn sends_goodbye(announced: bool, since_registration: Duration) -> bool {
    announced || since_registration >= ANNOUNCE_WAIT
}

/// What the daemon reported about a registration.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The daemon announced the instance under this full name. The name
    /// differs from the registered one when the daemon renamed the instance
    /// after a name conflict.
    Announced(String),
    /// The daemon reported an error.
    Failed(String),
    /// The daemon announced nothing before the wait ended.
    Silent,
    /// The daemon shut down before it announced anything.
    Stopped,
}

/// Reads daemon events from `next` until one decides the outcome of the
/// registration. `next` returns the next event, or an error when the wait
/// ended or the daemon shut down.
fn outcome(mut next: impl FnMut() -> Result<DaemonEvent, RecvTimeoutError>) -> Outcome {
    loop {
        match next() {
            Ok(DaemonEvent::Announce(fullname, _)) => return Outcome::Announced(fullname),
            Ok(DaemonEvent::Error(error)) => return Outcome::Failed(error.to_string()),
            // Address changes and answers to queries do not decide it.
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return Outcome::Silent,
            Err(RecvTimeoutError::Disconnected) => return Outcome::Stopped,
        }
    }
}

/// Logs the outcome of the registration of `fullname` once: the first
/// announcement, an error of the daemon, or the absence of an announcement
/// after [`ANNOUNCE_WAIT`]. The function sets `announced` when the daemon
/// announced the instance, and returns after the log line.
fn report(fullname: &str, events: &Receiver<DaemonEvent>, announced: &AtomicBool) {
    let deadline = Instant::now() + ANNOUNCE_WAIT;
    match outcome(|| events.recv_deadline(deadline)) {
        Outcome::Announced(name) => {
            announced.store(true, Ordering::Release);
            tracing::info!("advertising {name} over mDNS");
        }
        Outcome::Failed(error) => tracing::warn!("mDNS advertisement is off: {error}"),
        Outcome::Silent => tracing::warn!(
            "the mDNS daemon has not announced {fullname} after {} s, so clients may not find \
             the server; the log filter `mdns_sd=debug` shows the messages of the daemon",
            ANNOUNCE_WAIT.as_secs()
        ),
        Outcome::Stopped => {}
    }
}

/// Reads the instance ID from the file at `path`, which `init` writes as
/// `instance.id`. The ID is the content of the file without the surrounding
/// white space, and it must have the form that `init` writes: lower-case hex
/// digits in groups of 8, 4, 4, 4 and 12, separated by hyphens.
pub fn read_instance_id(path: &Path) -> Result<String, DiscoveryError> {
    let text = fs::read_to_string(path).map_err(|source| DiscoveryError::ReadInstanceId {
        path: path.to_path_buf(),
        source,
    })?;
    let id = text.trim();
    if !is_uuid(id) {
        return Err(DiscoveryError::InstanceId(path.to_path_buf()));
    }
    Ok(id.to_owned())
}

/// Whether `text` is lower-case hex digits in groups of 8, 4, 4, 4 and 12,
/// separated by hyphens.
fn is_uuid(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    groups.iter().map(|group| group.len()).eq([8, 4, 4, 4, 12])
        && groups.iter().all(|group| {
            group
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// Advertises the server when `discovery.mdns` is on. The TXT `id` is the
/// instance ID in the file at `instance_id_path`, and the host name of the
/// registration is built from that ID.
///
/// Returns `None` when `discovery.mdns` is off, and then starts no daemon.
/// Returns `None` and logs one warning when the advertisement cannot start,
/// so the server runs without it.
pub fn advertise(config: &Config, instance_id_path: &Path) -> Option<Advertiser> {
    start_if_enabled(config, || {
        let instance_id = read_instance_id(instance_id_path)?;
        let txt = TxtRecord::build(config, crate::VERSION, &instance_id);
        Advertiser::start(config, &instance_id, &txt)
    })
}

/// Calls `start` when `discovery.mdns` is on and returns what it started.
/// Returns `None` without calling `start` when `discovery.mdns` is off. An
/// error of `start` is logged as one warning and yields `None`.
fn start_if_enabled<T>(
    config: &Config,
    start: impl FnOnce() -> Result<T, DiscoveryError>,
) -> Option<T> {
    if !config.discovery.mdns {
        return None;
    }
    match start() {
        Ok(started) => Some(started),
        Err(error) => {
            tracing::warn!("mDNS advertisement is off: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Auth, Server};

    /// The instance ID the tests use.
    const INSTANCE_ID: &str = "01234567-89ab-4def-8123-456789abcdef";

    /// A config with the instance name `name`.
    fn config_named(name: &str) -> Config {
        Config {
            server: Server {
                name: name.to_owned(),
                ..Server::default()
            },
            ..Config::default()
        }
    }

    /// The pairs of `record` as string slices.
    fn pairs(record: &[(String, String)]) -> Vec<(&str, &str)> {
        record
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect()
    }

    #[test]
    fn service_type_is_waxnerd_over_tcp() {
        assert_eq!(SERVICE_TYPE, "_waxnerd._tcp.local.");
    }

    #[test]
    fn txt_record_has_the_eight_keys_in_order_with_the_documented_values() {
        let record = TxtRecord::build(&config_named("studio"), "1.2.3", INSTANCE_ID);
        assert_eq!(
            pairs(&record),
            [
                ("txtvers", "1"),
                ("api", "opensubsonic"),
                ("path", "/rest"),
                ("id", "01234567-89ab-4def-8123-456789abcdef"),
                ("name", "studio"),
                ("auth", "password"),
                ("tls", "0"),
                ("ver", "1.2.3"),
            ]
        );
    }

    #[test]
    fn txt_auth_is_none_when_auth_is_disabled() {
        let mut config = config_named("studio");
        config.auth = Auth {
            disabled_unsafe: true,
        };
        let record = TxtRecord::build(&config, "1.2.3", INSTANCE_ID);
        assert_eq!(
            pairs(&record),
            [
                ("txtvers", "1"),
                ("api", "opensubsonic"),
                ("path", "/rest"),
                ("id", "01234567-89ab-4def-8123-456789abcdef"),
                ("name", "studio"),
                ("auth", "none"),
                ("tls", "0"),
                ("ver", "1.2.3"),
            ]
        );
    }

    #[test]
    fn txt_ver_is_the_version_text_unchanged() {
        // A client shows `ver` and does not parse it, so a pre-release
        // version and build metadata pass through as they are.
        for version in ["0.1.0", "0.2.0-beta.1", "1.0.0-rc.2+build.5"] {
            let record = TxtRecord::build(&config_named("studio"), version, INSTANCE_ID);
            assert_eq!(record.last(), Some(&("ver".to_owned(), version.to_owned())));
        }
    }

    #[test]
    fn txt_record_takes_nothing_else_from_the_config() {
        let mut config = config_named("studio");
        config.server.bind = "192.168.1.5".parse().expect("an address");
        config.server.port = 4321;
        config.server.log_level = "wn_server=trace".to_owned();
        config.library.folders = vec![PathBuf::from("/home/adnan/Music")];
        config.transcoding.ffmpeg = "/opt/ffmpeg/bin/ffmpeg".to_owned();

        let record = TxtRecord::build(&config, "1.2.3", INSTANCE_ID);
        assert_eq!(
            record,
            TxtRecord::build(&config_named("studio"), "1.2.3", INSTANCE_ID)
        );
        let keys: Vec<&str> = record.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(
            keys,
            ["txtvers", "api", "path", "id", "name", "auth", "tls", "ver"]
        );
    }

    #[test]
    fn interface_kinds_disables_ipv6_for_an_ipv4_bind() {
        for bind in ["0.0.0.0", "192.168.1.5", "127.0.0.1"] {
            let kinds = interface_kinds(bind.parse().expect("an address"));
            assert!(matches!(kinds.as_slice(), [IfKind::IPv6]), "{bind}");
        }
    }

    #[test]
    fn interface_kinds_disables_nothing_for_an_ipv6_bind() {
        for bind in ["::", "::1", "fd00::5"] {
            let kinds = interface_kinds(bind.parse().expect("an address"));
            assert!(kinds.is_empty(), "{bind}");
        }
    }

    /// The address `text`.
    fn address(text: &str) -> IpAddr {
        text.parse().expect("an address")
    }

    #[test]
    fn a_wildcard_bind_names_no_address() {
        for bind in ["0.0.0.0", "::"] {
            assert!(addresses(address(bind)).is_empty(), "{bind}");
        }
    }

    #[test]
    fn a_lan_bind_address_is_the_only_address() {
        for bind in ["192.168.1.5", "10.0.0.7", "fd00::5"] {
            assert_eq!(addresses(address(bind)), [address(bind)], "{bind}");
        }
    }

    #[test]
    fn a_loopback_bind_address_is_the_only_address() {
        for bind in ["127.0.0.1", "::1"] {
            assert_eq!(addresses(address(bind)), [address(bind)], "{bind}");
        }
    }

    /// A second instance ID. Its first group differs from the first group of
    /// [`INSTANCE_ID`].
    const OTHER_INSTANCE_ID: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";

    /// The texts that are not an instance ID. Some have the form of a host
    /// name of a machine.
    const NOT_INSTANCE_IDS: [&str; 12] = [
        "",
        "1a2b3c4d",
        "kept",
        "studio",
        "studio.local",
        "studio.local.",
        "studio.example.com",
        "Adnans-MacBook-Pro.local",
        // Upper case.
        "1A2B3C4D-5E6F-4A7B-8C9D-0E1F2A3B4C5D",
        // No hyphens.
        "1a2b3c4d5e6f4a7b8c9d0e1f2a3b4c5d",
        // A dot in the first group.
        "1a2b.c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d",
        // A character of 2 bytes in the first group, which is 8 bytes long.
        "1a2b3cé-5e6f-4a7b-8c9d-0e1f2a3b4c5d",
    ];

    #[test]
    fn host_name_is_wn_server_and_the_first_8_hex_digits_of_the_instance_id() {
        for (instance_id, name) in [
            (OTHER_INSTANCE_ID, "wn-server-1a2b3c4d.local."),
            (INSTANCE_ID, "wn-server-01234567.local."),
            (
                "ffffffff-0000-4000-8000-000000000000",
                "wn-server-ffffffff.local.",
            ),
            (
                "00000000-ffff-4fff-bfff-ffffffffffff",
                "wn-server-00000000.local.",
            ),
        ] {
            assert_eq!(host_name(instance_id).expect("a host name"), name);
        }
    }

    #[test]
    fn host_name_is_one_label_in_the_local_domain() {
        let name = host_name(OTHER_INSTANCE_ID).expect("a host name");
        let labels: Vec<&str> = name.split('.').collect();
        // The text after the last dot is empty, because the name ends with
        // the dot of the root.
        assert_eq!(labels, ["wn-server-1a2b3c4d", "local", ""]);
        assert_eq!(labels[0].len(), 18);
        check_label("the host label", labels[0]).expect("one DNS label");
    }

    #[test]
    fn host_name_of_a_text_that_is_not_an_instance_id_is_an_error() {
        for text in NOT_INSTANCE_IDS {
            let error = host_name(text).expect_err("not an instance ID");
            assert!(
                matches!(error, DiscoveryError::NotAnInstanceId),
                "{text:?}: {error}"
            );
        }
    }

    #[test]
    fn the_srv_target_is_the_host_name_of_the_server_for_every_bind_address() {
        // A wildcard bind, a LAN address and a loopback address, each in
        // IPv4 and IPv6.
        for bind in [
            "0.0.0.0",
            "::",
            "192.168.1.5",
            "fd00::5",
            "127.0.0.1",
            "::1",
        ] {
            let mut config = config_named("studio");
            config.server.bind = address(bind);
            let txt = TxtRecord::build(&config, "1.2.3", OTHER_INSTANCE_ID);
            let info = service_info(&config, OTHER_INSTANCE_ID, &txt).expect("a registration");
            assert_eq!(info.get_hostname(), "wn-server-1a2b3c4d.local.", "{bind}");
        }
    }

    #[test]
    fn the_srv_target_takes_nothing_from_the_config() {
        // Each name has the form of a host name of a machine.
        for name in ["studio", "studio.local", "Adnans-MacBook-Pro", "wn-server"] {
            let mut config = config_named(name);
            config.server.port = 4321;
            config.server.log_level = "wn_server=trace".to_owned();
            config.library.folders = vec![PathBuf::from("/home/adnan/Music")];
            let txt = TxtRecord::build(&config, "1.2.3", OTHER_INSTANCE_ID);
            let info = service_info(&config, OTHER_INSTANCE_ID, &txt).expect("a registration");
            assert_eq!(info.get_hostname(), "wn-server-1a2b3c4d.local.", "{name}");
        }
    }

    #[test]
    fn the_srv_target_follows_the_instance_id() {
        let config = config_named("studio");
        for (instance_id, target) in [
            (INSTANCE_ID, "wn-server-01234567.local."),
            (OTHER_INSTANCE_ID, "wn-server-1a2b3c4d.local."),
        ] {
            let txt = TxtRecord::build(&config, "1.2.3", instance_id);
            let info = service_info(&config, instance_id, &txt).expect("a registration");
            assert_eq!(info.get_hostname(), target);
        }
    }

    #[test]
    fn service_info_rejects_a_text_that_is_not_an_instance_id() {
        let config = config_named("studio");
        let txt = TxtRecord::build(&config, "1.2.3", INSTANCE_ID);
        for text in NOT_INSTANCE_IDS {
            let error = service_info(&config, text, &txt).expect_err("not an instance ID");
            assert!(
                matches!(error, DiscoveryError::NotAnInstanceId),
                "{text:?}: {error}"
            );
        }
    }

    #[test]
    fn a_label_holds_1_to_63_bytes() {
        check_label("server.name", "a").expect("1 byte");
        check_label("server.name", &"a".repeat(63)).expect("63 bytes");
        // 21 characters of 3 bytes each.
        check_label("server.name", &"音".repeat(21)).expect("63 bytes");

        for (label, bytes) in [
            (String::new(), 0),
            ("a".repeat(64), 64),
            ("音".repeat(22), 66),
        ] {
            let error = check_label("server.name", &label).expect_err("out of range");
            assert_eq!(
                error.to_string(),
                format!("server.name is {bytes} bytes long; a DNS-SD name holds 1 to 63 bytes")
            );
        }
    }

    #[test]
    fn service_info_holds_the_type_the_names_the_port_and_the_txt_record() {
        let mut config = config_named("studio");
        config.server.port = 4321;
        let txt = TxtRecord::build(&config, "1.2.3", INSTANCE_ID);

        let info = service_info(&config, INSTANCE_ID, &txt).expect("a registration");
        assert_eq!(info.get_type(), "_waxnerd._tcp.local.");
        assert_eq!(info.get_fullname(), "studio._waxnerd._tcp.local.");
        assert_eq!(info.get_hostname(), "wn-server-01234567.local.");
        assert_eq!(info.get_port(), 4321);

        let properties: Vec<(&str, &str)> = info
            .get_properties()
            .iter()
            .map(|property| (property.key(), property.val_str()))
            .collect();
        assert_eq!(properties, pairs(&txt));
    }

    /// The registration of a server that binds `bind`.
    fn service_info_bound_to(bind: &str) -> ServiceInfo {
        let mut config = config_named("studio");
        config.server.bind = address(bind);
        let txt = TxtRecord::build(&config, "1.2.3", INSTANCE_ID);
        service_info(&config, INSTANCE_ID, &txt).expect("a registration")
    }

    #[test]
    fn service_info_leaves_the_addresses_of_a_wildcard_bind_to_the_daemon() {
        for bind in ["0.0.0.0", "::"] {
            let info = service_info_bound_to(bind);
            assert!(info.is_addr_auto(), "{bind}");
            assert!(info.get_addresses().is_empty(), "{bind}");
        }
    }

    #[test]
    fn service_info_names_a_lan_bind_address_and_no_other() {
        for bind in ["192.168.1.5", "fd00::5"] {
            let info = service_info_bound_to(bind);
            assert!(!info.is_addr_auto(), "{bind}");
            let named: Vec<IpAddr> = info.get_addresses().iter().copied().collect();
            assert_eq!(named, [address(bind)], "{bind}");
        }
    }

    #[test]
    fn service_info_names_a_loopback_bind_address_and_no_other() {
        for bind in ["127.0.0.1", "::1"] {
            let info = service_info_bound_to(bind);
            assert!(!info.is_addr_auto(), "{bind}");
            let named: Vec<IpAddr> = info.get_addresses().iter().copied().collect();
            assert_eq!(named, [address(bind)], "{bind}");
        }
    }

    #[test]
    fn service_info_keeps_a_dot_in_the_instance_name_in_one_label() {
        let config = config_named("studio.local");
        let txt = TxtRecord::build(&config, "1.2.3", INSTANCE_ID);
        let info = service_info(&config, INSTANCE_ID, &txt).expect("a registration");
        // `mdns-sd` writes a dot inside a label as `\.`.
        assert_eq!(info.get_fullname(), r"studio\.local._waxnerd._tcp.local.");
        // The instance name has the form of a host name and is not the
        // target.
        assert_eq!(info.get_hostname(), "wn-server-01234567.local.");
    }

    #[test]
    fn service_info_takes_the_longest_instance_name() {
        let config = config_named(&"n".repeat(63));
        let txt = TxtRecord::build(&config, "1.2.3", INSTANCE_ID);
        service_info(&config, INSTANCE_ID, &txt).expect("a registration");
    }

    #[test]
    fn service_info_rejects_a_name_that_is_not_one_label() {
        for name in [String::new(), "n".repeat(64)] {
            let config = config_named(&name);
            let txt = TxtRecord::build(&config, "1.2.3", INSTANCE_ID);
            let error = service_info(&config, INSTANCE_ID, &txt).expect_err("not one label");
            assert!(
                matches!(
                    error,
                    DiscoveryError::Label {
                        what: "server.name",
                        ..
                    }
                ),
                "{error}"
            );
        }
    }

    #[test]
    fn instance_id_is_the_content_of_the_file_without_the_line_ending() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("instance.id");
        for text in [
            format!("{INSTANCE_ID}\n"),
            format!("{INSTANCE_ID}\r\n"),
            INSTANCE_ID.to_owned(),
        ] {
            fs::write(&path, text).expect("write instance.id");
            assert_eq!(read_instance_id(&path).expect("an ID"), INSTANCE_ID);
        }
    }

    #[test]
    fn instance_id_that_is_not_a_uuid_is_an_error_that_names_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("instance.id");
        for text in [
            "",
            "\n",
            "kept\n",
            // Upper case.
            "01234567-89AB-4DEF-8123-456789ABCDEF\n",
            // No hyphens.
            "0123456789ab4def8123456789abcdef\n",
            // A group that is one character short.
            "01234567-89ab-4def-8123-456789abcde\n",
            // Two lines.
            "01234567-89ab-4def-8123-456789abcdef\nsecond\n",
        ] {
            fs::write(&path, text).expect("write instance.id");
            let error = read_instance_id(&path).expect_err("not a UUID");
            assert!(matches!(error, DiscoveryError::InstanceId(_)), "{text:?}");
            assert!(
                error.to_string().contains(&path.display().to_string()),
                "{error}"
            );
        }
    }

    #[test]
    fn a_missing_instance_id_is_an_error_that_names_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("instance.id");
        let error = read_instance_id(&path).expect_err("a missing file");
        assert!(
            matches!(error, DiscoveryError::ReadInstanceId { .. }),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .starts_with(&format!("cannot read {}: ", path.display())),
            "{error}"
        );
    }

    #[test]
    fn nothing_starts_when_mdns_is_off() {
        let mut config = Config::default();
        config.discovery.mdns = false;
        let started = start_if_enabled(&config, || -> Result<(), DiscoveryError> {
            panic!("start ran with discovery.mdns = false")
        });
        assert_eq!(started, None);
    }

    #[test]
    fn the_start_runs_when_mdns_is_on() {
        let config = Config::default();
        assert!(config.discovery.mdns);
        assert_eq!(start_if_enabled(&config, || Ok("started")), Some("started"));
    }

    #[test]
    fn a_start_that_fails_yields_none_and_no_error() {
        let started = start_if_enabled(&Config::default(), || -> Result<(), DiscoveryError> {
            Err(DiscoveryError::Mdns(mdns_sd::Error::Msg(
                "no socket".to_owned(),
            )))
        });
        assert_eq!(started, None);
    }

    #[test]
    fn advertise_starts_no_daemon_when_mdns_is_off() {
        // The instance ID is valid, so only `discovery.mdns` keeps the daemon
        // from starting.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("instance.id");
        fs::write(&path, format!("{INSTANCE_ID}\n")).expect("write instance.id");
        let mut config = config_named("studio");
        config.discovery.mdns = false;
        assert!(advertise(&config, &path).is_none());
    }

    #[test]
    fn advertise_without_an_instance_id_starts_no_daemon() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = config_named("studio");
        assert!(config.discovery.mdns);
        assert!(advertise(&config, &dir.path().join("instance.id")).is_none());
    }

    #[test]
    fn a_stop_before_the_announcement_sends_no_goodbye() {
        assert!(!sends_goodbye(false, Duration::ZERO));
        assert!(!sends_goodbye(false, Duration::from_millis(900)));
        assert!(!sends_goodbye(false, Duration::from_millis(9_999)));
    }

    #[test]
    fn a_stop_after_the_announcement_sends_a_goodbye() {
        assert!(sends_goodbye(true, Duration::ZERO));
        assert!(sends_goodbye(true, Duration::from_millis(900)));
        assert!(sends_goodbye(true, Duration::from_secs(3_600)));
    }

    #[test]
    fn a_stop_after_the_watch_ended_sends_a_goodbye() {
        // The reporting thread watches for ten seconds. It does not see an
        // announcement that comes later.
        assert!(sends_goodbye(false, Duration::from_secs(10)));
        assert!(sends_goodbye(false, Duration::from_secs(3_600)));
    }

    /// The outcome of the events in `events`, followed by `end`.
    fn outcome_of(events: Vec<DaemonEvent>, end: RecvTimeoutError) -> Outcome {
        let mut events = events.into_iter();
        outcome(|| events.next().ok_or(end))
    }

    #[test]
    fn the_first_announcement_decides_the_outcome() {
        let announced = "studio (2)._waxnerd._tcp.local.";
        let events = vec![
            DaemonEvent::IpAdd("192.168.1.5".parse().expect("an address")),
            DaemonEvent::Respond("en0".to_owned()),
            DaemonEvent::Announce(
                announced.to_owned(),
                "wn-server-01234567.local.:en0".to_owned(),
            ),
            DaemonEvent::Announce(
                announced.to_owned(),
                "wn-server-01234567.local.:lo0".to_owned(),
            ),
        ];
        assert_eq!(
            outcome_of(events, RecvTimeoutError::Timeout),
            Outcome::Announced(announced.to_owned())
        );
    }

    #[test]
    fn an_error_of_the_daemon_decides_the_outcome() {
        let events = vec![DaemonEvent::Error(mdns_sd::Error::Msg(
            "no socket".to_owned(),
        ))];
        assert_eq!(
            outcome_of(events, RecvTimeoutError::Timeout),
            Outcome::Failed("no socket".to_owned())
        );
    }

    #[test]
    fn no_announcement_before_the_wait_ends_is_silent() {
        let events = vec![DaemonEvent::IpAdd(
            "192.168.1.5".parse().expect("an address"),
        )];
        assert_eq!(
            outcome_of(events, RecvTimeoutError::Timeout),
            Outcome::Silent
        );
    }

    #[test]
    fn a_daemon_that_shuts_down_first_is_stopped() {
        assert_eq!(
            outcome_of(Vec::new(), RecvTimeoutError::Disconnected),
            Outcome::Stopped
        );
    }
}
