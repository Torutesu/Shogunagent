//! Self-install: a copy of the app opened from a disk image, from App Translocation or from a
//! user folder copies itself into Applications and reopens from there (owner request,
//! 2026-10-13). Launch at login, the bundled Memory API server and the updater all need a copy
//! that stays put, and the owner asked that the saved launch-at-login choice (default On) take
//! effect with no step from the user.
//!
//! This file is the pure half: where the running bundle is, what to do about it, how two bundle
//! versions compare, and the arguments the reopened copy receives. The macOS shell
//! (`apps/desktop/src-tauri/src/self_install.rs`) supplies the facts through [`Probe`], copies,
//! checks the code signature and reopens. Every path through [`decide`] that is not certain ends
//! in [`Action::Stay`]: the copy keeps running where it is.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

/// Passed to the reopened copy. A copy started with it never installs again, so a copy that
/// still looks uninstalled after reopening (for example, macOS translocated it again) cannot
/// reopen itself in a loop.
pub const RELAUNCH_ARG: &str = "--shogun-self-installed";
/// `--shogun-eject-volume=/Volumes/<name>`: the disk image the first copy ran from, ejected by
/// the reopened copy once the first one has quit.
pub const EJECT_ARG_PREFIX: &str = "--shogun-eject-volume=";
/// `--shogun-self-installed-at=<unix seconds>`: when the first copy reopened the installed one.
/// A restart forwards its arguments (the update restart, any `app.restart()`), so an eject
/// request is honoured only within [`EJECT_WINDOW_SECS`] of this, never by a later restart that
/// would eject a disk image the user mounted again.
pub const AT_ARG_PREFIX: &str = "--shogun-self-installed-at=";
/// How long after the hand-over the reopened copy may still eject the disk image.
pub const EJECT_WINDOW_SECS: u64 = 60;
/// Set to any non-empty value to keep a copy where it is (QA, or a developer running a signed
/// bundle from an unusual place).
pub const OPT_OUT_ENV: &str = "SHOGUN_SKIP_SELF_INSTALL";

/// Where a bundle lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// `/Applications/…` or `~/Applications/…`.
    Installed,
    /// On a mounted volume (`/Volumes/<name>/…`), normally the disk image it was shipped in.
    DiskImage { volume: PathBuf },
    /// macOS runs a quarantined app that was not moved from a randomised read-only path. The
    /// real location is resolved separately (see [`Facts::original`]).
    Translocated,
    /// A temporary folder (`/private/var/folders`, `/tmp`): an archive opened in place.
    Temporary,
    /// Anywhere else. `trashable` when it is the user's Downloads or Desktop, the two places a
    /// downloaded app is left by accident; a copy kept anywhere else is left alone.
    UserFolder { trashable: bool },
    /// A build tree (`…/target/…`), a relative path, or not a `.app` bundle at all.
    Development,
}

/// The `.app` bundle containing `exe` (`<name>.app/Contents/MacOS/<exe>`), or `None` for a bare
/// executable such as `cargo run`.
pub fn bundle_of(exe: &Path) -> Option<&Path> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    let is_app = bundle
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.len() > ".app".len() && name.ends_with(".app"));
    (exe.file_name().is_some()
        && macos.file_name() == Some(OsStr::new("MacOS"))
        && contents.file_name() == Some(OsStr::new("Contents"))
        && is_app)
        .then_some(bundle)
}

/// Where `bundle` (an absolute, canonical path) lives. `home` is the user's home folder.
pub fn classify(bundle: &Path, home: Option<&Path>) -> Location {
    let plain = bundle.is_absolute()
        && bundle
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    let names = || {
        bundle.components().filter_map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
    };
    let is_app = bundle
        .extension()
        .is_some_and(|extension| extension == "app");
    if !plain || !is_app || names().any(|name| name == "target") {
        return Location::Development;
    }
    if names().any(|name| name == "AppTranslocation") {
        return Location::Translocated;
    }
    let mut parts = names();
    if parts.next() == Some(OsStr::new("Volumes")) {
        if let Some(volume) = parts.next() {
            return Location::DiskImage {
                volume: Path::new("/Volumes").join(volume),
            };
        }
    }
    let user_applications = home.map(|home| home.join("Applications"));
    if bundle.starts_with("/Applications")
        || user_applications.is_some_and(|dir| bundle.starts_with(dir))
    {
        return Location::Installed;
    }
    if [
        "/private/var/folders",
        "/var/folders",
        "/private/tmp",
        "/tmp",
    ]
    .iter()
    .any(|dir| bundle.starts_with(dir))
    {
        return Location::Temporary;
    }
    let trashable = home.is_some_and(|home| {
        bundle.starts_with(home.join("Downloads")) || bundle.starts_with(home.join("Desktop"))
    });
    Location::UserFolder { trashable }
}

/// `CFBundleShortVersionString` and `CFBundleVersion` of a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleVersion {
    pub short: String,
    pub build: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Pre {
    Number(u64),
    Text(String),
}

/// `1.2.3`, `1.2`, `1.2.3-beta.1`, `1.2.3+abc` (build metadata ignored). Missing trailing parts
/// count as zero.
fn parse(text: &str) -> Option<(Vec<u64>, Vec<Pre>)> {
    let text = text.trim();
    let text = text.split('+').next().unwrap_or(text);
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (text, None),
    };
    let core = core
        .split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    let pre = match pre {
        None => Vec::new(),
        Some(pre) => pre
            .split('.')
            .map(|part| match part.parse::<u64>() {
                Ok(number) => Some(Pre::Number(number)),
                Err(_) if !part.is_empty() => Some(Pre::Text(part.to_owned())),
                Err(_) => None,
            })
            .collect::<Option<Vec<_>>>()?,
    };
    Some((core, pre))
}

