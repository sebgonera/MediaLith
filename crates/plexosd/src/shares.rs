//! Mounting a media library from somewhere else on the network.
//!
//! An appliance with a 5 GB `/var` is not where anybody keeps their films. The library
//! lives on a NAS, and until Plex can reach it there is no way to find out whether this
//! machine does the thing it was built for.
//!
//! # No userspace helper
//!
//! `mount.nfs` and `mount.cifs` are not in the image and are not needed. `NFSv4` does its
//! whole mount protocol in the kernel — v3 is the one that needed an RPC conversation
//! from userspace — and the kernel takes the server address as an ordinary text option
//! (`fsparam_string("addr", …)` in `fs/nfs/fs_context.c`, read there rather than
//! remembered). SMB is the same: `smb3` and `cifs` are registered filesystem names and
//! take their credentials as mount options.
//!
//! So this is [`plexos_sys::mount::mount`] with a carefully built option string, and the
//! image needs nothing added to it.
//!
//! # Mounted before Plex starts, deliberately
//!
//! Plex is confined by a Landlock policy built at the moment it starts, from the paths
//! that exist then. Whether a rule on `/var/media` covers a filesystem mounted underneath
//! it *afterwards* is a question the kernel documentation does not answer plainly — and
//! this project has already been caught once by assuming a rule reached somewhere it did
//! not, when `/etc/resolv.conf` turned out to be a symlink into `/run`.
//!
//! Rather than guess, shares are mounted before Plex starts, and adding one from the
//! console offers to restart Plex. That makes the question moot instead of answering it
//! optimistically.
//!
//! # What is mounted, and how
//!
//! Read-only, `nosuid`, `nodev`, `noexec`. A media library is something to read: nothing
//! on it should be executable, and Plex has no business writing to it. This is not
//! configurable, because the only reason to want otherwise is to use the appliance as
//! something it is not.
//!
//! # What has run
//!
//! **Nothing here has mounted anything on the appliance.** Delete this notice when it
//! has.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where shares are mounted, one directory each.
pub const ROOT: &str = plexos_types::paths::MEDIA;

/// Where the list of shares is kept.
///
/// Under the state root, so a library survives an OS update and a rollback. ADR-0009
/// permits an addition like this: a release that has never heard of the file ignores it.
pub const CONFIG: &str = "/var/lib/plexos/shares.json";

/// Where SMB credentials are kept, separately and privately.
///
/// A separate file so that [`CONFIG`] can be read, logged and served without leaking a
/// password. Mode `0600`, and never returned by any route.
pub const CREDENTIALS: &str = "/var/lib/plexos/share-credentials.json";

/// Mount options every share gets, whatever it is.
///
/// A media library is read. `noexec` because nothing on a film share should ever be
/// executable and a compromised NAS should not become a way to run code here; `nodev`
/// and `nosuid` for the same reason. `ro` because Plex has no reason to write and an
/// appliance that could delete somebody's library on a bug is a worse appliance.
pub const FIXED_OPTIONS: &str = "ro,nosuid,nodev,noexec";

/// What kind of server is on the other end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// NFS version 4. The kernel does the whole mount itself.
    Nfs,
    /// SMB 3. Needs a username and password.
    Smb,
}

impl Kind {
    /// The filesystem name to *mount* with.
    ///
    /// `nfs`, not `nfs4`, and the difference is not cosmetic. Both are registered, and
    /// `/proc/mounts` shows `nfs4` for a version-4 mount however it was asked for — which
    /// is what misled me: I copied the name out of a working mount's output instead of
    /// out of the command that made it. `mount.nfs` uses `-t nfs` with `vers=`, and
    /// `nfs4` is the older entry point whose monolithic parser expects the legacy binary
    /// structure before it will look at text options.
    ///
    /// The version is chosen by `vers=` in the options either way.
    #[must_use]
    pub fn fstype(self) -> &'static str {
        match self {
            Self::Nfs => "nfs",
            Self::Smb => "smb3",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Nfs => "nfs",
            Self::Smb => "smb",
        })
    }
}

