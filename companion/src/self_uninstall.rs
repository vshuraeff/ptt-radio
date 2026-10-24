//! self-uninstall: an installed companion removes its own install directory once
//! the ptt-radio mod is gone. checked at startup after self-update and, with
//! --launch, again after the game exits.
//!
//! it acts only on the layout the installer writes: the executable in
//! `<user-data>/ptt-radio-companion/` beside an `install.json` of version 1 whose
//! `mods_dir` is `<user-data>/mods`. a missing marker means a manual or
//! development install and is never touched; any other doubt leaves every file
//! in place with one warning. a check that finds the mod writes `mod-seen` beside
//! the marker, and only a mod missing after that counts as removed: before it, the
//! mod is simply not installed yet. the removal takes the marker directory and
//! nothing above it. opt-out: --no-self-uninstall or PTT_RADIO_NO_SELF_UNINSTALL=1.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::self_update::env_flag;

const INSTALL_DIR: &str = "ptt-radio-companion";
const MARKER: &str = "install.json";
const INSTALLED_BY: &str = "ptt-radio installer";
const MOD_LIST: &str = "mod-list.json";
const MOD: &str = "ptt-radio";
const SEEN: &str = "mod-seen";
const OPT_OUT_ENV: &str = "PTT_RADIO_NO_SELF_UNINSTALL";
const MARKER_LIMIT: u64 = 64 << 10;

#[derive(Debug, PartialEq)]
enum Verdict {
    /// not an installer layout, removal configured off, the mod is installed, or it
    /// was never seen installed
    Keep,
    /// an installer layout with something in doubt: nothing is removed
    Warn(String),
    /// the mod is gone: the reason for the one line
    Remove(String),
}

/// true when the install directory was removed and the companion should exit
pub fn check(opt_out: bool) -> bool {
    if opt_out || env_flag(OPT_OUT_ENV) {
        return false;
    }
    let Some(dir) = install_dir() else {
        return false;
    };
    match decide(&dir) {
        Verdict::Keep => false,
        Verdict::Warn(doubt) => {
            eprintln!("self-uninstall: {doubt}; nothing removed");
            false
        }
        Verdict::Remove(reason) => {
            match remove(&dir, |dir| self_replace::self_delete_outside_path(dir)) {
                Ok(()) => {
                    println!("self-uninstall: {reason}, removed {}", dir.display());
                    true
                }
                Err(err) => {
                    eprintln!(
                        "self-uninstall: {reason}, but removing {} failed: {err}",
                        dir.display()
                    );
                    false
                }
            }
        }
    }
}

/// the directory of the running executable, links resolved
fn install_dir() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?.canonicalize().ok()?;
    exe.parent().map(Path::to_path_buf)
}

fn decide(dir: &Path) -> Verdict {
    if dir.file_name().is_none_or(|name| name != INSTALL_DIR) {
        return Verdict::Keep;
    }
    let marker = dir.join(MARKER);
    let mods = match fs::symlink_metadata(&marker) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Verdict::Keep,
        Err(err) => return Verdict::Warn(format!("cannot read {}: {err}", marker.display())),
        Ok(meta) if !meta.is_file() => {
            return Verdict::Warn(format!("{} is not a plain file", marker.display()));
        }
        Ok(_) => match read_marker(&marker) {
            Ok(mods) => mods,
            Err(err) => return Verdict::Warn(format!("{}: {err}", marker.display())),
        },
    };
    let Some(user_data) = dir.parent() else {
        return Verdict::Warn(format!("{} has no parent directory", dir.display()));
    };
    let (mods_real, default) = match (mods.canonicalize(), user_data.join("mods").canonicalize()) {
        (Ok(mods_real), Ok(default)) => (mods_real, default),
        (Err(err), _) | (_, Err(err)) => {
            return Verdict::Warn(format!(
                "mods directory {} is missing or unreadable ({err})",
                mods.display()
            ));
        }
    };
    // the user decided that a mod directory of their own turns removal off
    if mods_real != default {
        return Verdict::Keep;
    }
    if !is_plain_file(&mods_real.join(MOD_LIST)) {
        return Verdict::Warn(format!(
            "no {MOD_LIST} in {}, not a Factorio mods directory",
            mods.display()
        ));
    }
    let seen = dir.join(SEEN);
    match mod_present(&mods_real) {
        // create_new never follows a link and leaves an existing entry as it is
        Ok(true) => match OpenOptions::new().write(true).create_new(true).open(&seen) {
            Err(err) if err.kind() != io::ErrorKind::AlreadyExists => {
                Verdict::Warn(format!("cannot write {}: {err}", seen.display()))
            }
            _ => Verdict::Keep,
        },
        Ok(false) if is_plain_file(&seen) => Verdict::Remove(format!(
            "the ptt-radio mod is no longer in {}",
            mods.display()
        )),
        // never seen installed: not installed yet rather than removed
        Ok(false) => Verdict::Keep,
        Err(err) => Verdict::Warn(format!("cannot list {}: {err}", mods.display())),
    }
}