/// Semver-style order of two version strings, or `None` when either does not parse.
pub fn compare_version_text(a: &str, b: &str) -> Option<Ordering> {
    let (a_core, a_pre) = parse(a)?;
    let (b_core, b_pre) = parse(b)?;
    for i in 0..a_core.len().max(b_core.len()) {
        let order = a_core
            .get(i)
            .copied()
            .unwrap_or(0)
            .cmp(&b_core.get(i).copied().unwrap_or(0));
        if order != Ordering::Equal {
            return Some(order);
        }
    }
    // A pre-release sorts before its release.
    Some(match (a_pre.is_empty(), b_pre.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            for (x, y) in a_pre.iter().zip(&b_pre) {
                let order = match (x, y) {
                    (Pre::Number(x), Pre::Number(y)) => x.cmp(y),
                    (Pre::Number(_), Pre::Text(_)) => Ordering::Less,
                    (Pre::Text(_), Pre::Number(_)) => Ordering::Greater,
                    (Pre::Text(x), Pre::Text(y)) => x.cmp(y),
                };
                if order != Ordering::Equal {
                    return Some(order);
                }
            }
            a_pre.len().cmp(&b_pre.len())
        }
    })
}

/// `0.0.0` (or `0`, `0.0`, with any pre-release): the version of a build no release tag named.
pub fn is_unversioned(short: &str) -> bool {
    parse(short).is_some_and(|(core, _)| core.iter().all(|part| *part == 0))
}

/// Order of two bundle versions: the marketing version first, then the build number. `None`
/// when the marketing versions cannot be compared; a build number that does not parse on
/// either side leaves the marketing order as the answer.
pub fn compare_bundle_versions(a: &BundleVersion, b: &BundleVersion) -> Option<Ordering> {
    match compare_version_text(&a.short, &b.short)? {
        Ordering::Equal => {
            Some(compare_version_text(&a.build, &b.build).unwrap_or(Ordering::Equal))
        }
        order => Some(order),
    }
}

/// What is at a candidate install path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Existing {
    /// `None` when something is there but it is not a readable bundle.
    pub bundle_id: Option<String>,
    pub version: Option<BundleVersion>,
    /// The team identifier in its signature, read without validating it (`None`: unsigned or
    /// ad hoc). Only a copy signed by the running copy's team is ever replaced.
    pub team: Option<String>,
    /// Its signature is intact and satisfies the running copy's designated requirement. A copy
    /// that fails this is repaired (replaced), never opened.
    pub valid: bool,
}

/// What the running copy knows about itself.
#[derive(Debug, Clone)]
pub struct Facts<'a> {
    /// The running `.app` (canonical), `None` when not running from a bundle.
    pub bundle: Option<&'a Path>,
    /// For a translocated copy, where the user actually put it, when macOS tells us.
    pub original: Option<&'a Path>,
    pub home: Option<&'a Path>,
    pub bundle_id: &'a str,
    /// `CFBundleName`: the installed copy is `<name>.app`.
    pub bundle_name: &'a str,
    pub version: Option<&'a BundleVersion>,
    /// Command-line arguments after the executable.
    pub args: &'a [OsString],
    pub opted_out: bool,
    pub debug_build: bool,
}

/// The facts that cost a system call, asked only once a copy is known to need installing.
pub trait Probe {
    /// What is at `path`, or `None` when nothing is.
    fn existing(&self, path: &Path) -> Option<Existing>;
    /// Another process of this app is running (any copy). The running one keeps the single
    /// instance; this copy then starts normally and hands over to it.
    fn other_instance_running(&self) -> bool;
    /// `/Applications` is writable without an administrator password.
    fn applications_writable(&self) -> bool;
    /// The team identifier of the running code when it satisfies a Developer ID requirement for
    /// that team, `None` otherwise (ad hoc, Apple Development, unsigned). Only a Developer ID
    /// build installs itself: an ad hoc build copied out of a build tree must never replace a
    /// notarized install.
    fn developer_id_team(&self) -> Option<String>;
}

/// What the running copy leaves behind once the installed copy is open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cleanup {
    /// Disk image volume for the reopened copy to eject (it is still in use until this copy
    /// quits).
    pub eject_volume: Option<PathBuf>,
    /// Downloads/Desktop copy for this copy to move to the Trash.
    pub trash: Option<PathBuf>,
}

/// Why a copy keeps running where it is. Static labels only: they are logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StayReason {
    Relaunched,
    OptedOut,
    DebugBuild,
    NotABundle,
    Development,
    AlreadyInstalled,
    OtherInstanceRunning,
    InvalidName,
    NoTarget,
    ForeignAppAtTarget,
    UnknownVersion,
    UnversionedBuild,
    NotDeveloperId,
    OtherSigner,
    TargetNotWritable,
}