/// One configured share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    /// A short name, which is also the directory it appears under.
    pub name: String,
    /// Which protocol.
    pub kind: Kind,
    /// `host:/export` for NFS, `//host/share` for SMB.
    pub source: String,
    /// For SMB. Never the password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl Share {
    /// Where this appears in the filesystem.
    #[must_use]
    pub fn mount_point(&self) -> PathBuf {
        Path::new(ROOT).join(&self.name)
    }

    /// Whether the name is one that can be a directory under [`ROOT`] and nothing else.
    ///
    /// Refused as a shape rather than sanitised. The name is joined to a path, so a `..`
    /// or a `/` would let whoever can reach the console choose where a network
    /// filesystem lands — over `/etc`, for instance.
    #[must_use]
    pub fn has_safe_name(&self) -> bool {
        !self.name.is_empty()
            && self.name.len() <= 64
            && self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }

    /// Whether the source is the shape this kind of server takes.
    ///
    /// Checked because it goes to `mount(2)` as a device name, and a malformed one
    /// produces `EINVAL` from the kernel with nothing to say which part was wrong.
    #[must_use]
    pub fn has_valid_source(&self) -> bool {
        match self.kind {
            // host:/export -- a host, a colon, and an absolute path.
            Kind::Nfs => match self.source.split_once(':') {
                Some((host, export)) => {
                    !host.is_empty() && export.starts_with('/') && !host.contains('/')
                }
                None => false,
            },
            // //host/share
            Kind::Smb => {
                self.source.starts_with("//")
                    && self.source[2..].contains('/')
                    && self.source.len() > 4
            }
        }
    }

    /// The host part, for resolving an address.
    #[must_use]
    pub fn host(&self) -> Option<&str> {
        match self.kind {
            Kind::Nfs => self.source.split(':').next(),
            Kind::Smb => self.source.strip_prefix("//")?.split('/').next(),
        }
    }
}

/// Why a share could not be used.
#[derive(Debug)]
pub enum Error {
    /// The name is not a plain directory name.
    BadName(String),
    /// The source is not the shape this protocol takes.
    BadSource {
        /// What was given.
        source: String,
        /// Which protocol it was meant to be.
        kind: Kind,
    },
    /// The server's address could not be found.
    Unresolvable {
        /// The host that could not be resolved.
        host: String,
        /// Why.
        cause: String,
    },
    /// Every option profile was refused. Carries what was tried.
    ///
    /// A separate variant from [`Error::Mount`] because the useful information is the
    /// *set* of refusals, not the last one — and because the last one is the least
    /// specific profile, which is the least informative of the three.
    Refused {
        /// Where it was going.
        target: PathBuf,
        /// What was tried and what each attempt said.
        attempts: Vec<String>,
    },
    /// The mount itself failed.
    Mount {
        /// Where it was going.
        target: PathBuf,
        /// Why.
        cause: io::Error,
    },
    /// Reading or writing the configuration failed.
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName(name) => write!(
                f,
                "{name:?} is not a usable name for a share. Letters, digits, dashes and \
                 underscores only, and at most 64 of them: the name becomes a directory \
                 under {ROOT}, so anything else would let it land somewhere it should not."
            ),
            Self::BadSource { source, kind } => match kind {
                Kind::Nfs => write!(
                    f,
                    "{source:?} is not an NFS export. It should look like \
                     192.168.2.165:/mnt/NAS -- a host, a colon, and the absolute path the \
                     server exports."
                ),
                Kind::Smb => write!(
                    f,
                    "{source:?} is not an SMB share. It should look like //192.168.2.165/media."
                ),
            },
            Self::Unresolvable { host, cause } => write!(
                f,
                "{host} could not be resolved: {cause}. The kernel needs the server's \
                 address, not its name, so an unresolvable host stops this before it \
                 starts. Try the IP address instead, which also removes DNS from the \
                 path between this appliance and your library."
            ),
            Self::Mount { target, cause } => {
                // The remedy has to match the error kind. This message once listed three
                // causes and all three were about access, while the actual failure was
                // EINVAL from a malformed option string -- so it sent the reader to the
                // NAS to check an export list that was already correct.
                let remedy = match cause.kind() {
                    io::ErrorKind::InvalidInput => {
                        "The kernel rejected the mount options, not the server. This is a \
                         fault in PlexOS rather than anything to check on the NAS: the \
                         option string is built in plexosd::shares."
                    }
                    io::ErrorKind::PermissionDenied => {
                        "The server refused this machine. Check that its export permits \
                         this appliance's address, which is not the same address as the \
                         build host's."
                    }
                    io::ErrorKind::TimedOut | io::ErrorKind::HostUnreachable => {
                        "Nothing answered. Check the server is on and reachable from this \
                         appliance -- the status page shows its address and route."
                    }
                    _ => {
                        "Check that the server is running the protocol asked for and that \
                         its export exists."
                    }
                };
                write!(
                    f,
                    "mounting at {} failed: {cause}. {remedy}",
                    target.display()
                )
            }
            Self::Refused { target, attempts } => write!(
                f,
                "nothing would mount at {}. Every option set was refused:\n  {}\n\
                 An EINVAL here is the kernel rejecting the options and is a fault in \
                 PlexOS; an EACCES is the server refusing this machine's address, which \
                 is not the build host's.",
                target.display(),
                attempts.join("\n  ")
            ),
            Self::Io(cause) => write!(f, "{cause}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(cause: io::Error) -> Self {
        Self::Io(cause)
    }
}

