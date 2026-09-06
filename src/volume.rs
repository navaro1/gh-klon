//! The btrfs loop volume (spec §7 C15, R33; handoff §4, the `btrfs-volume`
//! row). This module holds the host layer: the sparse image, `mkfs.btrfs`, the
//! udisks calls, and the record that lets a later command find all of it again.
//! `src/cli/init.rs` owns the transaction that moves golden onto the volume.
//!
//! Golden lives on ext4 on most Linux laptops, and ext4 has no snapshot. A
//! loop volume gives the same laptop a small btrfs filesystem without a
//! partition and without `sudo`: udisks binds the image to a loop device and
//! mounts it for an active local session under `allow_active=yes`.
//!
//! The S1 spike (`docs/spikes/2026-btrfs-loop-volume.md`) measured every rule
//! below on the development laptop:
//!
//! | Rule | Reason |
//! |---|---|
//! | never store a loop device path | `loop-setup -f` takes the first free number, and snapd claims 40 of them at boot (S1 §9.4) |
//! | read the mount point from `findmnt` | a second volume with the same label gets a suffix (S1 §9) |
//! | treat `AlreadyMounted` as success | the desktop automounter wins the race in under 350 ms (S1 §5) |
//! | `loop-delete` only when `SetupByUID` is the caller | any other device raises a polkit password dialog (S1 §9) |
//! | seed a user-owned `klon/` through `--rootdir` | the mount root belongs to `root` under udisks (S1 §6) |
//! | `--no-user-interaction` on every udisks call | a polkit dialog would block klon and pop up on the desktop |
//!
//! The volume feature is Linux-only. macOS has APFS, which clones without a
//! volume, so `registry` never reaches this file there. One allow keeps the
//! module readable instead of a `cfg` on each item.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use crate::{journal, paths, time, Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The format version of `volume.json`. An unknown version fails closed.
pub const VERSION: u32 = 1;

/// The smallest image that `mkfs.btrfs` accepts with DUP metadata. Below this
/// the tool fails with a message about the metadata profile, which says
/// nothing to a user who asked for a small volume.
pub const MIN_BYTES: u64 = 128 * 1024 * 1024;

/// The name of the user-owned directory that the image seeds. The mount root
/// itself belongs to `root`, so every klon path starts here.
const WORK_DIR: &str = "klon";

/// The record of one volume. It lives in two places, because a command that
/// runs after a reboot cannot read the copy that sits on the unmounted volume:
///
/// | Copy | Path | Reader |
/// |---|---|---|
/// | repository | `<common>/klon/volume.json` | every command that can reach golden |
/// | registry | `<data>/volumes/<name>.json` | `add` when the volume is not mounted and golden's symlink dangles |
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    pub version: u32,
    /// The sparse image file. klon stores this and never a loop device: the
    /// device number is not stable across a reboot (S1 §9.4).
    pub image: PathBuf,
    /// The filesystem label, `klon-<repo>`. udisks derives the mount point
    /// from it, so a readable label gives a readable path.
    pub label: String,
    /// The mount point of the last attach. `findmnt` gives the live one, and
    /// klon rewrites this field when udisks picks another.
    pub mount: PathBuf,
    /// Golden's original path. It holds the symlink after the conversion.
    pub golden_old: PathBuf,
    /// `<mount>/klon/<repo>`: the subvolume that holds golden now.
    pub golden_new: PathBuf,
    /// The time of the conversion, RFC 3339 in UTC.
    pub created: String,
}

impl Volume {
    /// The plan of a conversion of `golden`, before anything exists on disk.
    pub fn plan(golden: &Path, mount: &Path) -> Result<Volume> {
        let slug = slug(golden);
        Ok(Volume {
            version: VERSION,
            image: image_path(golden)?,
            label: format!("klon-{slug}"),
            mount: mount.to_path_buf(),
            golden_old: golden.to_path_buf(),
            golden_new: work_dir(mount).join(&slug),
            created: time::now_rfc3339(),
        })
    }