impl StayReason {
    pub fn label(self) -> &'static str {
        match self {
            StayReason::Relaunched => "relaunched",
            StayReason::OptedOut => "opted out",
            StayReason::DebugBuild => "debug build",
            StayReason::NotABundle => "not a bundle",
            StayReason::Development => "development build",
            StayReason::AlreadyInstalled => "installed",
            StayReason::OtherInstanceRunning => "another copy is running",
            StayReason::InvalidName => "invalid bundle name",
            StayReason::NoTarget => "no install folder",
            StayReason::ForeignAppAtTarget => "another app at the install path",
            StayReason::UnknownVersion => "version unreadable",
            StayReason::UnversionedBuild => "unversioned build",
            StayReason::NotDeveloperId => "not a Developer ID build",
            StayReason::OtherSigner => "installed copy has another signer",
            StayReason::TargetNotWritable => "install folder not writable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Stay(StayReason),
    /// Copy `source` to `target` (swapping out an older copy when `replace`), then open it.
    Install {
        source: PathBuf,
        target: PathBuf,
        replace: bool,
        cleanup: Cleanup,
    },
    /// A copy at least as new is already installed: open that one instead.
    LaunchExisting {
        path: PathBuf,
        cleanup: Cleanup,
    },
}

fn installed_name(bundle_name: &str) -> Option<String> {
    let name = bundle_name.trim();
    let valid = !name.is_empty()
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\0')
        && name != ".."
        && name.len() <= 200;
    valid.then(|| format!("{name}.app"))
}

fn cleanup_for(origin: &Location, origin_path: &Path) -> Cleanup {
    match origin {
        Location::DiskImage { volume } => Cleanup {
            eject_volume: Some(volume.clone()),
            trash: None,
        },
        Location::UserFolder { trashable: true } => Cleanup {
            eject_volume: None,
            trash: Some(origin_path.to_path_buf()),
        },
        _ => Cleanup::default(),
    }
}

/// What the running copy should do. See the module docs; anything uncertain stays.
pub fn decide(facts: &Facts<'_>, probe: &impl Probe) -> Action {
    if is_relaunch(facts.args) {
        return Action::Stay(StayReason::Relaunched);
    }
    if facts.opted_out {
        return Action::Stay(StayReason::OptedOut);
    }
    if facts.debug_build {
        return Action::Stay(StayReason::DebugBuild);
    }
    let Some(bundle) = facts.bundle else {
        return Action::Stay(StayReason::NotABundle);
    };
    let location = classify(bundle, facts.home);
    match location {
        Location::Installed => return Action::Stay(StayReason::AlreadyInstalled),
        Location::Development => return Action::Stay(StayReason::Development),
        _ => {}
    }
    // A version that does not parse cannot be ordered against an installed copy, and must never
    // replace one. 0.0.0 is what an untagged release-workflow run builds (a QA build): installing
    // it would either clobber the real install or reopen an older one in its place.
    let Some(running) = facts
        .version
        .filter(|version| parse(&version.short).is_some())
    else {
        return Action::Stay(StayReason::UnknownVersion);
    };
    if is_unversioned(&running.short) {
        return Action::Stay(StayReason::UnversionedBuild);
    }
    let Some(team) = probe.developer_id_team() else {
        return Action::Stay(StayReason::NotDeveloperId);
    };
    if probe.other_instance_running() {
        return Action::Stay(StayReason::OtherInstanceRunning);
    }
    let Some(name) = installed_name(facts.bundle_name) else {
        return Action::Stay(StayReason::InvalidName);
    };
    // What to tidy up depends on where the user put the app, not on the translocated mirror.
    let (origin, origin_path) = match (&location, facts.original) {
        (Location::Translocated, Some(original)) => (classify(original, facts.home), original),
        (Location::Translocated, None) => (Location::Translocated, bundle),
        _ => (location.clone(), bundle),
    };
    match origin {
        // An installed copy macOS still translocates (quarantined, copied in by a tool rather
        // than Finder): reopen that same copy, which the shell first clears of quarantine.
        Location::Installed => {
            return Action::LaunchExisting {
                path: origin_path.to_path_buf(),
                cleanup: Cleanup::default(),
            }
        }
        Location::Development => return Action::Stay(StayReason::Development),
        _ => {}
    }
    let cleanup = cleanup_for(&origin, origin_path);

    let system = Path::new("/Applications").join(&name);
    let user = facts.home.map(|home| home.join("Applications").join(&name));
    let candidates =
        std::iter::once((system.clone(), true)).chain(user.clone().map(|path| (path, false)));
    for (path, is_system) in candidates {
        let Some(existing) = probe.existing(&path) else {
            continue;
        };
        if existing.bundle_id.as_deref() != Some(facts.bundle_id) {
            return Action::Stay(StayReason::ForeignAppAtTarget);
        }
        // The running version parses (checked above), so `None` means the installed one does not.
        let order = existing
            .version
            .as_ref()
            .and_then(|installed| compare_bundle_versions(installed, running));
        let current = matches!(order, Some(Ordering::Equal | Ordering::Greater));
        if current && existing.valid {
            return Action::LaunchExisting { path, cleanup };
        }
        // Older, an unreadable version, or a damaged signature: replace it, but only a copy
        // signed by the same team.
        if existing.team.as_deref() != Some(team.as_str()) {
            return Action::Stay(StayReason::OtherSigner);
        }
        if is_system && !probe.applications_writable() {
            return Action::Stay(StayReason::TargetNotWritable);
        }
        return Action::Install {
            source: bundle.to_path_buf(),
            target: path,
            replace: true,
            cleanup,
        };
    }
    let target = if probe.applications_writable() {
        system
    } else if let Some(user) = user {
        user
    } else {
        return Action::Stay(StayReason::NoTarget);
    };
    Action::Install {
        source: bundle.to_path_buf(),
        target,
        replace: false,
        cleanup,
    }
}

/// The copy was reopened by a self-install.
pub fn is_relaunch(args: &[OsString]) -> bool {
    args.iter().any(|arg| arg == RELAUNCH_ARG)
}

/// Arguments for the reopened copy. `now_secs` is the Unix time of the hand-over.
pub fn relaunch_args(cleanup: &Cleanup, now_secs: u64) -> Vec<OsString> {
    let mut args = vec![
        OsString::from(RELAUNCH_ARG),
        OsString::from(format!("{AT_ARG_PREFIX}{now_secs}")),
    ];
    if let Some(volume) = &cleanup.eject_volume {
        let mut arg = OsString::from(EJECT_ARG_PREFIX);
        arg.push(volume.as_os_str());
        args.push(arg);
    }
    args
}

/// The volume a reopened copy should eject: only with the relaunch marker, only within
/// [`EJECT_WINDOW_SECS`] of the hand-over (`now_secs` is the current Unix time), and only a direct
/// child of `/Volumes`.
pub fn eject_request(args: &[OsString], now_secs: u64) -> Option<PathBuf> {
    if !is_relaunch(args) {
        return None;
    }
    let at = args.iter().find_map(|arg| {
        arg.to_str()?
            .strip_prefix(AT_ARG_PREFIX)?
            .parse::<u64>()
            .ok()
    })?;
    // A few seconds of clock adjustment between the two processes is tolerated.
    if at > now_secs.saturating_add(5) || now_secs.saturating_sub(at) > EJECT_WINDOW_SECS {
        return None;
    }
    args.iter().find_map(|arg| {
        let volume = Path::new(arg.to_str()?.strip_prefix(EJECT_ARG_PREFIX)?);
        let mut parts = volume.components();
        let valid = parts.next() == Some(Component::RootDir)
            && parts.next() == Some(Component::Normal(OsStr::new("Volumes")))
            && matches!(parts.next(), Some(Component::Normal(_)))
            && parts.next().is_none();
        valid.then(|| volume.to_path_buf())
    })
}

/// `args` without the self-install arguments, for a restart that forwards its own arguments.
pub fn strip_relaunch_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    args.into_iter()
        .filter(|arg| {
            arg.to_str().is_none_or(|text| {
                text != RELAUNCH_ARG
                    && !text.starts_with(EJECT_ARG_PREFIX)
                    && !text.starts_with(AT_ARG_PREFIX)
            })
        })
        .collect()
}