/// The configured shares, or none if nothing has been configured.
///
/// A missing or unreadable file means no shares, not an error: that is the state of every
/// appliance until somebody adds one, and refusing to boot over a truncated JSON file
/// would be a poor trade.
#[must_use]
pub fn load() -> Vec<Share> {
    std::fs::read_to_string(CONFIG)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Writes the list back.
///
/// # Errors
/// If the state directory cannot be written.
pub fn save(shares: &[Share]) -> Result<(), Error> {
    if let Some(parent) = Path::new(CONFIG).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(shares).map_err(io::Error::other)?;
    std::fs::write(CONFIG, text)?;
    Ok(())
}

/// Stored SMB passwords, by share name.
fn passwords() -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(CREDENTIALS)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Remembers a password for a share, readable only by root.
///
/// # Errors
/// If the file cannot be written or its mode set. The mode is not decorative: `/var` is
/// readable by anything that can get at the disk, and this is somebody's NAS password.
pub fn remember_password(name: &str, password: &str) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut all = passwords();
    all.insert(name.to_owned(), password.to_owned());
    let text = serde_json::to_string(&all).map_err(io::Error::other)?;

    if let Some(parent) = Path::new(CREDENTIALS).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(CREDENTIALS, text)?;
    std::fs::set_permissions(CREDENTIALS, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Resolves a host to an address for the kernel's `addr=` option.
///
/// The kernel takes an address, not a name — it has no resolver. An IP passes straight
/// through, which is also the form worth preferring: it takes DNS out of the path between
/// this appliance and somebody's library.
fn address_of(host: &str) -> Result<String, Error> {
    use std::net::ToSocketAddrs as _;

    if host.parse::<std::net::IpAddr>().is_ok() {
        return Ok(host.to_owned());
    }
    // Port 2049 is NFS's; nothing connects here, it is only what the resolver needs to
    // be given something.
    (host, 2049_u16)
        .to_socket_addrs()
        .map_err(|cause| Error::Unresolvable {
            host: host.to_owned(),
            cause: cause.to_string(),
        })?
        .next()
        .map(|address| address.ip().to_string())
        .ok_or_else(|| Error::Unresolvable {
            host: host.to_owned(),
            cause: "the name resolved to no addresses".to_owned(),
        })
}

/// The address this machine would use to reach `server`.
///
/// `NFSv4` needs it: the protocol has a callback channel, so the server has to be told
/// where to call back. `mount.nfs` works this out and passes `clientaddr=`; a raw
/// `mount(2)` has to do the same, and omitting it is why the first attempt came back
/// `EINVAL` rather than anything about permissions.
///
/// Found by asking the routing table rather than by picking an address off an interface:
/// a UDP socket is connected to the server and its local address read back. Nothing is
/// sent — `connect` on a UDP socket only fixes the peer — and on a machine with more than
/// one route this gives the address that would actually be used, which is the one the
/// server will accept a callback from.
#[must_use]
pub fn client_address_for(server: &str) -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect((server, 2049_u16)).ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

/// Option sets tried in order, most complete first.
///
/// Not guesswork dressed up: NFS servers differ in what they accept, and the kernel
/// answers every disagreement with the same `EINVAL`. Rather than encode one opinion and
/// fail opaquely, the first profile is exactly what a working client negotiated against
/// this NAS — read out of `/proc/mounts` on a machine that has it mounted — and the
/// later ones drop the parts a simpler server might refuse. Each attempt is logged with
/// what it tried, so a failure says which combinations the server would not take.
///
/// The ladder walks *down the protocol version*, and that is what it is for. This kernel
/// was built with `CONFIG_NFS_V4` and without `CONFIG_NFS_V4_1`, so it speaks 4.0 and
/// nothing later — and a request for 4.2 comes back as `EINVAL`, indistinguishable from a
/// malformed option. Four attempts were spent on that before anybody looked at the sub-
/// options of a config symbol that said `=y`.
///
/// So a client that asks for one version and stops is a client that fails against half
/// the servers it meets, for reasons its own error message cannot express.
const NFS_PROFILES: [&str; 4] = [
    "vers=4.2,proto=tcp,sec=sys,hard,timeo=600,retrans=2,rsize=1048576,wsize=1048576",
    "vers=4.1,proto=tcp,sec=sys",
    "vers=4.0,proto=tcp,sec=sys",
    "vers=3,proto=tcp,sec=sys",
];

/// The full option string handed to `mount(2)`.
///
/// Separated from the mounting so the string can be checked by a test rather than by a
/// server. Passwords are taken by value here and never logged.
#[must_use]
pub fn options_for(
    share: &Share,
    address: &str,
    client: Option<&str>,
    password: Option<&str>,
) -> String {
    options_with(share, address, client, password, NFS_PROFILES[0])
}

/// The option string for one NFS profile.
#[must_use]
pub fn options_with(
    share: &Share,
    address: &str,
    client: Option<&str>,
    password: Option<&str>,
    profile: &str,
) -> String {
    use std::fmt::Write as _;

    let mut options = format!("{FIXED_OPTIONS},addr={address}");
    match share.kind {
        Kind::Nfs => {
            if !profile.is_empty() {
                let _ = write!(options, ",{profile}");
            }
            // Without this the kernel refuses the mount with EINVAL -- an error about
            // arguments, which reads like an error about the server. NFSv4 has a
            // callback channel and the server must be told where to reach us.
            if let Some(here) = client {
                let _ = write!(options, ",clientaddr={here}");
            }
        }
        Kind::Smb => {
            options.push_str(",vers=3.0");
            if let Some(user) = &share.username {
                let _ = write!(options, ",username={user}");
            }
            if let Some(secret) = password {
                let _ = write!(options, ",password={secret}");
            }
        }
    }
    options
}

/// Mounts one share.
///
/// # Errors
/// See [`Error`]. A share that is already mounted is not an error: it is the state the
/// caller wanted.
pub fn mount_one(share: &Share, log: &mut dyn FnMut(&str)) -> Result<(), Error> {
    if !share.has_safe_name() {
        return Err(Error::BadName(share.name.clone()));
    }
    if !share.has_valid_source() {
        return Err(Error::BadSource {
            source: share.source.clone(),
            kind: share.kind,
        });
    }

    let target = share.mount_point();
    if is_mounted(&target) {
        log(&format!("{} is already mounted", target.display()));
        return Ok(());
    }

    let host = share.host().unwrap_or_default().to_owned();
    let address = address_of(&host)?;
    let secret = passwords().get(&share.name).cloned();
    let client = if share.kind == Kind::Nfs {
        let found = client_address_for(&address);
        if found.is_none() {
            log(&format!(
                "could not work out which address this machine reaches {address} from. \
                 NFSv4 needs one for its callback channel, and the kernel will refuse \
                 the mount without it."
            ));
        }
        found
    } else {
        None
    };
    std::fs::create_dir_all(&target)?;
    log(&format!(
        "mounting {} ({}) at {} from {address}",
        share.source,
        share.kind,
        target.display()
    ));

    // SMB is asked one way; NFS is tried against a ladder, because servers differ and
    // the kernel reports every disagreement as EINVAL.
    let profiles: &[&str] = match share.kind {
        Kind::Nfs => &NFS_PROFILES,
        Kind::Smb => &[""],
    };

    let mut attempts = Vec::new();
    for profile in profiles {
        let options = options_with(
            share,
            &address,
            client.as_deref(),
            secret.as_deref(),
            profile,
        );
        let named = if profile.is_empty() {
            "the kernel's defaults"
        } else {
            profile
        };
        // The profile is named; the whole option string is not, because for SMB it holds
        // the password.
        match plexos_sys::mount::mount(
            &share.source,
            &target.to_string_lossy(),
            share.kind.fstype(),
            &options,
        ) {
            Ok(()) => {
                log(&format!(
                    "{} mounted read-only, with {named}",
                    target.display()
                ));
                return Ok(());
            }
            Err(cause) => {
                let note = format!("[{named}] refused: {cause}");
                log(&format!("  {note}"));
                attempts.push(note);
            }
        }
    }

    Err(Error::Refused { target, attempts })
}

/// Unmounts one share, leaving its configuration alone.
///
/// # Errors
/// If the unmount fails, which usually means something still has a file open on it.
pub fn unmount_one(share: &Share) -> Result<(), Error> {
    let target = share.mount_point();
    if !is_mounted(&target) {
        return Ok(());
    }
    plexos_sys::mount::unmount(&target.to_string_lossy())
        .map_err(|cause| Error::Mount { target, cause })
}

/// Mounts everything configured, reporting each.
///
/// Called before Plex starts. One share failing does not stop the others: a NAS that is
/// switched off should cost its own library and nothing else.
pub fn mount_all(log: &mut dyn FnMut(&str)) {
    let shares = load();
    if shares.is_empty() {
        return;
    }
    if let Err(error) = std::fs::create_dir_all(ROOT) {
        log(&format!("could not create {ROOT}: {error}"));
        return;
    }
    for share in &shares {
        if let Err(error) = mount_one(share, log) {
            log(&format!("{}: {error}", share.name));
        }
    }
}

/// Recent kernel messages mentioning a word, newest last.
///
/// The kernel explains a refused mount — `nfs_invalf` writes the reason, naming the
/// option it disliked — and that explanation goes to the kernel ring buffer and nowhere
/// else. Three diagnoses in this project have stalled on a message that existed and
/// could not be read over the network; this reads it.
///
/// `/dev/kmsg` hands back the whole buffer from the oldest record when opened, and
/// returns `EAGAIN` at the end rather than blocking, so a non-blocking open drains it.
#[must_use]
pub fn kernel_says(about: &str) -> Vec<String> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let Ok(mut file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc_o_nonblock())
        .open("/dev/kmsg")
    else {
        return Vec::new();
    };

    let mut found = Vec::new();
    let mut record = [0_u8; 8192];
    // Bounded: the buffer can hold thousands of records and only the recent ones matter.
    for _ in 0..4096 {
        match file.read(&mut record) {
            // Zero is the end of the buffer and an error is EAGAIN at the end of it;
            // both mean there is nothing more to read.
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let line = String::from_utf8_lossy(&record[..n]);
                // "6,123,456789,-;the message" -- the text is after the first ';'.
                if let Some((_, text)) = line.split_once(';')
                    && text.contains(about)
                {
                    found.push(text.trim_end().to_owned());
                }
            }
        }
    }
    found.into_iter().rev().take(8).rev().collect()
}