    /// The record with the mount point that `findmnt` reported, and the golden
    /// path below it. udisks appends a suffix to the mount point when a second
    /// volume carries the same label, so the plan can be wrong here.
    pub fn at_mount(&self, mount: &Path) -> Volume {
        let name = self
            .golden_new
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(slug(&self.golden_old)));
        Volume {
            mount: mount.to_path_buf(),
            golden_new: work_dir(mount).join(name),
            ..self.clone()
        }
    }
}

// --- Paths ---------------------------------------------------------------------

/// `$XDG_DATA_HOME/klon`, else `~/.local/share/klon`. The images and the
/// registry live here, outside every repository.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir).join("klon"));
        }
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| Error::klon("neither XDG_DATA_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("klon"))
}

/// `<data>/<repo>-<hash>.img`.
///
/// The hash covers golden's whole path. Two repositories named `api` in two
/// directories would otherwise share one image, and the second `init --volume`
/// would refuse or, worse, attach the first one.
pub fn image_path(golden: &Path) -> Result<PathBuf> {
    Ok(data_dir()?.join(format!("{}-{}.img", slug(golden), digest(golden))))
}

/// `<mount>/klon`: the user-owned directory that the image seeds.
pub fn work_dir(mount: &Path) -> PathBuf {
    mount.join(WORK_DIR)
}

/// The repository name, reduced to the characters a filesystem label and a
/// file name both accept. udisks builds the mount point from the label, so a
/// space or a slash there would reach every printed path.
fn slug(golden: &Path) -> String {
    let raw = golden
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    let cleaned = cleaned.trim_matches(['.', '-']).to_string();
    if cleaned.is_empty() {
        "repo".to_string()
    } else {
        cleaned
    }
}

/// Eight hex digits of golden's path.
fn digest(golden: &Path) -> String {
    Sha256::digest(golden.as_os_str().as_encoded_bytes())
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

// --- The record ----------------------------------------------------------------

/// `<common>/klon/volume.json`.
pub fn record_path(common: &Path) -> PathBuf {
    paths::absolute(common)
        .unwrap_or_else(|_| common.to_path_buf())
        .join("klon")
        .join("volume.json")
}

/// `<data>/volumes/<name>.json`, the copy that survives an unmounted volume.
fn registry_path(golden_old: &Path) -> Result<PathBuf> {
    Ok(data_dir()?
        .join("volumes")
        .join(format!("{}.json", journal::name_for(golden_old))))
}

/// The record of the repository whose common directory is `common`, or None.
pub fn read(common: &Path) -> Result<Option<Volume>> {
    read_file(&record_path(common))
}

/// Read one record file. A missing file is no record. An unknown version fails
/// closed, like the journal and the probe cache.
fn read_file(path: &Path) -> Result<Option<Volume>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(Error::io(format!("read {}", path.display()))(err)),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| Error::klon(format!("{} is not valid JSON: {err}", path.display())))?;
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(v) if v == u64::from(VERSION) => {}
        found => {
            return Err(Error::klon(format!(
                "unknown volume record version {} in {}; upgrade klon",
                found.map_or("(missing)".to_string(), |v| v.to_string()),
                path.display()
            )))
        }
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|err| Error::klon(format!("{} is not a volume record: {err}", path.display())))
}

/// Write both copies of the record. Each write is atomic: a temporary file in
/// the same directory, then one rename.
pub fn write(common: &Path, record: &Volume) -> Result<()> {
    write_file(&record_path(common), record)?;
    write_file(&registry_path(&record.golden_old)?, record)
}

fn write_file(path: &Path, record: &Volume) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::klon(format!("{} has no parent directory", path.display())))?;
    fs::create_dir_all(dir).map_err(Error::io(format!("create {}", dir.display())))?;
    let text = serde_json::to_string_pretty(record)
        .map_err(|err| Error::klon(format!("serialize the volume record: {err}")))?;
    let temp = dir.join(format!(".volume.{}.tmp", std::process::id()));
    fs::write(&temp, text.as_bytes()).map_err(Error::io(format!("write {}", temp.display())))?;
    if let Err(err) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(Error::io(format!("write {}", path.display()))(err));
    }
    Ok(())
}

/// Delete both copies of the record. A missing file is not an error.
pub fn forget(common: &Path, golden_old: &Path) -> Result<()> {
    for path in [record_path(common), registry_path(golden_old)?] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(Error::io(format!("delete {}", path.display()))(err)),
        }
    }
    Ok(())
}

