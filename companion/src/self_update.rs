//! self-update from the latest GitHub release, run once at startup before any
//! socket, audio device or hotkey exists.
//!
//! flow: GET releases/latest, update only to a strictly newer stable version,
//! download the platform archive and SHA256SUMS of that release, verify the
//! archive, take only the companion executable out of it, swap it in with
//! self-replace and re-launch with the same arguments. every outcome is one
//! `self-update: ...` line on stderr and every failure keeps the current binary.
//!
//! active only in binaries compiled with PTT_RADIO_RELEASE_BUILD=1 (release.yml),
//! so a local cargo build never replaces itself. opt-out: --no-self-update or
//! PTT_RADIO_NO_SELF_UPDATE=1. the re-launched process carries
//! PTT_RADIO_UPDATED=<version>, which skips the check and so prevents a loop.
//!
//! PTT_RADIO_UPDATE_BASE_URL is a compile-time knob for the local fake-release
//! proof: it replaces https://api.github.com, and only a binary compiled with it
//! accepts plain http. a shipped binary has no runtime override of any kind.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Read};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use semver::Version;
use sha2::{Digest, Sha256};

const API_BASE: &str = match option_env!("PTT_RADIO_UPDATE_BASE_URL") {
    Some(base) => base,
    None => "https://api.github.com",
};
const LATEST_PATH: &str = "/repos/vshuraeff/ptt-radio/releases/latest";
const UPDATED_ENV: &str = "PTT_RADIO_UPDATED";
const OPT_OUT_ENV: &str = "PTT_RADIO_NO_SELF_UPDATE";
const SUMS_ASSET: &str = "SHA256SUMS";
/// resolve, connect and every read; also the whole budget of the small requests
const STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// whole budget of the archive download
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const META_LIMIT: u64 = 1 << 20;
/// cap of the archive and of the executable unpacked from it
const BINARY_LIMIT: u64 = 64 << 20;
/// cap of everything a tar.gz unpacks to, skipped entries included: the
/// executable plus room for LICENSE, README and the headers
const TAR_LIMIT: u64 = BINARY_LIMIT + META_LIMIT;

/// the startup check. returns to let this binary continue; after a successful
/// update it re-launches the new binary instead and does not return
pub fn at_startup(opt_out: bool) {
    let (Ok(line) | Err(line)) = check(opt_out);
    eprintln!("self-update: {line}");
}

fn check(opt_out: bool) -> Result<String, String> {
    if let Some(version) = env::var_os(UPDATED_ENV) {
        return Ok(format!("updated to {}", version.to_string_lossy()));
    }
    if option_env!("PTT_RADIO_RELEASE_BUILD") != Some("1") {
        return Ok("disabled (not a release build)".into());
    }
    if opt_out || env::var_os(OPT_OUT_ENV).is_some_and(|v| !v.is_empty() && v != "0") {
        return Ok("disabled (opted out)".into());
    }
    let current = Version::parse(env!("CARGO_PKG_VERSION")).map_err(|err| err.to_string())?;
    let agent = agent();
    let release = fetch(
        &agent,
        &format!("{API_BASE}{LATEST_PATH}"),
        "application/vnd.github+json",
        META_LIMIT,
        STEP_TIMEOUT,
    )?;
    let release: serde_json::Value =
        serde_json::from_slice(&release).map_err(|err| format!("release JSON: {err}"))?;
    let latest = release_version(&release)?;
    if !is_update(&latest, &current) {
        return Ok(format!("up to date ({current}, latest release {latest})"));
    }
    let (os, arch) = (env::consts::OS, env::consts::ARCH);
    let (asset, entry) = asset_for(&latest, os, arch)
        .ok_or_else(|| format!("no {latest} release asset for {os}-{arch}"))?;
    let sums = fetch(
        &agent,
        asset_url(&release, SUMS_ASSET)?,
        "application/octet-stream",
        META_LIMIT,
        STEP_TIMEOUT,
    )?;
    let archive = fetch(
        &agent,
        asset_url(&release, &asset)?,
        "application/octet-stream",
        BINARY_LIMIT,
        DOWNLOAD_TIMEOUT,
    )?;
    verify(&sums, &asset, &archive)?;
    let binary = extract(&asset, entry, &archive)?;
    // taken before the swap: on linux it reads "<path> (deleted)" afterwards
    let exe = env::current_exe().map_err(|err| format!("current executable: {err}"))?;
    install(&exe, &binary)?;
    Err(relaunch(&exe, &latest))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(option_env!("PTT_RADIO_UPDATE_BASE_URL").is_none())
        .user_agent(concat!("ptt-radio-companion/", env!("CARGO_PKG_VERSION")))
        .timeout_resolve(Some(STEP_TIMEOUT))
        .timeout_connect(Some(STEP_TIMEOUT))
        .timeout_per_read(Some(STEP_TIMEOUT))
        .max_redirects(5)
        .build()
        .into()
}