/// The staging copy an install builds next to `<bundle_file>` (e.g. `ShogunAI.app`).
pub fn staging_name(bundle_file: &str, pid: u32) -> String {
    format!(".{bundle_file}.installing-{pid}")
}

/// The process that made a staging copy named `entry`, when it is one for `bundle_file`. An
/// install removes those whose process is gone (an install that crashed or lost power).
pub fn staging_pid(entry: &str, bundle_file: &str) -> Option<u32> {
    entry
        .strip_prefix('.')?
        .strip_prefix(bundle_file)?
        .strip_prefix(".installing-")?
        .parse()
        .ok()
}

/// Mount points of attached disk images, from `hdiutil info` output: the last tab-separated
/// field of each device line. Only these are ejected, never an external drive.
pub fn disk_image_mount_points(hdiutil_info: &str) -> Vec<PathBuf> {
    hdiutil_info
        .lines()
        .filter(|line| line.starts_with("/dev/"))
        .filter_map(|line| line.split('\t').next_back())
        .map(str::trim)
        .filter(|field| field.starts_with("/Volumes/"))
        .map(PathBuf::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::HashMap;

    const ID: &str = "com.syogun.shogunai";
    const HOME: &str = "/Users/me";
    const TEAM: &str = "ABCDE12345";

    fn v(short: &str, build: &str) -> BundleVersion {
        BundleVersion {
            short: short.into(),
            build: build.into(),
        }
    }

    struct Fake {
        existing: HashMap<PathBuf, Existing>,
        running: bool,
        writable: bool,
        team: Option<String>,
        probed: Cell<bool>,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                existing: HashMap::new(),
                running: false,
                writable: false,
                team: Some(TEAM.into()),
                probed: Cell::new(false),
            }
        }
    }

    impl Probe for Fake {
        fn existing(&self, path: &Path) -> Option<Existing> {
            self.probed.set(true);
            self.existing.get(path).cloned()
        }
        fn other_instance_running(&self) -> bool {
            self.probed.set(true);
            self.running
        }
        fn applications_writable(&self) -> bool {
            self.writable
        }
        fn developer_id_team(&self) -> Option<String> {
            self.probed.set(true);
            self.team.clone()
        }
    }

    fn writable() -> Fake {
        Fake {
            writable: true,
            ..Fake::default()
        }
    }

    fn ours(version: BundleVersion) -> Existing {
        Existing {
            bundle_id: Some(ID.into()),
            version: Some(version),
            team: Some(TEAM.into()),
            valid: true,
        }
    }

    fn facts<'a>(bundle: &'a Path, version: &'a BundleVersion, args: &'a [OsString]) -> Facts<'a> {
        Facts {
            bundle: Some(bundle),
            original: None,
            home: Some(Path::new(HOME)),
            bundle_id: ID,
            bundle_name: "ShogunAI",
            version: Some(version),
            args,
            opted_out: false,
            debug_build: false,
        }
    }

    fn install(source: &str, target: &str, replace: bool, cleanup: Cleanup) -> Action {
        Action::Install {
            source: source.into(),
            target: target.into(),
            replace,
            cleanup,
        }
    }

    const DMG: &str = "/Volumes/ShogunAI/ShogunAI.app";
    const SYSTEM: &str = "/Applications/ShogunAI.app";
    const USER: &str = "/Users/me/Applications/ShogunAI.app";

    #[test]
    fn bundle_of_requires_an_app_bundle_executable() {
        assert_eq!(
            bundle_of(Path::new(
                "/Applications/ShogunAI.app/Contents/MacOS/shogun"
            )),
            Some(Path::new("/Applications/ShogunAI.app"))
        );
        for exe in [
            "/Users/me/ShogunAI-/target/debug/shogun-desktop-spike",
            "/Applications/.app/Contents/MacOS/shogun",
            "/Applications/ShogunAI.app/Contents/Resources/shogun",
            "shogun",
        ] {
            assert_eq!(bundle_of(Path::new(exe)), None, "{exe}");
        }
    }

    #[test]
    fn classifies_every_location() {
        let home = Some(Path::new(HOME));
        let cases = [
            (SYSTEM, Location::Installed),
            ("/Applications/Utilities/ShogunAI.app", Location::Installed),
            (USER, Location::Installed),
            (
                DMG,
                Location::DiskImage {
                    volume: "/Volumes/ShogunAI".into(),
                },
            ),
            (
                "/Volumes/ShogunAI 1/ShogunAI.app",
                Location::DiskImage {
                    volume: "/Volumes/ShogunAI 1".into(),
                },
            ),
            (
                "/private/var/folders/x1/abc/T/AppTranslocation/0A1B/d/ShogunAI.app",
                Location::Translocated,
            ),
            (
                "/private/var/folders/x1/abc/T/ShogunAI.app",
                Location::Temporary,
            ),
            ("/private/tmp/ShogunAI.app", Location::Temporary),
            (
                "/Users/me/Downloads/ShogunAI.app",
                Location::UserFolder { trashable: true },
            ),
            (
                "/Users/me/Desktop/stuff/ShogunAI.app",
                Location::UserFolder { trashable: true },
            ),
            (
                "/Users/me/Documents/ShogunAI.app",
                Location::UserFolder { trashable: false },
            ),
            (
                "/Users/Shared/ShogunAI.app",
                Location::UserFolder { trashable: false },
            ),
            (
                "/Users/me/ShogunAI-/target/release/bundle/macos/ShogunAI.app",
                Location::Development,
            ),
            ("relative/ShogunAI.app", Location::Development),
            ("/Applications/../tmp/ShogunAI.app", Location::Development),
            ("/Users/me/Downloads/ShogunAI", Location::Development),
            // `/ApplicationsX` is not `/Applications`.
            (
                "/ApplicationsX/ShogunAI.app",
                Location::UserFolder { trashable: false },
            ),
        ];
        for (path, expected) in cases {
            assert_eq!(classify(Path::new(path), home), expected, "{path}");
        }
        // Without a home folder, ~/Applications and Downloads are unknown.
        assert_eq!(
            classify(Path::new(USER), None),
            Location::UserFolder { trashable: false }
        );
    }

    #[test]
    fn version_order() {
        use Ordering::*;
        for (a, b, expected) in [
            ("1.2.3", "1.2.3", Equal),
            ("1.2", "1.2.0", Equal),
            ("1.10.0", "1.9.9", Greater),
            ("0.2.4", "0.10.0", Less),
            ("1.0.0-beta.1", "1.0.0", Less),
            ("1.0.0-beta.2", "1.0.0-beta.10", Less),
            ("1.0.0-alpha", "1.0.0-alpha.1", Less),
            ("1.0.0-1", "1.0.0-alpha", Less),
            ("1.0.0+abc", "1.0.0", Equal),
            (" 2.0 ", "1.99", Greater),
        ] {
            assert_eq!(compare_version_text(a, b), Some(expected), "{a} vs {b}");
            assert_eq!(
                compare_version_text(b, a),
                Some(expected.reverse()),
                "{b} vs {a}"
            );
        }
        for bad in ["", "abc", "1..2", "1.x", "1.0-"] {
            assert_eq!(compare_version_text(bad, "1.0"), None, "{bad}");
        }
        assert_eq!(
            compare_bundle_versions(&v("1.0.0", "5"), &v("1.0.0", "12")),
            Some(Less)
        );
        assert_eq!(
            compare_bundle_versions(&v("1.0.1", "5"), &v("1.0.0", "12")),
            Some(Greater)
        );
        assert_eq!(
            compare_bundle_versions(&v("1.0.0", "x"), &v("1.0.0", "12")),
            Some(Equal)
        );
        assert_eq!(
            compare_bundle_versions(&v("x", "1"), &v("1.0.0", "1")),
            None
        );
    }

    #[test]
    fn fresh_install_from_a_disk_image_goes_to_applications_and_ejects() {
        let version = v("1.0.0", "1");
        let action = decide(&facts(Path::new(DMG), &version, &[]), &writable());
        assert_eq!(
            action,
            install(
                DMG,
                SYSTEM,
                false,
                Cleanup {
                    eject_volume: Some("/Volumes/ShogunAI".into()),
                    trash: None
                }
            )
        );
    }

    #[test]
    fn unwritable_applications_installs_for_the_user_without_a_password() {
        let version = v("1.0.0", "1");
        let action = decide(&facts(Path::new(DMG), &version, &[]), &Fake::default());
        assert!(
            matches!(action, Action::Install { ref target, replace: false, .. } if target == Path::new(USER))
        );
        let mut no_home = facts(Path::new(DMG), &version, &[]);
        no_home.home = None;
        assert_eq!(
            decide(&no_home, &Fake::default()),
            Action::Stay(StayReason::NoTarget)
        );
    }

    #[test]
    fn downloads_copy_is_installed_and_trashed_temp_copy_is_left() {
        let version = v("1.0.0", "1");
        let downloads = "/Users/me/Downloads/ShogunAI.app";
        assert_eq!(
            decide(&facts(Path::new(downloads), &version, &[]), &writable()),
            install(
                downloads,
                SYSTEM,
                false,
                Cleanup {
                    eject_volume: None,
                    trash: Some(downloads.into())
                }
            )
        );
        let documents = "/Users/me/Documents/ShogunAI.app";
        assert_eq!(
            decide(&facts(Path::new(documents), &version, &[]), &writable()),
            install(documents, SYSTEM, false, Cleanup::default())
        );
        let temp = "/private/var/folders/x1/T/ShogunAI.app";
        assert_eq!(
            decide(&facts(Path::new(temp), &version, &[]), &writable()),
            install(temp, SYSTEM, false, Cleanup::default())
        );
    }

    #[test]
    fn translocated_copy_uses_the_original_location_for_cleanup() {
        let version = v("1.0.0", "1");
        let mirror = "/private/var/folders/x1/T/AppTranslocation/0A1B/d/ShogunAI.app";
        let original = Path::new("/Users/me/Downloads/ShogunAI.app");
        let mut known = facts(Path::new(mirror), &version, &[]);
        known.original = Some(original);
        assert_eq!(
            decide(&known, &writable()),
            install(
                mirror,
                SYSTEM,
                false,
                Cleanup {
                    eject_volume: None,
                    trash: Some(original.into())
                }
            )
        );
        let dmg_original = Path::new(DMG);
        known.original = Some(dmg_original);
        assert!(matches!(
            decide(&known, &writable()),
            Action::Install { cleanup: Cleanup { eject_volume: Some(ref volume), trash: None }, .. }
                if volume == Path::new("/Volumes/ShogunAI")
        ));
        // Unknown original: install, touch nothing else.
        assert_eq!(
            decide(&facts(Path::new(mirror), &version, &[]), &writable()),
            install(mirror, SYSTEM, false, Cleanup::default())
        );
    }

    #[test]
    fn a_translocated_installed_copy_is_reopened_in_place() {
        let version = v("1.0.0", "1");
        let mirror = "/private/var/folders/x1/T/AppTranslocation/0A1B/d/ShogunAI.app";
        let mut known = facts(Path::new(mirror), &version, &[]);
        known.original = Some(Path::new(SYSTEM));
        assert_eq!(
            decide(&known, &writable()),
            Action::LaunchExisting {
                path: SYSTEM.into(),
                cleanup: Cleanup::default()
            }
        );
    }

    #[test]
    fn existing_copy_same_or_newer_is_opened_older_is_replaced() {
        let running = v("1.2.0", "7");
        let dmg_cleanup = Cleanup {
            eject_volume: Some("/Volumes/ShogunAI".into()),
            trash: None,
        };
        for installed in [v("1.2.0", "7"), v("1.3.0", "1"), v("1.2.0", "8")] {
            let mut probe = writable();
            probe.existing.insert(SYSTEM.into(), ours(installed));
            assert_eq!(
                decide(&facts(Path::new(DMG), &running, &[]), &probe),
                Action::LaunchExisting {
                    path: SYSTEM.into(),
                    cleanup: dmg_cleanup.clone()
                }
            );
        }
        for installed in [Some(v("1.1.9", "99")), Some(v("1.2.0", "6")), None] {
            let mut probe = writable();
            probe.existing.insert(
                SYSTEM.into(),
                Existing {
                    version: installed,
                    ..ours(v("0", "0"))
                },
            );
            assert_eq!(
                decide(&facts(Path::new(DMG), &running, &[]), &probe),
                install(DMG, SYSTEM, true, dmg_cleanup.clone())
            );
            // Same, but /Applications is not writable: never ask for a password.
            probe.writable = false;
            assert_eq!(
                decide(&facts(Path::new(DMG), &running, &[]), &probe),
                Action::Stay(StayReason::TargetNotWritable)
            );
        }
    }

    #[test]
    fn existing_user_copy_is_used_before_a_second_install() {
        let running = v("1.2.0", "7");
        let mut probe = writable();
        probe.existing.insert(USER.into(), ours(v("1.0.0", "1")));
        assert_eq!(
            decide(&facts(Path::new(DMG), &running, &[]), &probe),
            install(
                DMG,
                USER,
                true,
                Cleanup {
                    eject_volume: Some("/Volumes/ShogunAI".into()),
                    trash: None
                }
            )
        );
    }

    #[test]
    fn another_app_at_the_target_is_never_touched() {
        let running = v("1.0.0", "1");
        for other in [
            Existing {
                bundle_id: Some("com.example.other".into()),
                ..ours(v("9.0", "9"))
            },
            Existing {
                bundle_id: None,
                version: None,
                team: None,
                valid: false,
            },
        ] {
            let mut probe = writable();
            probe.existing.insert(SYSTEM.into(), other);
            assert_eq!(
                decide(&facts(Path::new(DMG), &running, &[]), &probe),
                Action::Stay(StayReason::ForeignAppAtTarget)
            );
        }
    }

    #[test]
    fn unknown_running_version_never_installs_or_replaces() {
        let version = v("1.0.0", "1");
        let mut probe = writable();
        probe.existing.insert(SYSTEM.into(), ours(v("1.0.0", "1")));
        let mut f = facts(Path::new(DMG), &version, &[]);
        f.version = None;
        assert_eq!(decide(&f, &probe), Action::Stay(StayReason::UnknownVersion));
        assert_eq!(
            decide(&f, &writable()),
            Action::Stay(StayReason::UnknownVersion)
        );
        // A running version that does not parse must not downgrade a real install.
        let unparsable = v("0.4 beta", "1");
        let mut probe = writable();
        probe.existing.insert(SYSTEM.into(), ours(v("1.2.0", "3")));
        assert_eq!(
            decide(&facts(Path::new(DMG), &unparsable, &[]), &probe),
            Action::Stay(StayReason::UnknownVersion)
        );
    }

    #[test]
    fn unversioned_qa_builds_never_install_or_reopen_another_copy() {
        for short in ["0.0.0", "0.0", "0", "0.0.0-rc.1"] {
            assert!(is_unversioned(short), "{short}");
            let version = v(short, short);
            for installed in [None, Some(v("0.0.0", "0.0.0")), Some(v("1.0.0", "1"))] {
                let mut probe = writable();
                if let Some(installed) = installed {
                    probe.existing.insert(SYSTEM.into(), ours(installed));
                }
                assert_eq!(
                    decide(&facts(Path::new(DMG), &version, &[]), &probe),
                    Action::Stay(StayReason::UnversionedBuild),
                    "{short}"
                );
            }
        }
        for short in ["0.0.1", "0.1.0", "1.0.0", "x"] {
            assert!(!is_unversioned(short), "{short}");
        }
    }

    #[test]
    fn only_a_developer_id_build_installs() {
        let version = v("1.0.0", "1");
        let probe = Fake {
            writable: true,
            team: None,
            ..Fake::default()
        };
        assert_eq!(
            decide(&facts(Path::new(DMG), &version, &[]), &probe),
            Action::Stay(StayReason::NotDeveloperId)
        );
    }

    #[test]
    fn only_a_copy_signed_by_the_same_team_is_replaced() {
        let running = v("1.2.0", "7");
        for team in [None, Some("ZZZZZ99999".to_owned())] {
            let mut probe = writable();
            probe.existing.insert(
                SYSTEM.into(),
                Existing {
                    team: team.clone(),
                    ..ours(v("1.0.0", "1"))
                },
            );
            assert_eq!(
                decide(&facts(Path::new(DMG), &running, &[]), &probe),
                Action::Stay(StayReason::OtherSigner),
                "{team:?}"
            );
        }
    }

    #[test]
    fn a_damaged_install_is_repaired_not_opened() {
        let running = v("1.2.0", "7");
        let cleanup = Cleanup {
            eject_volume: Some("/Volumes/ShogunAI".into()),
            trash: None,
        };
        for installed in [v("1.2.0", "7"), v("1.3.0", "1")] {
            let mut probe = writable();
            probe.existing.insert(
                SYSTEM.into(),
                Existing {
                    valid: false,
                    ..ours(installed)
                },
            );
            assert_eq!(
                decide(&facts(Path::new(DMG), &running, &[]), &probe),
                install(DMG, SYSTEM, true, cleanup.clone())
            );
        }
        // A damaged copy from another signer is still left alone.
        let mut probe = writable();
        probe.existing.insert(
            SYSTEM.into(),
            Existing {
                valid: false,
                team: None,
                ..ours(v("1.3.0", "1"))
            },
        );
        assert_eq!(
            decide(&facts(Path::new(DMG), &running, &[]), &probe),
            Action::Stay(StayReason::OtherSigner)
        );
    }

    #[test]
    fn installed_development_and_guarded_copies_stay_without_probing() {
        let version = v("1.0.0", "1");
        let relaunched = [OsString::from(RELAUNCH_ARG)];
        let cases: Vec<(Facts<'_>, StayReason)> = vec![
            (
                facts(Path::new(SYSTEM), &version, &[]),
                StayReason::AlreadyInstalled,
            ),
            (
                facts(Path::new(USER), &version, &[]),
                StayReason::AlreadyInstalled,
            ),
            (
                facts(
                    Path::new("/Users/me/ShogunAI-/target/release/bundle/macos/ShogunAI.app"),
                    &version,
                    &[],
                ),
                StayReason::Development,
            ),
            (
                facts(Path::new(DMG), &version, &relaunched),
                StayReason::Relaunched,
            ),
            (
                Facts {
                    opted_out: true,
                    ..facts(Path::new(DMG), &version, &[])
                },
                StayReason::OptedOut,
            ),
            (
                Facts {
                    debug_build: true,
                    ..facts(Path::new(DMG), &version, &[])
                },
                StayReason::DebugBuild,
            ),
            (
                Facts {
                    bundle: None,
                    ..facts(Path::new(DMG), &version, &[])
                },
                StayReason::NotABundle,
            ),
        ];
        for (f, reason) in cases {
            let probe = writable();
            assert_eq!(decide(&f, &probe), Action::Stay(reason));
            assert!(!probe.probed.get(), "{reason:?} must not probe");
        }
    }

    #[test]
    fn a_running_copy_keeps_the_single_instance() {
        let version = v("1.0.0", "1");
        let probe = Fake {
            running: true,
            writable: true,
            ..Fake::default()
        };
        assert_eq!(
            decide(&facts(Path::new(DMG), &version, &[]), &probe),
            Action::Stay(StayReason::OtherInstanceRunning)
        );
    }

    #[test]
    fn bundle_name_cannot_escape_applications() {
        let version = v("1.0.0", "1");
        for name in ["", "  ", "../evil", ".hidden", "a/b"] {
            let f = Facts {
                bundle_name: name,
                ..facts(Path::new(DMG), &version, &[])
            };
            assert_eq!(
                decide(&f, &writable()),
                Action::Stay(StayReason::InvalidName),
                "{name:?}"
            );
        }
    }

    #[test]
    fn relaunch_arguments_round_trip_and_reject_other_volumes() {
        const NOW: u64 = 1_800_000_000;
        let cleanup = Cleanup {
            eject_volume: Some("/Volumes/ShogunAI 1".into()),
            trash: None,
        };
        let args = relaunch_args(&cleanup, NOW);
        assert!(is_relaunch(&args));
        assert_eq!(
            eject_request(&args, NOW + 3),
            Some(PathBuf::from("/Volumes/ShogunAI 1"))
        );
        assert_eq!(
            relaunch_args(&Cleanup::default(), NOW),
            vec![
                OsString::from(RELAUNCH_ARG),
                OsString::from(format!("{AT_ARG_PREFIX}{NOW}"))
            ]
        );
        assert_eq!(
            eject_request(&relaunch_args(&Cleanup::default(), NOW), NOW),
            None
        );
        // The volume request is ignored without the relaunch marker, and only a direct child of
        // /Volumes is accepted.
        assert_eq!(
            eject_request(
                &[OsString::from("--shogun-eject-volume=/Volumes/ShogunAI")],
                NOW
            ),
            None
        );
        for bad in [
            "/Volumes",
            "/Volumes/",
            "/Volumes/a/b",
            "/Volumes/../",
            "/Volumes/..",
            "/System",
            "Volumes/a",
        ] {
            let args = [
                OsString::from(RELAUNCH_ARG),
                OsString::from(format!("{AT_ARG_PREFIX}{NOW}")),
                OsString::from(format!("{EJECT_ARG_PREFIX}{bad}")),
            ];
            assert_eq!(eject_request(&args, NOW), None, "{bad}");
        }
    }

    #[test]
    fn a_later_restart_never_ejects() {
        const NOW: u64 = 1_800_000_000;
        let cleanup = Cleanup {
            eject_volume: Some("/Volumes/ShogunAI".into()),
            trash: None,
        };
        let args = relaunch_args(&cleanup, NOW);
        assert!(eject_request(&args, NOW + EJECT_WINDOW_SECS).is_some());
        // Forwarded by a restart minutes later, or stamped in the future: ignored.
        assert_eq!(eject_request(&args, NOW + EJECT_WINDOW_SECS + 1), None);
        assert_eq!(eject_request(&args, NOW - 60), None);
        // Without the time stamp (an older relaunch): ignored.
        let unstamped: Vec<_> = args
            .iter()
            .filter(|arg| !arg.to_string_lossy().starts_with(AT_ARG_PREFIX))
            .cloned()
            .collect();
        assert_eq!(eject_request(&unstamped, NOW), None);
        // A restart that forwards its arguments drops the self-install ones and keeps the rest.
        let mut forwarded = vec![OsString::from("--other")];
        forwarded.extend(args);
        assert_eq!(
            strip_relaunch_args(forwarded),
            vec![OsString::from("--other")]
        );
    }

    #[test]
    fn staging_copies_are_recognised_by_name() {
        let name = staging_name("ShogunAI.app", 4242);
        assert_eq!(name, ".ShogunAI.app.installing-4242");
        assert_eq!(staging_pid(&name, "ShogunAI.app"), Some(4242));
        for other in [
            "ShogunAI.app",
            ".ShogunAI.app.installing-",
            ".ShogunAI.app.installing-12x",
            ".Other.app.installing-1",
            ".ShogunAI.app.old-1",
        ] {
            assert_eq!(staging_pid(other, "ShogunAI.app"), None, "{other}");
        }
    }

    #[test]
    fn disk_image_mount_points_come_from_device_lines_only() {
        let info = "framework       : 671\n\
            ================================================\n\
            image-path      : /Users/me/Downloads/ShogunAI.dmg\n\
            image-type      : read-only disk image\n\
            /dev/disk4          \tGUID_partition_scheme          \t\n\
            /dev/disk4s1        \tApple_HFS                      \t/Volumes/ShogunAI\n\
            ================================================\n\
            image-path      : /Users/me/other.dmg\n\
            /dev/disk5s1        \tApple_APFS                     \t/Volumes/Other Disk\n";
        assert_eq!(
            disk_image_mount_points(info),
            vec![
                PathBuf::from("/Volumes/ShogunAI"),
                PathBuf::from("/Volumes/Other Disk")
            ]
        );
        assert!(disk_image_mount_points("").is_empty());
    }
}