/// The record that covers `cwd`, from the registry alone.
///
/// The repository copy is unreachable while the volume is down: it sits on the
/// volume, and golden's symlink then points at nothing. `add` therefore looks
/// here first, before its first `git` call.
///
/// Three shapes match, in this order:
///
/// | Shape | Who stands there |
/// |---|---|
/// | an ancestor of `cwd` is a converted golden | the usual caller, inside the repository at its old path |
/// | `cwd` is on the volume, below a recorded golden | a caller who followed the symlink, or a shell left there |
/// | a recorded golden sits **below** `cwd` | a caller who could not enter the dangling symlink and ran the command from the directory above it |
///
/// The first shape costs one failed `stat` per ancestor and reads no
/// directory. The other two read the registry, which holds one small file per
/// converted repository.
///
/// An ambiguous third match gives None with one line on stderr, because klon
/// must not guess which repository the user meant.
pub fn find_for(cwd: &Path) -> Result<Option<Match>> {
    let here = paths::absolute(cwd)?;
    for ancestor in here.ancestors() {
        if let Some(record) = read_file(&registry_path(ancestor)?)? {
            return Ok(Some(Match {
                record,
                below: false,
            }));
        }
    }
    let dir = data_dir()?.join("volumes");
    let read = match fs::read_dir(&dir) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(Error::io(format!("read {}", dir.display()))(err)),
    };
    let mut below: Vec<Volume> = Vec::new();
    for item in read {
        let item = item.map_err(Error::io(format!("read {}", dir.display())))?;
        let path = item.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(record) = read_file(&path)? else {
            continue;
        };
        if here.starts_with(&record.golden_new) {
            return Ok(Some(Match {
                record,
                below: false,
            }));
        }
        if record.golden_old.starts_with(&here) {
            below.push(record);
        }
    }
    match below.len() {
        0 => Ok(None),
        1 => Ok(below.pop().map(|record| Match {
            record,
            below: true,
        })),
        _ => {
            eprintln!(
                "klon: {} holds {} klon volumes; run the command inside one of them",
                here.display(),
                below.len()
            );
            Ok(None)
        }
    }
}

/// One record and how the working directory reached it.
pub struct Match {
    pub record: Volume,
    /// True when golden sits **below** the working directory. The caller then
    /// stands outside the repository, which is what a dangling symlink leaves
    /// after a reboot: no shell can enter a path that points at nothing.
    pub below: bool,
}

// --- Attach --------------------------------------------------------------------

/// Attach and mount the volume of the repository at or below `cwd` when it is
/// down. Every command that touches a repository calls this before its first
/// `git` call (S1 §9.4: the first `add` after a reboot re-runs `loop-setup`
/// and `mount`).
///
/// The answer is the working directory to use. It differs from `cwd` in one
/// case: the volume was down and golden sits below `cwd`. No shell can stand
/// in a repository whose symlink points at nothing, so the user runs the
/// command from the directory above it, and klon steps in once the volume is
/// back. klon prints that step, and it takes it only for the one repository
/// that the registry names below `cwd`.
///
/// A repository without a volume costs one failed `stat` per ancestor of `cwd`
/// and no subprocess.
pub fn ensure_attached(cwd: &Path) -> Result<PathBuf> {
    let here = cwd.to_path_buf();
    let Some(found) = find_for(cwd)? else {
        return Ok(here);
    };
    if found.record.golden_new.is_dir() {
        return Ok(here);
    }
    let live = attach(&found.record)?;
    if live != found.record {
        // udisks mounted the volume somewhere else, so golden's symlink now
        // points at nothing. Both it and the record are rewritten before the
        // first git command runs.
        repoint(&live)?;
    }
    if !found.below {
        return Ok(here);
    }
    std::env::set_current_dir(&live.golden_old)
        .map_err(Error::io(format!("enter {}", live.golden_old.display())))?;
    eprintln!(
        "klon: the volume was down, so klon ran the command in {}",
        live.golden_old.display()
    );
    Ok(live.golden_old)
}