/// GET with redirects (assets live on a githubusercontent.com host), the body capped at `limit`
fn fetch(
    agent: &ureq::Agent,
    url: &str,
    accept: &str,
    limit: u64,
    total: Duration,
) -> Result<Vec<u8>, String> {
    let fail = |err: ureq::Error| format!("GET {url}: {err}");
    agent
        .get(url)
        .header("Accept", accept)
        .config()
        .timeout_global(Some(total))
        .build()
        .call()
        .map_err(fail)?
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(fail)
}

/// `tag_name` of the release as `v<semver>`
fn release_version(release: &serde_json::Value) -> Result<Version, String> {
    let tag = release["tag_name"]
        .as_str()
        .ok_or("release JSON has no tag_name")?;
    tag.strip_prefix('v')
        .and_then(|version| Version::parse(version).ok())
        .ok_or_else(|| format!("release tag {tag:?} is not v<semver>"))
}

/// strictly newer and stable: never a downgrade, never the same version, never a pre-release
fn is_update(latest: &Version, current: &Version) -> bool {
    latest.pre.is_empty() && latest > current
}

/// the release asset of a target and the executable inside it (release archive names)
fn asset_for(version: &Version, os: &str, arch: &str) -> Option<(String, &'static str)> {
    let (platform, kind, entry) = match (os, arch) {
        ("linux", "x86_64") => ("linux-x86_64", "tar.gz", "ptt-radio-companion"),
        ("windows", "x86_64") => ("windows-x86_64", "zip", "ptt-radio-companion.exe"),
        ("macos", "x86_64" | "aarch64") => ("macos-universal", "zip", "ptt-radio-companion"),
        _ => return None,
    };
    Some((
        format!("ptt-radio-companion-{version}-{platform}.{kind}"),
        entry,
    ))
}

fn asset_url<'a>(release: &'a serde_json::Value, name: &str) -> Result<&'a str, String> {
    release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|asset| asset["name"] == name)
        .and_then(|asset| asset["browser_download_url"].as_str())
        .ok_or_else(|| format!("the release has no asset {name}"))
}

fn verify(sums: &[u8], asset: &str, archive: &[u8]) -> Result<(), String> {
    let sums = std::str::from_utf8(sums).map_err(|_| format!("{SUMS_ASSET} is not text"))?;
    if Sha256::digest(archive).as_slice() != expected_digest(sums, asset)? {
        return Err(format!("checksum mismatch for {asset}, update refused"));
    }
    Ok(())
}

/// the one sha256sum line of `asset`: `<hex>  <name>`, or `<hex> *<name>` in binary mode
fn expected_digest(sums: &str, asset: &str) -> Result<[u8; 32], String> {
    let mut found = None;
    for line in sums.lines() {
        let Some((hex, name)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let name = name.trim_start();
        if name.strip_prefix('*').unwrap_or(name) != asset {
            continue;
        }
        let digest = decode_digest(hex)
            .ok_or_else(|| format!("{SUMS_ASSET}: malformed line for {asset}"))?;
        if found.replace(digest).is_some() {
            return Err(format!("{SUMS_ASSET} lists {asset} twice"));
        }
    }
    found.ok_or_else(|| format!("{SUMS_ASSET} has no line for {asset}, update refused"))
}

fn decode_digest(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let nibble = |c: u8| char::from(c).to_digit(16);
    let mut digest = [0; 32];
    for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
        *byte = (nibble(pair[0])? << 4 | nibble(pair[1])?) as u8;
    }
    Some(digest)
}