/// a plain file at `path` itself, not through a link
fn is_plain_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// `mods_dir` of a version 1 marker written by the installer, an absolute path
fn read_marker(path: &Path) -> Result<PathBuf, String> {
    let mut text = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MARKER_LIMIT + 1).read_to_end(&mut text))
        .map_err(|err| err.to_string())?;
    if text.len() as u64 > MARKER_LIMIT {
        return Err(format!("larger than {MARKER_LIMIT} bytes"));
    }
    let marker: serde_json::Value =
        serde_json::from_slice(&text).map_err(|err| format!("not JSON: {err}"))?;
    if marker["version"].as_u64() != Some(1) {
        return Err("not a version 1 marker".into());
    }
    if marker["installed_by"] != INSTALLED_BY {
        return Err(format!("not written by the {INSTALLED_BY}"));
    }
    let mods = marker["mods_dir"]
        .as_str()
        .map(PathBuf::from)
        .ok_or("no mods_dir")?;
    if !mods.is_absolute() {
        return Err(format!("mods_dir {} is not absolute", mods.display()));
    }
    Ok(mods)
}

/// a `ptt-radio_<ver>.zip` file or a `ptt-radio` / `ptt-radio_<ver>` directory. any
/// entry of such a name counts whatever its type (the mod can be a link), and so
/// does a name that is not unicode, since it could be one
fn mod_present(mods: &Path) -> io::Result<bool> {
    for entry in fs::read_dir(mods)? {
        let name = entry?.file_name();
        match name.to_str() {
            Some(name) if name == MOD || name.starts_with("ptt-radio_") => return Ok(true),
            Some(_) => {}
            None => return Ok(true),
        }
    }
    Ok(false)
}