/// Bring the volume up and answer with the record that names the live mount.
///
/// The device number is resolved from the image every time, never stored. A
/// mount that the automounter already made counts as success. The call writes
/// nothing: `ensure_attached` owns the symlink and the record, and
/// `init --volume` has neither of them yet when it calls this.
pub fn attach(record: &Volume) -> Result<Volume> {
    if !record.image.is_file() {
        return Err(Error::klon(format!(
            "the klon volume image {} is gone; run gh klon init --volume --undo \
             to put golden back, or restore the image",
            record.image.display()
        )));
    }
    let device = match loop_device(&record.image)? {
        Some(device) => device,
        None => loop_setup(&record.image)?,
    };
    if mount_point(&device).is_none() {
        mount(&device)?;
    }
    let mount = mount_point(&device).ok_or_else(|| {
        Error::klon(format!(
            "{device} carries {} but no mount point appeared; \
             run udisksctl mount -b {device} by hand",
            record.image.display()
        ))
    })?;
    eprintln!(
        "klon: attached {} at {}",
        record.image.display(),
        mount.display()
    );
    Ok(record.at_mount(&mount))
}

/// Point golden's symlink at the live path and store the new record.
///
/// udisks appends a suffix to a mount point whose label is already taken, so a
/// volume can come back at another path. The symlink then points at nothing
/// and every git command would fail; this rewrites it and both record copies.
fn repoint(live: &Volume) -> Result<()> {
    eprintln!(
        "klon: the volume came back at {}; klon repoints {}",
        live.mount.display(),
        live.golden_old.display()
    );
    let link = &live.golden_old;
    if fs::symlink_metadata(link).is_ok_and(|m| m.is_symlink()) {
        fs::remove_file(link).map_err(Error::io(format!("delete {}", link.display())))?;
    }
    std::os::unix::fs::symlink(&live.golden_new, link)
        .map_err(Error::io(format!("link {}", link.display())))?;
    // The repository copy sits behind the symlink that this call just fixed,
    // so its path is resolved now and not from the stale record.
    let common = crate::git::common_dir_of_main(link)?;
    write(&common, live)
}

// --- The host tools ------------------------------------------------------------

/// The loop device that carries `image`, or None.
///
/// `/sys/block/loop*/loop/backing_file` answers without a subprocess and
/// without `/usr/sbin` on PATH. `losetup -j` is the documented command (S1
/// §9.4) and stands behind it for a host whose sysfs does not answer.
pub fn loop_device(image: &Path) -> Result<Option<String>> {
    let wanted = fs::canonicalize(image).unwrap_or_else(|_| image.to_path_buf());
    if let Ok(read) = fs::read_dir("/sys/block") {
        for item in read.flatten() {
            let name = item.file_name();
            if !name.to_string_lossy().starts_with("loop") {
                continue;
            }
            let file = item.path().join("loop").join("backing_file");
            let Ok(text) = fs::read_to_string(&file) else {
                continue;
            };
            if Path::new(text.trim_end_matches('\n')) == wanted {
                return Ok(Some(format!("/dev/{}", name.to_string_lossy())));
            }
        }
        return Ok(None);
    }
    let Some(tool) = losetup() else {
        return Ok(None);
    };
    let out = Command::new(tool)
        .arg("-j")
        .arg(image)
        .stdin(Stdio::null())
        .output()
        .map_err(Error::io("run losetup"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .next()
        .and_then(|line| line.split_once(':'))
        .map(|(device, _)| device.trim().to_string()))
}

/// `losetup` on PATH, else in the two directories that hold it on a system
/// whose PATH leaves `sbin` out.
fn losetup() -> Option<PathBuf> {
    crate::probe::tool_path("losetup").or_else(|| {
        ["/usr/sbin/losetup", "/sbin/losetup"]
            .iter()
            .find_map(|p| crate::probe::executable(Path::new(p)))
    })
}

/// `udisksctl loop-setup -f <image>`. The answer is the device it mapped.
pub fn loop_setup(image: &Path) -> Result<String> {
    let reply = udisks(&["loop-setup", "-f", &text(image)?])?;
    device_in(&reply).ok_or_else(|| {
        Error::klon(format!(
            "udisksctl loop-setup gave no device for {}: {}",
            image.display(),
            reply.trim()
        ))
    })
}

/// `udisksctl mount -b <device>`. `AlreadyMounted` is success: the desktop
/// automounter usually wins the race (S1 §5).
pub fn mount(device: &str) -> Result<()> {
    match udisks(&["mount", "-b", device]) {
        Ok(_) => Ok(()),
        Err(err) if err.to_string().contains("AlreadyMounted") => Ok(()),
        Err(err) => Err(err),
    }
}

/// `udisksctl unmount -b <device>`.
pub fn unmount(device: &str) -> Result<()> {
    match udisks(&["unmount", "-b", device]) {
        Ok(_) => Ok(()),
        Err(err) if err.to_string().contains("NotMounted") => Ok(()),
        Err(err) => Err(err),
    }
}

/// `udisksctl loop-delete -b <device>`, and only for a device this user set
/// up. udisks asks for a password for any other one (S1 §9), and klon never
/// raises a password dialog.
pub fn loop_delete(device: &str) -> Result<bool> {
    if !setup_by_caller(device) {
        return Ok(false);
    }
    udisks(&["loop-delete", "-b", device]).map(|_| true)
}

/// True when `udisksctl info` reports the caller in `SetupByUID`.
pub fn setup_by_caller(device: &str) -> bool {
    // SAFETY: `getuid` reads a process property and cannot fail.
    let me = unsafe { libc::getuid() };
    let Ok(text) = udisks(&["info", "-b", device]) else {
        return false;
    };
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("SetupByUID:"))
        .any(|value| value.trim().parse::<u32>() == Ok(me))
}