/// the executable, and nothing else, out of the release archive; it is read into
/// memory, so no entry path ever reaches the file system
fn extract(asset: &str, entry: &str, archive: &[u8]) -> Result<Vec<u8>, String> {
    let found = if asset.ends_with(".tar.gz") {
        from_tar_gz(archive, entry)
    } else {
        from_zip(archive, entry)
    };
    found.map_err(|err| format!("{asset}: {err}"))
}

fn from_tar_gz(archive: &[u8], entry: &str) -> Result<Vec<u8>, String> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive).take(TAR_LIMIT));
    let found = find_in_tar(&mut tar, entry);
    // checked whatever the search returned: a stream cut at the cap ends an entry
    // early without an error, or makes tar fail with a less telling one
    if tar.into_inner().limit() == 0 {
        return Err(format!("unpacks to more than {TAR_LIMIT} bytes"));
    }
    found
}

fn find_in_tar(tar: &mut tar::Archive<impl Read>, entry: &str) -> Result<Vec<u8>, String> {
    let mut found = None;
    for file in tar.entries().map_err(|err| err.to_string())? {
        let file = file.map_err(|err| err.to_string())?;
        let path = file.path().map_err(|err| err.to_string())?.into_owned();
        if !file.header().entry_type().is_file() || !is_entry(&path, entry) {
            continue;
        }
        if found.replace(read_capped(file)?).is_some() {
            return Err(format!("{entry} appears twice"));
        }
    }
    found.ok_or_else(|| format!("no {entry} inside"))
}

fn from_zip(archive: &[u8], entry: &str) -> Result<Vec<u8>, String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(archive)).map_err(|err| err.to_string())?;
    let mut found = None;
    for index in 0..zip.len() {
        let file = zip.by_index(index).map_err(|err| err.to_string())?;
        // enclosed_name is None for an absolute or escaping path
        if !file.is_file()
            || !file
                .enclosed_name()
                .is_some_and(|path| is_entry(&path, entry))
        {
            continue;
        }
        if found.replace(read_capped(file)?).is_some() {
            return Err(format!("{entry} appears twice"));
        }
    }
    found.ok_or_else(|| format!("no {entry} inside"))
}

/// the executable at the top or under a folder; the exact name also leaves out
/// the `._<name>` AppleDouble entry that ditto adds to the macOS zip
fn is_entry(path: &Path, entry: &str) -> bool {
    path.file_name().is_some_and(|name| name == entry)
}

fn read_capped(reader: impl Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(BINARY_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| err.to_string())?;
    if bytes.len() as u64 > BINARY_LIMIT {
        return Err(format!("executable larger than {BINARY_LIMIT} bytes"));
    }
    Ok(bytes)
}

/// writes the new binary to a temp file beside the executable, then swaps it in
fn install(exe: &Path, binary: &[u8]) -> Result<(), String> {
    let fail = |err: io::Error| format!("replace {}: {err}", exe.display());
    let exe = exe.canonicalize().map_err(fail)?;
    let dir = exe
        .parent()
        .ok_or_else(|| fail(io::Error::other("executable has no parent directory")))?;
    let pid = std::process::id();
    let temp = dir.join(format!(".ptt-radio-companion.update-{pid}"));
    let previous = dir.join(format!(".ptt-radio-companion.previous-{pid}"));
    write_executable(&temp, binary).map_err(fail)?;
    let result = replace_keeping(&exe, &previous, || self_replace::self_replace(&temp));
    // best effort: self_replace has copied it, or the update has already failed
    let _ = fs::remove_file(&temp);
    result
}

/// runs `replace` with a recovery copy of `exe` at `previous`. on windows
/// self-replace first moves the running executable away and does not roll back
/// when a later step fails, so a launch path left empty is filled from the copy
fn replace_keeping(
    exe: &Path,
    previous: &Path,
    replace: impl FnOnce() -> io::Result<()>,
) -> Result<(), String> {
    if let Err(err) = File::open(exe).and_then(|current| write_executable(previous, current)) {
        return Err(format!("recovery copy of {}: {err}", exe.display()));
    }
    let result = match replace() {
        Ok(()) => Ok(()),
        // an unknown state counts as present: never move the copy over a file
        Err(err) if exe.try_exists().unwrap_or(true) => {
            Err(format!("replace {}: {err}", exe.display()))
        }
        // the copy goes back, or stays where it is for a manual restore
        Err(err) => {
            return Err(match fs::rename(previous, exe) {
                Ok(()) => format!("replace failed ({err}), previous version restored"),
                Err(restore) => format!(
                    "replace failed ({err}), restore failed: {restore}; the previous version is {}",
                    previous.display()
                ),
            });
        }
    };
    // best effort: a plain copy, not the running image
    let _ = fs::remove_file(previous);
    result
}