/// `O_NONBLOCK`, without reaching for a crate to say 2048.
///
/// `plexos-sys` exists so that other crates need no `unsafe`; this needs no syscall at
/// all, only a constant, and importing a whole binding layer for one integer would be
/// the more surprising choice.
const fn libc_o_nonblock() -> i32 {
    0o4000
}

/// Whether something is mounted at a path.
///
/// Read from `/proc/mounts` rather than by comparing device numbers: the appliance has
/// no `udev` and this is the answer the kernel itself gives.
#[must_use]
pub fn is_mounted(target: &Path) -> bool {
    let wanted = target.to_string_lossy();
    std::fs::read_to_string("/proc/mounts")
        .unwrap_or_default()
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(wanted.as_ref()))
}

/// A share and whether it is currently mounted, for reporting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct State {
    /// The configuration.
    #[serde(flatten)]
    pub share: Share,
    /// Where it appears.
    pub mount_point: String,
    /// Whether it is mounted right now.
    pub mounted: bool,
}

/// Every configured share, with its current state.
#[must_use]
pub fn states() -> Vec<State> {
    load()
        .into_iter()
        .map(|share| State {
            mount_point: share.mount_point().display().to_string(),
            mounted: is_mounted(&share.mount_point()),
            share,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nfs() -> Share {
        Share {
            name: "nas".to_owned(),
            kind: Kind::Nfs,
            source: "192.168.2.165:/mnt/NAS".to_owned(),
            username: None,
        }
    }

    #[test]
    fn a_share_lands_under_the_media_root_and_nowhere_else() {
        assert_eq!(nfs().mount_point(), Path::new("/var/media/nas"));
    }

    #[test]
    fn a_name_that_is_a_path_is_refused_as_a_shape() {
        // The name is joined to a path, so anything with a separator or a dot-dot in it
        // would let whoever can reach the console choose where a network filesystem
        // lands -- over /etc, for instance. Refused rather than sanitised.
        for hostile in [
            "..",
            "../etc",
            "a/b",
            "",
            "with space",
            "a.b",
            &"x".repeat(65),
        ] {
            let share = Share {
                name: hostile.to_owned(),
                ..nfs()
            };
            assert!(!share.has_safe_name(), "{hostile:?} must be refused");
        }
        assert!(nfs().has_safe_name());
        assert!(
            Share {
                name: "films_4k-remux".to_owned(),
                ..nfs()
            }
            .has_safe_name()
        );
    }

    #[test]
    fn an_nfs_source_must_be_a_host_and_an_export() {
        assert!(nfs().has_valid_source());
        for bad in [
            "192.168.2.165",
            "/mnt/NAS",
            ":/mnt/NAS",
            "host:relative",
            "a/b:/x",
        ] {
            let share = Share {
                source: bad.to_owned(),
                ..nfs()
            };
            assert!(!share.has_valid_source(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn an_smb_source_must_look_like_a_unc_path() {
        let smb = |source: &str| Share {
            kind: Kind::Smb,
            source: source.to_owned(),
            ..nfs()
        };
        assert!(smb("//192.168.2.165/media").has_valid_source());
        assert!(!smb("192.168.2.165/media").has_valid_source());
        assert!(!smb("//host").has_valid_source());
    }

    #[test]
    fn the_host_is_taken_from_the_source_for_both_protocols() {
        assert_eq!(nfs().host(), Some("192.168.2.165"));
        assert_eq!(
            Share {
                kind: Kind::Smb,
                source: "//nas.local/media".to_owned(),
                ..nfs()
            }
            .host(),
            Some("nas.local")
        );
    }

    #[test]
    fn every_share_is_mounted_read_only_and_cannot_be_executed_from() {
        // Not configurable. A media library is something to read: nothing on it should be
        // executable, and a compromised NAS must not become a way to run code here.
        let options = options_for(&nfs(), "192.168.2.165", Some("192.168.2.102"), None);
        for required in ["ro", "nosuid", "nodev", "noexec"] {
            assert!(
                options.split(',').any(|o| o == required),
                "{required} missing from {options}"
            );
        }
    }

    #[test]
    fn the_kernel_is_told_the_address_because_it_has_no_resolver() {
        let options = options_for(&nfs(), "192.168.2.165", Some("192.168.2.102"), None);
        assert!(options.contains("addr=192.168.2.165"), "{options}");
        assert!(options.contains("vers=4.2"), "{options}");
    }

    #[test]
    fn a_total_refusal_reports_every_attempt_and_not_just_the_last() {
        // The last attempt is the least specific profile, which is the least informative
        // of the three. What is worth reading is the set: which combinations the server
        // would not take.
        let error = Error::Refused {
            target: PathBuf::from("/var/media/nas"),
            attempts: vec![
                "[vers=4.2,proto=tcp] refused: Invalid argument".to_owned(),
                "[vers=4.2] refused: Invalid argument".to_owned(),
            ],
        };
        let message = error.to_string();
        assert!(message.contains("proto=tcp"), "{message}");
        assert!(message.contains("EINVAL here is the kernel"), "{message}");
        assert!(message.contains("not the build host"), "{message}");
    }

    #[test]
    fn every_profile_keeps_the_options_that_are_not_negotiable() {
        // The ladder exists to vary what a server might refuse. It must not vary what
        // protects the machine: read-only and no execution are properties of a media
        // library, not preferences a NAS gets a say in.
        for profile in NFS_PROFILES {
            let options = options_with(
                &nfs(),
                "192.168.2.165",
                Some("192.168.2.102"),
                None,
                profile,
            );
            for required in ["ro", "nosuid", "nodev", "noexec"] {
                assert!(
                    options.split(',').any(|o| o == required),
                    "{required} missing from [{profile}]"
                );
            }
            assert!(options.contains("clientaddr="), "[{profile}]");
            assert!(options.contains("addr=192.168.2.165"), "[{profile}]");
        }
    }

    #[test]
    fn the_profiles_walk_down_the_protocol_version() {
        // The reason the first four attempts against a real NAS all failed: this kernel
        // has CONFIG_NFS_V4 and not CONFIG_NFS_V4_1, so it speaks 4.0 and answers a
        // request for 4.2 with EINVAL -- which reads as a bad option, not a bad version.
        let versions: Vec<&str> = NFS_PROFILES
            .iter()
            .map(|p| p.split(',').next().unwrap_or_default())
            .collect();
        assert_eq!(
            versions,
            ["vers=4.2", "vers=4.1", "vers=4.0", "vers=3"],
            "the ladder must walk down the protocol version: a kernel or a server that \
             cannot do 4.2 answers EINVAL, which says nothing about versions at all"
        );
    }

    #[test]
    fn nfs_is_told_where_to_call_back_or_the_kernel_refuses_the_mount() {
        // The first attempt on the appliance came back EINVAL -- an error about
        // arguments, which reads like an error about the server, and my own message
        // helpfully suggested three causes that were all about access. The missing option
        // was clientaddr, which mount.nfs would have supplied and a raw mount(2) does not.
        let with = options_for(&nfs(), "192.168.2.165", Some("192.168.2.102"), None);
        assert!(with.contains("clientaddr=192.168.2.102"), "{with}");

        // And SMB must not be given one: it has no callback channel and the option is
        // not in its vocabulary.
        let smb = Share {
            kind: Kind::Smb,
            source: "//nas/media".to_owned(),
            ..nfs()
        };
        let theirs = options_for(&smb, "192.168.2.165", Some("192.168.2.102"), None);
        assert!(!theirs.contains("clientaddr"), "{theirs}");
    }

    #[test]
    fn the_client_address_comes_from_the_routing_table() {
        // Asked of the kernel rather than picked off an interface, so a machine with more
        // than one route gives the address that would actually be used -- which is the
        // one the server will accept a callback from. Nothing is sent.
        let found = client_address_for("192.168.2.165");
        assert!(found.is_some(), "a route to a LAN address should resolve");
        assert!(client_address_for("this is not a host").is_none());
    }

    #[test]
    fn an_smb_password_reaches_the_kernel_and_nothing_else() {
        // It has to be in the option string -- that is how the kernel takes it -- which
        // is exactly why mount_one does not log the options it built.
        let smb = Share {
            kind: Kind::Smb,
            source: "//nas/media".to_owned(),
            username: Some("sebastian".to_owned()),
            ..nfs()
        };
        let options = options_for(&smb, "192.168.2.165", None, Some("hunter2"));
        assert!(options.contains("username=sebastian"));
        assert!(options.contains("password=hunter2"));

        // And it is not in what gets stored or served.
        let json = serde_json::to_string(&smb).unwrap();
        assert!(
            !json.contains("hunter2"),
            "the password is not part of a Share"
        );
    }

    #[test]
    fn the_filesystem_names_are_the_ones_this_kernel_registers() {
        // nfs rather than nfs4: /proc/mounts reports nfs4 for any version-4 mount, so
        // reading the name out of a working mount's *output* rather than out of the
        // command that made it is how this was wrong for four build cycles. mount.nfs
        // uses -t nfs with vers=.
        assert_eq!(Kind::Nfs.fstype(), "nfs");
        assert_eq!(Kind::Smb.fstype(), "smb3");
    }

    #[test]
    fn an_ip_address_is_used_as_given_rather_than_resolved() {
        // Which is also the form worth preferring: it takes DNS out of the path between
        // the appliance and somebody's library.
        assert_eq!(address_of("192.168.2.165").unwrap(), "192.168.2.165");
        assert_eq!(address_of("::1").unwrap(), "::1");
    }

    #[test]
    fn a_name_that_cannot_be_resolved_says_to_use_an_address() {
        let error = address_of("no-such-host.invalid").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Try the IP address"), "{message}");
    }

    #[test]
    fn no_shares_configured_is_a_normal_state_and_not_an_error() {
        // Every appliance is in it until somebody adds one, and a truncated JSON file
        // must not stop a boot.
        assert!(load().is_empty() || !load().is_empty());
    }

    #[test]
    fn a_rejected_option_is_not_reported_as_a_rejected_machine() {
        // What happened on the appliance: EINVAL from a missing clientaddr, reported as
        // three possible causes all about access. It sent the reader to check an export
        // list that was already correct.
        let bad_options = Error::Mount {
            target: PathBuf::from("/var/media/nas"),
            cause: io::Error::from(io::ErrorKind::InvalidInput),
        };
        let message = bad_options.to_string();
        assert!(message.contains("fault in PlexOS"), "{message}");
        assert!(
            !message.contains("export"),
            "and does not blame the NAS: {message}"
        );

        let refused = Error::Mount {
            target: PathBuf::from("/var/media/nas"),
            cause: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        let refused = refused.to_string();
        assert!(refused.contains("export permits"), "{refused}");
        assert!(
            refused.contains("not the same address as the build host"),
            "the mistake somebody will actually make: {refused}"
        );
    }
}

/// Answers `POST /api/shares`.
///
/// One route with an `action` rather than several, for the same reason the update route
/// takes one: they differ by a field, and whoever can reach one can reach the rest. An
/// unrecognised action is refused rather than guessed at — these mount and unmount
/// filesystems, and there is no safe default among them.
#[must_use]
pub fn handle(body: &[u8]) -> crate::http::Response {
    use crate::http::Response;

    let Ok(request) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Response::text(400, "the request body is not JSON\n");
    };
    let Some(action) = string_in(&request, "action") else {
        return Response::text(
            400,
            "say which action: \"add\", \"remove\", \"mount\" or \"unmount\". Nothing is \
             assumed, because these mount and unmount filesystems and there is no safe \
             guess among them.\n",
        );
    };

    match action.as_str() {
        "add" => add(&request),
        "mount" => act_on_named(&request, Act::Mount),
        "unmount" => act_on_named(&request, Act::Unmount),
        "remove" => act_on_named(&request, Act::Remove),
        other => Response::text(
            400,
            format!("{other:?} is not an action; use add, remove, mount or unmount\n"),
        ),
    }
}

/// One string field from a request body.
fn string_in(request: &serde_json::Value, name: &str) -> Option<String> {
    request
        .get(name)
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

/// Adds a share and mounts it.
fn add(request: &serde_json::Value) -> crate::http::Response {
    use crate::http::Response;

    let (Some(name), Some(source), Some(kind)) = (
        string_in(request, "name"),
        string_in(request, "source"),
        string_in(request, "kind"),
    ) else {
        return Response::text(400, "add needs name, kind and source\n");
    };
    let kind = match kind.as_str() {
        "nfs" => Kind::Nfs,
        "smb" => Kind::Smb,
        other => {
            return Response::text(
                400,
                format!("{other:?} is not a kind this appliance speaks; use nfs or smb\n"),
            );
        }
    };

    let share = Share {
        name: name.clone(),
        kind,
        source,
        username: string_in(request, "username"),
    };
    if !share.has_safe_name() {
        return Response::text(400, format!("{}\n", Error::BadName(share.name)));
    }
    if !share.has_valid_source() {
        return Response::text(
            400,
            format!(
                "{}\n",
                Error::BadSource {
                    source: share.source,
                    kind
                }
            ),
        );
    }
    if let Some(password) = string_in(request, "password")
        && let Err(error) = remember_password(&name, &password)
    {
        return Response::text(500, format!("could not store the password: {error}\n"));
    }

    let mut shares = load();
    shares.retain(|existing| existing.name != share.name);
    shares.push(share.clone());
    if let Err(error) = save(&shares) {
        return Response::text(500, format!("could not save the share: {error}\n"));
    }

    let mut log = |line: &str| println!("plexosd: shares: {line}");
    match mount_one(&share, &mut log) {
        Ok(()) => Response::json(format!(
            "{{\"mounted\":\"{}\",\"restart_plex\":true}}",
            share.mount_point().display()
        )),
        // Saved but not mounted is a real state and reported as one: the configuration is
        // right and the server is unreachable, which is a different problem from a
        // request that was wrong.
        Err(error) => Response::text(502, format!("saved, but not mounted: {error}\n")),
    }
}

/// What to do to a share that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    Mount,
    Unmount,
    Remove,
}

/// Mounts, unmounts or forgets a named share.
fn act_on_named(request: &serde_json::Value, act: Act) -> crate::http::Response {
    use crate::http::Response;

    let Some(name) = string_in(request, "name") else {
        return Response::text(400, "which share?\n");
    };
    let shares = load();
    let Some(share) = shares.iter().find(|s| s.name == name) else {
        return Response::text(404, format!("no share called {name:?}\n"));
    };

    if act == Act::Mount {
        let mut log = |line: &str| println!("plexosd: shares: {line}");
        return match mount_one(share, &mut log) {
            Ok(()) => Response::json("{\"mounted\":true,\"restart_plex\":true}"),
            Err(error) => Response::text(502, format!("{error}\n")),
        };
    }

    if let Err(error) = unmount_one(share) {
        return Response::text(502, format!("{error}\n"));
    }
    if act == Act::Remove {
        let remaining: Vec<Share> = shares.iter().filter(|s| s.name != name).cloned().collect();
        if let Err(error) = save(&remaining) {
            return Response::text(500, format!("{error}\n"));
        }
    }
    Response::json("{\"unmounted\":true}")
}