/// The mount point of `device`, from `findmnt`. klon never computes the path:
/// a second volume with the same label gets a suffix (S1 §9).
pub fn mount_point(device: &str) -> Option<PathBuf> {
    let out = Command::new("findmnt")
        .args(["-n", "-o", "TARGET", device])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
}

/// Run one `udisksctl` subcommand.
///
/// Three guards keep a polkit password dialog out of the way.
/// `--no-user-interaction` tells udisks to fail instead of asking.
/// `SUDO_ASKPASS=/bin/false` and a closed stdin block the two fallbacks that a
/// helper could still use. An active local session needs none of them
/// (`allow_active=yes`), so they cost nothing on the normal path.
fn udisks(args: &[&str]) -> Result<String> {
    let (subcommand, rest) = args
        .split_first()
        .ok_or_else(|| Error::klon("udisksctl needs a subcommand"))?;
    let mut command = Command::new("udisksctl");
    command.arg(subcommand);
    // `info` is a read and takes no authorization flag.
    if *subcommand != "info" {
        command.arg("--no-user-interaction");
    }
    let out = command
        .args(rest)
        .env("SUDO_ASKPASS", "/bin/false")
        .stdin(Stdio::null())
        .output()
        .map_err(Error::io(format!("run udisksctl {subcommand}")))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(Error::klon(format!(
        "udisksctl {subcommand} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

/// The `/dev/loopN` path in a `loop-setup` reply.
fn device_in(reply: &str) -> Option<String> {
    let at = reply.find("/dev/loop")?;
    let rest = &reply[at..];
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '/')
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// A path as a `&str`. udisks takes text arguments, so a path that is not
/// UTF-8 has no way through.
fn text(path: &Path) -> Result<String> {
    path.to_str().map(str::to_string).ok_or_else(|| {
        Error::klon(format!(
            "{} is not valid UTF-8, and udisksctl takes text paths",
            path.display()
        ))
    })
}

// --- The session gate ----------------------------------------------------------

/// Refuse a session that polkit does not call active and local.
///
/// The udisks policy grants `loop-setup` and `filesystem-mount` with
/// `allow_active=yes` and nothing else (S1 §11). An ssh session or a headless
/// runner therefore gets `auth_admin`, which means a password dialog. klon
/// tells the user that before it writes anything.
///
/// A host without `loginctl` cannot answer, so klon goes on: every udisks call
/// carries `--no-user-interaction` and fails in a second instead of waiting.
pub fn refuse_a_remote_session() -> Result<()> {
    let Some(loginctl) = crate::probe::tool_path("loginctl") else {
        eprintln!("klon: loginctl is not on PATH, so klon cannot check the session");
        return Ok(());
    };
    for id in session_ids(&loginctl) {
        if let Some(true) = active_and_local(&loginctl, &id) {
            return Ok(());
        }
    }
    Err(Error::klon(
        "gh klon init --volume needs an active local session: udisks asks for a \
         password in an ssh session and on a headless host. Run it from the \
         desktop session, or run gh klon init on a btrfs filesystem.",
    ))
}

/// The session ids to test: the caller's own session first, then every session
/// of this user. `XDG_SESSION_ID` is empty in a process that a service started,
/// and `/proc/self/sessionid` holds the audit id, which is not always the
/// logind id, so the list is the last word.
fn session_ids(loginctl: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(id) = std::env::var_os("XDG_SESSION_ID") {
        let id = id.to_string_lossy().into_owned();
        if !id.is_empty() {
            ids.push(id);
        }
    }
    // SAFETY: `getuid` reads a process property and cannot fail.
    let me = unsafe { libc::getuid() };
    let Ok(out) = Command::new(loginctl)
        .args(["list-sessions", "--no-legend"])
        .stdin(Stdio::null())
        .output()
    else {
        return ids;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut fields = line.split_whitespace();
        let (Some(id), Some(uid)) = (fields.next(), fields.next()) else {
            continue;
        };
        if uid.parse::<u32>() == Ok(me) && !ids.iter().any(|seen| seen == id) {
            ids.push(id.to_string());
        }
    }
    ids
}

/// True when `loginctl show-session` calls the session active and local, false
/// when it calls it neither, None when the session is unknown.
fn active_and_local(loginctl: &Path, id: &str) -> Option<bool> {
    let out = Command::new(loginctl)
        .args(["show-session", id, "-p", "Active", "-p", "Remote"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.contains("Active=yes") && text.contains("Remote=no"))
}

// --- The image -----------------------------------------------------------------

/// `<size>` as a byte count. `4G`, `500M`, `2T`, `512K`, and a plain number of
/// bytes are accepted. The suffix is a power of 1024, as `truncate` reads it.
pub fn parse_size(text: &str) -> Result<u64> {
    let trimmed = text.trim();
    let (digits, shift) = match trimmed.chars().last() {
        Some('K' | 'k') => (&trimmed[..trimmed.len() - 1], 10),
        Some('M' | 'm') => (&trimmed[..trimmed.len() - 1], 20),
        Some('G' | 'g') => (&trimmed[..trimmed.len() - 1], 30),
        Some('T' | 't') => (&trimmed[..trimmed.len() - 1], 40),
        _ => (trimmed, 0),
    };
    let value: u64 = digits
        .trim()
        .parse()
        .map_err(|_| Error::klon(format!("{text} is not a size; write it as 4G, 500M, or 2T")))?;
    let bytes = value
        .checked_shl(shift)
        .filter(|_| value < (1u64 << (63 - shift)))
        .ok_or_else(|| Error::klon(format!("the size {text} does not fit in 64 bits")))?;
    if bytes < MIN_BYTES {
        return Err(Error::klon(format!(
            "the size {text} is below the {} MiB that mkfs.btrfs needs",
            MIN_BYTES / (1024 * 1024)
        )));
    }
    Ok(bytes)
}

/// Create the sparse image. The file reports its full size and uses no disk
/// space until klon writes into it (S1 §4.1).
pub fn create_image(image: &Path, bytes: u64) -> Result<()> {
    let dir = image
        .parent()
        .ok_or_else(|| Error::klon(format!("{} has no parent directory", image.display())))?;
    fs::create_dir_all(dir).map_err(Error::io(format!("create {}", dir.display())))?;
    let file = fs::File::options()
        .write(true)
        .create_new(true)
        .open(image)
        .map_err(Error::io(format!("create {}", image.display())))?;
    file.set_len(bytes)
        .map_err(Error::io(format!("size {}", image.display())))
}

/// `mkfs.btrfs -L <label> --rootdir <seed> <image>`.
///
/// The seed holds one empty directory that this user owns. `--rootdir` keeps
/// the owner, and the mount root itself belongs to `root` under udisks, so
/// without the seed the volume would have no writable path (S1 §6).
pub fn mkfs(image: &Path, label: &str, seed: &Path) -> Result<()> {
    let tool = crate::backend::btrfs::mkfs_tool()
        .ok_or_else(|| Error::klon(crate::backend::btrfs::install_lines()))?;
    let work = work_dir(seed);
    fs::create_dir_all(&work).map_err(Error::io(format!("create {}", work.display())))?;
    let out = Command::new(&tool)
        .arg("-q")
        .args(["-L", label])
        .arg("--rootdir")
        .arg(seed)
        .arg(image)
        .stdin(Stdio::null())
        .output()
        .map_err(Error::io(format!("run {}", tool.display())));
    let _ = fs::remove_dir_all(seed);
    let out = out?;
    if out.status.success() {
        return Ok(());
    }
    Err(Error::klon(format!(
        "mkfs.btrfs failed on {}: {}",
        image.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_takes_a_power_of_1024_suffix() {
        assert_eq!(parse_size("4G").unwrap(), 4 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("500M").unwrap(), 500 * 1024 * 1024);
        assert_eq!(parse_size("2T").unwrap(), 2 * 1024u64.pow(4));
        assert_eq!(parse_size(" 1g ").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("268435456").unwrap(), 256 * 1024 * 1024);
    }

    #[test]
    fn a_size_that_mkfs_cannot_use_is_refused() {
        for bad in ["", "4", "4X", "-1G", "1M", "0", "G", "99999999T"] {
            assert!(parse_size(bad).is_err(), "{bad} must be refused");
        }
    }

    /// The label reaches the mount path, so it may hold no space and no slash.
    #[test]
    fn the_slug_holds_only_label_characters() {
        assert_eq!(slug(Path::new("/home/u/work/gh-klon")), "gh-klon");
        assert_eq!(slug(Path::new("/home/u/work/my repo")), "my-repo");
        assert_eq!(slug(Path::new("/home/u/work/..hidden..")), "hidden");
        assert_eq!(slug(Path::new("/")), "repo");
        let long = slug(Path::new(
            "/a/0123456789012345678901234567890123456789012345678901234567890123456789",
        ));
        assert_eq!(long.len(), 48);
    }

    /// Two repositories with one name must not share an image.
    #[test]
    fn the_image_name_separates_two_repositories_with_one_name() {
        let a = digest(Path::new("/home/u/one/api"));
        let b = digest(Path::new("/home/u/two/api"));
        assert_ne!(a, b);
        assert_eq!(a.len(), 8);
        assert_eq!(a, digest(Path::new("/home/u/one/api")));
    }

    #[test]
    fn the_device_comes_out_of_the_loop_setup_reply() {
        assert_eq!(
            device_in("Mapped file /home/u/.local/share/klon/x.img as /dev/loop57.\n").as_deref(),
            Some("/dev/loop57")
        );
        assert_eq!(device_in("nothing here"), None);
    }

    /// A record that moved to another mount point must carry golden with it.
    #[test]
    fn a_new_mount_point_moves_golden() {
        let record = Volume {
            version: VERSION,
            image: PathBuf::from("/data/klon/repo-0011aabb.img"),
            label: "klon-repo".to_string(),
            mount: PathBuf::from("/media/u/klon-repo"),
            golden_old: PathBuf::from("/home/u/work/repo"),
            golden_new: PathBuf::from("/media/u/klon-repo/klon/repo"),
            created: "2026-09-06T08:00:00Z".to_string(),
        };
        let moved = record.at_mount(Path::new("/media/u/klon-repo1"));
        assert_eq!(moved.golden_new, Path::new("/media/u/klon-repo1/klon/repo"));
        assert_eq!(moved.golden_old, record.golden_old);
        assert_eq!(record.at_mount(&record.mount), record);
    }

    /// An unknown version fails closed, so an old binary never rewrites a
    /// record that a newer klon wrote.
    #[test]
    fn an_unknown_record_version_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("volume.json");
        fs::write(&path, r#"{"version":99,"image":"/x.img"}"#).unwrap();
        let err = read_file(&path).expect_err("version 99 must fail");
        assert!(
            err.to_string().contains("unknown volume record version 99"),
            "unexpected error {err}"
        );
        assert!(read_file(&tmp.path().join("absent.json"))
            .unwrap()
            .is_none());
    }
}