/// a new executable file (never through an existing one or a link), flushed to disk
fn write_executable(path: &Path, mut content: impl Read) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o755);
    let mut file = options.open(path)?;
    let result = io::copy(&mut content, &mut file).and_then(|_| file.sync_all());
    drop(file);
    if result.is_err() {
        // best effort: only remove the partial file this call successfully created
        let _ = fs::remove_file(path);
    }
    result
}

/// the new binary with the same arguments: exec on unix; on windows a spawned
/// process that takes over the console while this one exits. returns only on failure
fn relaunch(exe: &Path, version: &Version) -> String {
    let mut args = env::args_os();
    let arg0 = args.next();
    let mut command = Command::new(exe);
    command.args(args).env(UPDATED_ENV, version.to_string());
    #[cfg(unix)]
    let err = {
        use std::os::unix::process::CommandExt;
        if let Some(arg0) = arg0 {
            command.arg0(arg0);
        }
        command.exec()
    };
    #[cfg(not(unix))]
    let err = {
        let _ = arg0;
        match command.spawn() {
            Ok(_) => std::process::exit(0),
            Err(err) => err,
        }
    };
    format!("updated to {version}, but the re-launch failed: {err}")
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use super::*;

    const NAME: &str = "ptt-radio-companion";

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn updates_only_to_a_strictly_newer_stable_version() {
        assert!(is_update(&v("0.5.0"), &v("0.4.0")));
        assert!(is_update(&v("0.4.1"), &v("0.4.0")));
        assert!(is_update(&v("0.10.0"), &v("0.9.0")));
        assert!(is_update(&v("1.0.0"), &v("0.99.99")));
        assert!(!is_update(&v("0.4.0"), &v("0.4.0")));
        assert!(!is_update(&v("0.3.9"), &v("0.4.0")));
        assert!(!is_update(&v("0.5.0-rc.1"), &v("0.4.0")));
    }

    #[test]
    fn reads_the_version_from_a_v_tag_only() {
        let tag = |tag: &str| release_version(&serde_json::json!({ "tag_name": tag }));
        assert_eq!(tag("v0.5.0").unwrap(), v("0.5.0"));
        assert!(tag("0.5.0").is_err());
        assert!(tag("v0.5").is_err());
        assert!(tag("vnext").is_err());
        assert!(release_version(&serde_json::json!({ "message": "Not Found" })).is_err());
    }

    #[test]
    fn picks_release_asset_per_target() {
        let version = v("0.4.0");
        let asset = |os, arch| asset_for(&version, os, arch);
        assert_eq!(
            asset("linux", "x86_64"),
            Some(("ptt-radio-companion-0.4.0-linux-x86_64.tar.gz".into(), NAME))
        );
        assert_eq!(
            asset("windows", "x86_64"),
            Some((
                "ptt-radio-companion-0.4.0-windows-x86_64.zip".into(),
                "ptt-radio-companion.exe"
            ))
        );
        for arch in ["x86_64", "aarch64"] {
            assert_eq!(
                asset("macos", arch),
                Some(("ptt-radio-companion-0.4.0-macos-universal.zip".into(), NAME))
            );
        }
        assert_eq!(asset("linux", "aarch64"), None);
        assert_eq!(asset("windows", "aarch64"), None);
        assert_eq!(asset("freebsd", "x86_64"), None);
    }

    #[test]
    fn finds_the_asset_url_by_exact_name() {
        let release = serde_json::json!({ "assets": [
            { "name": "SHA256SUMS.sig", "browser_download_url": "https://example.invalid/sig" },
            { "name": "SHA256SUMS", "browser_download_url": "https://example.invalid/sums" },
        ]});
        assert_eq!(
            asset_url(&release, "SHA256SUMS").unwrap(),
            "https://example.invalid/sums"
        );
        assert!(asset_url(&release, "missing.zip").is_err());
        assert!(asset_url(&serde_json::json!({}), "SHA256SUMS").is_err());
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn parses_sha256sum_lines() {
        let digest = Sha256::digest(b"archive");
        let line = hex(&digest);
        let text = format!(
            "{}  other.zip\n{line}  a.zip\r\n{} *b.zip\n",
            "0".repeat(64),
            line.to_uppercase()
        );
        assert_eq!(expected_digest(&text, "a.zip").unwrap(), digest.as_slice());
        assert_eq!(expected_digest(&text, "b.zip").unwrap(), digest.as_slice());
        assert!(expected_digest(&text, "c.zip").is_err());
        assert!(expected_digest(&format!("{line}  a.zip\n{line}  a.zip\n"), "a.zip").is_err());
        assert!(expected_digest(&format!("{}  a.zip", &line[1..]), "a.zip").is_err());
        assert!(expected_digest(&format!("{}g  a.zip", &line[1..]), "a.zip").is_err());
        assert!(expected_digest(&format!("+{}  a.zip", &line[1..]), "a.zip").is_err());
    }

    #[test]
    fn refuses_a_mismatch_or_a_missing_line() {
        let sums = format!("{}  a.zip\n", hex(&Sha256::digest(b"archive")));
        assert!(verify(sums.as_bytes(), "a.zip", b"archive").is_ok());
        assert!(
            verify(sums.as_bytes(), "a.zip", b"tampered")
                .unwrap_err()
                .contains("checksum mismatch")
        );
        assert!(verify(sums.as_bytes(), "b.zip", b"archive").is_err());
        assert!(verify(b"\xff\xfe", "a.zip", b"archive").is_err());
    }

    fn tar_gz(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        for (path, kind, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            if kind.is_symlink() {
                header.set_link_name("elsewhere").unwrap();
            }
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn extracts_only_the_executable_from_a_tar_gz() {
        let file = tar::EntryType::Regular;
        let flat = tar_gz(&[
            ("LICENSE", file, b"gpl"),
            (NAME, file, b"new"),
            ("README.md", file, b"readme"),
        ]);
        assert_eq!(extract("x.tar.gz", NAME, &flat).unwrap(), b"new");
        let nested = tar_gz(&[
            ("ptt-radio-companion/", tar::EntryType::Directory, b""),
            ("ptt-radio-companion/ptt-radio-companion", file, b"nested"),
        ]);
        assert_eq!(extract("x.tar.gz", NAME, &nested).unwrap(), b"nested");
        let link = tar_gz(&[(NAME, tar::EntryType::Symlink, b"")]);
        assert!(
            extract("x.tar.gz", NAME, &link)
                .unwrap_err()
                .contains("no ptt-radio-companion")
        );
        let twice = tar_gz(&[(NAME, file, b"a"), ("dir/ptt-radio-companion", file, b"b")]);
        assert!(
            extract("x.tar.gz", NAME, &twice)
                .unwrap_err()
                .contains("twice")
        );
        assert!(extract("x.tar.gz", NAME, &tar_gz(&[("LICENSE", file, b"gpl")])).is_err());
        assert!(extract("x.tar.gz", NAME, b"not gzip").is_err());
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(bytes).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn caps_the_whole_tar_stream() {
        // a small download whose skipped entry unpacks past the cap, the executable after it
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(TAR_LIMIT);
        builder
            .append_data(&mut header, "padding", io::repeat(0).take(TAR_LIMIT))
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(2);
        builder.append_data(&mut header, NAME, &b"ok"[..]).unwrap();
        let archive = gzip(&builder.into_inner().unwrap());
        assert!(archive.len() < (1 << 20));
        assert!(
            extract("x.tar.gz", NAME, &archive)
                .unwrap_err()
                .contains("unpacks to more than")
        );
    }

    #[test]
    fn refuses_a_tar_cut_inside_the_executable() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(4096);
        builder
            .append_data(&mut header, NAME, &[7; 4096][..])
            .unwrap();
        let tar = builder.into_inner().unwrap();
        // the header block, then half of the data: a clean end of the gzip stream
        assert!(extract("x.tar.gz", NAME, &gzip(&tar[..512 + 2048])).is_err());
    }

    fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (path, data) in entries {
            if let Some(dir) = path.strip_suffix('/') {
                writer.add_directory(dir, options).unwrap();
            } else {
                writer.start_file(*path, options).unwrap();
                writer.write_all(data).unwrap();
            }
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn extracts_only_the_executable_from_a_zip() {
        let exe = "ptt-radio-companion.exe";
        let flat = zip(&[("LICENSE", b"gpl"), (exe, b"new"), ("README.md", b"readme")]);
        assert_eq!(extract("x.zip", exe, &flat).unwrap(), b"new");
        // the layout `ditto -c -k` writes: the AppleDouble entry beside the binary
        let ditto = zip(&[("._ptt-radio-companion", b"rsrc"), (NAME, b"mac")]);
        assert_eq!(extract("x.zip", NAME, &ditto).unwrap(), b"mac");
        let nested = zip(&[
            ("ptt-radio-companion/", b""),
            ("ptt-radio-companion/ptt-radio-companion", b"nested"),
        ]);
        assert_eq!(extract("x.zip", NAME, &nested).unwrap(), b"nested");
        let escaping = zip(&[("../ptt-radio-companion", b"evil")]);
        assert!(extract("x.zip", NAME, &escaping).is_err());
        let twice = zip(&[
            ("a/ptt-radio-companion", b"a"),
            ("b/ptt-radio-companion", b"b"),
        ]);
        assert!(
            extract("x.zip", NAME, &twice)
                .unwrap_err()
                .contains("twice")
        );
        assert!(extract("x.zip", exe, &zip(&[(NAME, b"unix")])).is_err());
        assert!(extract("x.zip", NAME, b"not a zip").is_err());
    }

    #[test]
    fn caps_the_unpacked_size() {
        assert_eq!(read_capped(&b"small"[..]).unwrap(), b"small");
        // an endless entry stops one byte past the cap
        assert!(
            read_capped(io::repeat(0))
                .unwrap_err()
                .contains("larger than")
        );
    }

    /// an empty directory of its own per test, since the tests run in parallel
    fn scratch(test: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "ptt-radio-self-update-{test}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn restores_the_launch_path_when_the_replace_fails_after_moving_it() {
        let dir = scratch("restore");
        let (exe, previous) = (dir.join(NAME), dir.join("previous"));
        fs::write(&exe, b"old binary").unwrap();
        // what self-replace does on windows: move the running executable away, then fail
        let err = replace_keeping(&exe, &previous, || {
            fs::rename(&exe, dir.join("relocated"))?;
            Err(io::Error::other("disk full"))
        })
        .unwrap_err();
        assert!(err.contains("disk full") && err.contains("previous version restored"));
        assert_eq!(fs::read(&exe).unwrap(), b"old binary");
        assert!(!previous.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_the_launch_path_on_an_early_failure_and_drops_the_copy_on_success() {
        let dir = scratch("keep");
        let (exe, previous) = (dir.join(NAME), dir.join("previous"));
        fs::write(&exe, b"old binary").unwrap();
        let err = replace_keeping(&exe, &previous, || Err(io::Error::other("denied"))).unwrap_err();
        assert!(err.contains("denied") && !err.contains("restored"));
        assert_eq!(fs::read(&exe).unwrap(), b"old binary");
        assert!(!previous.exists());
        replace_keeping(&exe, &previous, || fs::write(&exe, b"new binary")).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new binary");
        assert!(!previous.exists());
        // a stale file at the copy's name is never written through
        fs::write(&previous, b"stale").unwrap();
        let err = replace_keeping(&exe, &previous, || unreachable!()).unwrap_err();
        assert!(err.contains("recovery copy"));
        assert_eq!(fs::read(&exe).unwrap(), b"new binary");
        assert_eq!(fs::read(&previous).unwrap(), b"stale");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_a_fresh_executable_temp_file() {
        let dir = scratch("write");
        let path = dir.join("binary");
        write_executable(&path, &b"new"[..]).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // the owner bit only: the umask may clear the others
            assert_ne!(fs::metadata(&path).unwrap().permissions().mode() & 0o100, 0);
        }
        // create_new: never writes through an existing file or link
        assert!(write_executable(&path, &b"again"[..]).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"new");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn removes_only_its_own_partial_executable_on_read_failure() {
        struct FailedRead;
        impl Read for FailedRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("read failed"))
            }
        }
        let dir = scratch("partial");
        let path = dir.join("binary");
        let content = Cursor::new(b"partial").chain(FailedRead);
        assert!(write_executable(&path, content).is_err());
        assert!(!path.exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