/// deletes the running executable (scheduled on windows, outside `dir`), then `dir`
fn remove(dir: &Path, delete_exe: impl FnOnce(&Path) -> io::Result<()>) -> io::Result<()> {
    // the marker is checked again right before anything is deleted
    if !is_plain_file(&dir.join(MARKER)) {
        return Err(io::Error::other("install.json disappeared"));
    }
    delete_exe(dir)?;
    fs::remove_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "ptt-radio-companion";

    /// `<root>/user-data/{ptt-radio-companion/{exe, install.json}, mods/mod-list.json}`,
    /// with a sibling that must survive every removal
    struct Layout {
        root: PathBuf,
        dir: PathBuf,
        mods: PathBuf,
    }

    impl Layout {
        fn new(test: &str) -> Self {
            let root = env::temp_dir().join(format!(
                "ptt-radio-self-uninstall-{test}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            let user_data = root.join("user-data");
            let (dir, mods) = (user_data.join(INSTALL_DIR), user_data.join("mods"));
            fs::create_dir_all(&dir).unwrap();
            fs::create_dir_all(&mods).unwrap();
            fs::write(dir.join(EXE), b"binary").unwrap();
            fs::write(mods.join(MOD_LIST), br#"{"mods": []}"#).unwrap();
            fs::write(user_data.join("config.ini"), b"keep").unwrap();
            let layout = Layout { root, dir, mods };
            layout.marker(&layout.mods);
            layout
        }

        fn marker(&self, mods: &Path) {
            self.write_marker(&serde_json::json!({
                "version": 1,
                "mods_dir": mods,
                "installed_by": INSTALLED_BY,
            }));
        }

        fn write_marker(&self, value: &serde_json::Value) {
            fs::write(self.dir.join(MARKER), value.to_string()).unwrap();
        }

        fn verdict(&self) -> Verdict {
            decide(&self.dir.canonicalize().unwrap())
        }
    }

    impl Drop for Layout {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn warns(verdict: Verdict) -> bool {
        matches!(verdict, Verdict::Warn(_))
    }

    #[test]
    fn no_marker_is_a_manual_install_and_is_kept_silently() {
        let layout = Layout::new("no-marker");
        fs::remove_file(layout.dir.join(MARKER)).unwrap();
        assert_eq!(layout.verdict(), Verdict::Keep);
    }

    #[test]
    fn a_bad_marker_only_warns() {
        let layout = Layout::new("bad-marker");
        let mods = layout.mods.to_str().unwrap();
        for marker in [
            serde_json::json!({ "version": 2, "mods_dir": mods, "installed_by": INSTALLED_BY }),
            serde_json::json!({ "version": "1", "mods_dir": mods, "installed_by": INSTALLED_BY }),
            serde_json::json!({ "version": 1, "installed_by": INSTALLED_BY }),
            serde_json::json!({ "version": 1, "mods_dir": "mods", "installed_by": INSTALLED_BY }),
            serde_json::json!({ "version": 1, "mods_dir": mods }),
            serde_json::json!([1]),
        ] {
            layout.write_marker(&marker);
            assert!(warns(layout.verdict()), "{marker}");
        }
        fs::write(layout.dir.join(MARKER), b"{not json").unwrap();
        assert!(warns(layout.verdict()));
        fs::write(
            layout.dir.join(MARKER),
            vec![b' '; MARKER_LIMIT as usize + 1],
        )
        .unwrap();
        assert!(warns(layout.verdict()));
        fs::remove_file(layout.dir.join(MARKER)).unwrap();
        fs::create_dir(layout.dir.join(MARKER)).unwrap();
        assert!(warns(layout.verdict()));
    }

    #[test]
    fn a_missing_mods_dir_only_warns() {
        let layout = Layout::new("missing-mods");
        fs::remove_dir_all(&layout.mods).unwrap();
        assert!(warns(layout.verdict()));
    }

    #[test]
    fn a_mods_dir_without_mod_list_only_warns() {
        let layout = Layout::new("no-mod-list");
        fs::remove_file(layout.mods.join(MOD_LIST)).unwrap();
        assert!(warns(layout.verdict()));
        fs::create_dir(layout.mods.join(MOD_LIST)).unwrap();
        assert!(warns(layout.verdict()));
    }

    #[test]
    fn a_fresh_install_without_the_mod_is_kept_until_the_mod_was_seen() {
        let layout = Layout::new("fresh");
        assert_eq!(layout.verdict(), Verdict::Keep);
        assert!(layout.dir.join(EXE).is_file());
        assert!(!layout.dir.join(SEEN).exists());
    }

    #[test]
    fn the_mod_present_writes_mod_seen_once() {
        let layout = Layout::new("seen");
        fs::write(layout.mods.join("ptt-radio_0.4.0.zip"), b"zip").unwrap();
        assert_eq!(layout.verdict(), Verdict::Keep);
        let seen = layout.dir.join(SEEN);
        assert!(is_plain_file(&seen));
        assert_eq!(fs::read(&seen).unwrap(), b"");
        fs::write(&seen, b"kept").unwrap();
        assert_eq!(layout.verdict(), Verdict::Keep);
        assert_eq!(fs::read(&seen).unwrap(), b"kept");
    }

    #[test]
    fn mod_seen_that_is_not_a_plain_file_never_counts() {
        let layout = Layout::new("seen-not-file");
        let seen = layout.dir.join(SEEN);
        fs::create_dir(&seen).unwrap();
        assert_eq!(layout.verdict(), Verdict::Keep);
        fs::remove_dir(&seen).unwrap();
        #[cfg(unix)]
        {
            let target = layout.root.join("seen-target");
            std::os::unix::fs::symlink(&target, &seen).unwrap();
            fs::create_dir(layout.mods.join("ptt-radio")).unwrap();
            assert_eq!(layout.verdict(), Verdict::Keep);
            assert!(!target.exists(), "the link was followed");
            fs::remove_dir(layout.mods.join("ptt-radio")).unwrap();
            fs::write(&target, b"").unwrap();
            assert_eq!(layout.verdict(), Verdict::Keep);
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_install_dir_with_the_mod_present_only_warns() {
        use std::os::unix::fs::PermissionsExt;
        let layout = Layout::new("seen-unwritable");
        fs::write(layout.mods.join("ptt-radio_0.4.0.zip"), b"zip").unwrap();
        fs::set_permissions(&layout.dir, fs::Permissions::from_mode(0o555)).unwrap();
        let verdict = layout.verdict();
        let written = layout.dir.join(SEEN).exists();
        fs::set_permissions(&layout.dir, fs::Permissions::from_mode(0o755)).unwrap();
        // root ignores the mode and writes the file anyway
        assert!(warns(verdict) || written, "unwritable dir");
        assert!(layout.dir.join(EXE).is_file());
    }

    #[test]
    fn the_mod_as_a_zip_or_a_directory_keeps_the_install() {
        let layout = Layout::new("present");
        let zip = layout.mods.join("ptt-radio_0.4.0.zip");
        fs::write(&zip, b"zip").unwrap();
        assert_eq!(layout.verdict(), Verdict::Keep);
        fs::remove_file(&zip).unwrap();
        for dir in ["ptt-radio", "ptt-radio_0.4.0"] {
            fs::create_dir(layout.mods.join(dir)).unwrap();
            assert_eq!(layout.verdict(), Verdict::Keep, "{dir}");
            fs::remove_dir(layout.mods.join(dir)).unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&layout.root, layout.mods.join("ptt-radio")).unwrap();
            assert_eq!(layout.verdict(), Verdict::Keep);
        }
    }

    #[test]
    fn a_mods_dir_of_the_users_own_turns_removal_off() {
        let layout = Layout::new("own-mods");
        let own = layout.root.join("elsewhere");
        fs::create_dir(&own).unwrap();
        fs::write(own.join(MOD_LIST), b"{}").unwrap();
        layout.marker(&own);
        assert_eq!(layout.verdict(), Verdict::Keep);
    }

    #[test]
    fn an_install_dir_of_another_name_is_never_touched() {
        let layout = Layout::new("renamed");
        let renamed = layout.root.join("user-data").join("companion");
        fs::rename(&layout.dir, &renamed).unwrap();
        assert_eq!(decide(&renamed.canonicalize().unwrap()), Verdict::Keep);
    }

    #[test]
    fn the_mod_absent_after_it_was_seen_removes_only_the_install_dir() {
        let layout = Layout::new("absent");
        fs::write(layout.mods.join("other-mod_1.0.0.zip"), b"zip").unwrap();
        let zip = layout.mods.join("ptt-radio_0.4.0.zip");
        fs::write(&zip, b"zip").unwrap();
        assert_eq!(layout.verdict(), Verdict::Keep);
        fs::remove_file(&zip).unwrap();
        let dir = layout.dir.canonicalize().unwrap();
        let Verdict::Remove(reason) = decide(&dir) else {
            panic!("the mod is absent");
        };
        assert!(reason.contains("no longer in"));
        let mut deleted = None;
        remove(&dir, |dir| {
            deleted = Some(dir.to_path_buf());
            fs::remove_file(dir.join(EXE))
        })
        .unwrap();
        assert_eq!(deleted.as_deref(), Some(dir.as_path()));
        assert!(!layout.dir.exists());
        let user_data = layout.root.join("user-data");
        assert_eq!(fs::read(user_data.join("config.ini")).unwrap(), b"keep");
        assert!(layout.mods.join(MOD_LIST).is_file());
        assert!(layout.mods.join("other-mod_1.0.0.zip").is_file());
    }

    #[test]
    fn removal_refuses_when_the_marker_is_gone_and_stops_at_a_failed_delete() {
        let layout = Layout::new("remove-guards");
        let dir = layout.dir.canonicalize().unwrap();
        fs::remove_file(dir.join(MARKER)).unwrap();
        assert!(remove(&dir, |_| unreachable!()).is_err());
        assert!(dir.join(EXE).is_file());
        layout.marker(&layout.mods);
        assert!(remove(&dir, |_| Err(io::Error::other("denied"))).is_err());
        assert!(dir.join(EXE).is_file() && dir.join(MARKER).is_file());
    }
}
