// "Install WhatsApp": one click instead of GitHub, a zip, Explorer and a terminal. Downloads GOWA's
// own latest build for this PC from its GitHub releases, checks its SHA-256 against the checksums
// GOWA publishes (no checksum, nothing installed), and puts the binary in Coucou's GOWA folder
// (%LOCALAPPDATA%\Coucou\GOWA, next to the WhatsApp session it keeps). The only network call is to
// GitHub, once, after the click; no message ever goes there. Port of WhatsAppGOWAInstaller.swift.
//
// The download uses reqwest with the system's certificate store as well as the built-in roots, so
// a PC whose antivirus inspects TLS with its own certificate (it adds it to the Windows store)
// still reaches GitHub.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{redirect, Url};
use ring::digest;
use serde::Serialize;

use super::model::hex;
use super::release::{self, Target};
use crate::i18n::t;
use crate::net;

/// Where the installed binary is called.
#[cfg(windows)]
pub const BINARY_NAME: &str = "gowa.exe";
#[cfg(not(windows))]
pub const BINARY_NAME: &str = "gowa";

/// The folder GOWA lives in: its binary and its WhatsApp session (its own database, not Coucou's).
pub fn dir() -> PathBuf {
    crate::settings::local_dir().join("GOWA")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "step", rename_all = "camelCase")]
pub enum Step {
    Idle,
    Checking,
    Downloading { megabytes: u64 },
    Verifying,
    Installing,
    Done { version: String },
    Failed { message: String },
}

impl Step {
    pub fn busy(&self) -> bool {
        matches!(self, Step::Checking | Step::Downloading { .. } | Step::Verifying | Step::Installing)
    }
}

/// Where the release comes from, and which hosts the download may follow redirects to.
#[derive(Clone)]
pub struct Source {
    pub latest_url: String,
    pub allow: fn(&Url) -> bool,
}

fn github_only(url: &Url) -> bool {
    url.scheme() == "https" && release::allowed_host(url.host_str())
}

impl Source {
    pub fn github() -> Source {
        Source { latest_url: release::LATEST_URL.into(), allow: github_only }
    }
}

const MAX_ZIP: u64 = 150 * 1024 * 1024;
const MAX_BINARY: u64 = 250 * 1024 * 1024;

fn client(allow: fn(&Url) -> bool) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("Coucou/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        // GitHub redirects its assets to its own storage, and nothing else is followed.
        .redirect(redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 || !allow(attempt.url()) {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|e| e.to_string())
}

fn download_failed() -> String {
    t("The download didn't go through. Check your connection and try again.")
}

/// A small answer (the release's description, the checksums) read whole, capped.
async fn fetch(http: &reqwest::Client, url: &str, accept: &str, limit: usize) -> Result<Vec<u8>, String> {
    let response = http.get(url).header("Accept", accept).send().await.map_err(|_| download_failed())?;
    if !response.status().is_success() {
        return Err(download_failed());
    }
    net::read_capped(response, limit).await.map_err(|_| download_failed())
}

/// Installs the latest GOWA into `dir` and returns its version. `progress` is told each step;
/// `before_replace` runs once the new binary is unpacked and verified, right before it takes the
/// old one's place: Windows won't replace a running program, so the caller stops GOWA there.
pub async fn install(
    source: &Source,
    dir: &Path,
    target: Target,
    progress: &(dyn Fn(Step) + Send + Sync),
    before_replace: &(dyn Fn() + Send + Sync),
) -> Result<String, String> {
    // 1. Which build: GOWA's latest release, the zip for this PC.
    progress(Step::Checking);
    let first = Url::parse(&source.latest_url).map_err(|_| download_failed())?;
    if !(source.allow)(&first) {
        return Err(download_failed());
    }
    let http = client(source.allow)?;
    let meta = fetch(&http, &source.latest_url, "application/vnd.github+json", 2 * 1024 * 1024).await?;
    let no_build = || t("No GOWA build for this computer was found on GitHub. Try again later, or set your own gateway under Advanced.");
    let release = release::parse(&meta).ok_or_else(no_build)?;
    let (zip, sums) = release::pick(&release.assets, target).ok_or_else(no_build)?;
    let checksums = match sums {
        Some(asset) => fetch(&http, &asset.url, "text/plain", 64 * 1024).await.ok().map(|b| String::from_utf8_lossy(&b).into_owned()),
        None => None,
    };
    let expected = release::expected_sha256(zip, checksums.as_deref())
        .ok_or_else(|| t("GOWA's release has no checksum to check the download against: nothing was installed."))?;

    // 2. Download, hashing as it comes.
    progress(Step::Downloading { megabytes: (zip.size / 1_000_000).max(1) });
    std::fs::create_dir_all(dir).map_err(|_| download_failed())?;
    let part = dir.join("gowa-download.part");
    let digest_hex = match download(&http, &zip.url, &part).await {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
    };

    // 3. Verify: exactly the file GOWA published.
    progress(Step::Verifying);
    if digest_hex != expected {
        let _ = std::fs::remove_file(&part);
        return Err(t("The download doesn't match the file GOWA published: it was deleted, nothing was installed."));
    }

    // 4. Unpack next to the old binary, then swap them.
    progress(Step::Installing);
    let fresh = dir.join(format!("{BINARY_NAME}.new"));
    let wanted = target.binary_name();
    let (part_for_task, fresh_for_task) = (part.clone(), fresh.clone());
    let unpacked = tokio::task::spawn_blocking(move || extract_binary(&part_for_task, &wanted, &fresh_for_task)).await;
    let _ = std::fs::remove_file(&part);
    match unpacked {
        Ok(Ok(())) => {}
        Ok(Err(Unpack::Missing)) => {
            let _ = std::fs::remove_file(&fresh);
            return Err(no_build());
        }
        _ => {
            let _ = std::fs::remove_file(&fresh);
            return Err(t("The download couldn't be unzipped. Try again."));
        }
    }
    before_replace();
    let place = dir.join(BINARY_NAME);
    if std::fs::rename(&fresh, &place).is_err() {
        // A running program can't be replaced; a moment later it can.
        std::thread::sleep(Duration::from_millis(500));
        if let Err(error) = std::fs::rename(&fresh, &place) {
            let _ = std::fs::remove_file(&fresh);
            crate::log::line(format!("WhatsApp: GOWA could not be put in place: {error}"));
            return Err(t("The download couldn't be unzipped. Try again."));
        }
    }
    Ok(release.version)
}

/// Streams `url` into `to`; returns the SHA-256 (lowercase hex) of what was written.
async fn download(http: &reqwest::Client, url: &str, to: &Path) -> Result<String, String> {
    let mut response = http.get(url).send().await.map_err(|_| download_failed())?;
    if !response.status().is_success() {
        return Err(download_failed());
    }
    if response.content_length().is_some_and(|n| n > MAX_ZIP) {
        return Err(download_failed());
    }
    let mut file = std::fs::File::create(to).map_err(|_| download_failed())?;
    let mut context = digest::Context::new(&digest::SHA256);
    let mut total: u64 = 0;
    while let Some(chunk) = response.chunk().await.map_err(|_| download_failed())? {
        total += chunk.len() as u64;
        if total > MAX_ZIP {
            return Err(download_failed());
        }
        context.update(&chunk);
        file.write_all(&chunk).map_err(|_| download_failed())?;
    }
    file.flush().map_err(|_| download_failed())?;
    Ok(hex(context.finish().as_ref()))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Unpack {
    /// Not a readable zip, or a write failed.
    Broken,
    /// A zip without the binary.
    Missing,
}

/// Copies the entry named `binary_name` (anywhere in the zip) to `to`. Nothing else is written,
/// whatever the zip's paths say.
pub fn extract_binary(zip_path: &Path, binary_name: &str, to: &Path) -> Result<(), Unpack> {
    let file = std::fs::File::open(zip_path).map_err(|_| Unpack::Broken)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|_| Unpack::Broken)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|_| Unpack::Broken)?;
        let name = entry.name().rsplit(['/', '\\']).next().unwrap_or("").to_string();
        if entry.is_dir() || !name.eq_ignore_ascii_case(binary_name) {
            continue;
        }
        if entry.size() > MAX_BINARY {
            return Err(Unpack::Broken);
        }
        let mut out = std::fs::File::create(to).map_err(|_| Unpack::Broken)?;
        let copied = std::io::copy(&mut (&mut entry).take(MAX_BINARY + 1), &mut out).map_err(|_| Unpack::Broken)?;
        if copied > MAX_BINARY {
            let _ = std::fs::remove_file(to);
            return Err(Unpack::Broken);
        }
        out.flush().map_err(|_| Unpack::Broken)?;
        return Ok(());
    }
    Err(Unpack::Missing)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::whatsapp::testutil::FakeServer;

    const WINDOWS: Target = Target { os: "windows", arch: "amd64" };
    const EXE: &[u8] = b"MZ pretend this is GOWA";

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("coucou-wa-{name}-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A zip like GOWA's: a readme and the binary.
    fn zip_bytes(binary: &str, content: &[u8]) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut out);
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        writer.start_file("readme.md", options).unwrap();
        writer.write_all(b"# GOWA").unwrap();
        writer.start_file(binary, options).unwrap();
        writer.write_all(content).unwrap();
        writer.finish().unwrap();
        out.into_inner()
    }

    fn loopback_http(url: &Url) -> bool {
        url.scheme() == "http" && url.host_str() == Some("127.0.0.1")
    }

    fn sha(bytes: &[u8]) -> String {
        hex(digest::digest(&digest::SHA256, bytes).as_ref())
    }

    /// A fake GitHub: the release, the checksums and the zip. `sum` is what checksums.txt says.
    fn github(zip: Vec<u8>, sum: Option<String>, with_checksums_asset: bool) -> (FakeServer, Source) {
        let server = FakeServer::start(move |req| {
            let base = format!("http://127.0.0.1:{}", req.port);
            match req.path.as_str() {
                "/latest" => {
                    let mut assets = vec![format!(
                        r#"{{"name":"whatsapp_9.6.0_windows_amd64.zip","size":{},"browser_download_url":"{base}/redirect-to-zip"}}"#,
                        zip.len()
                    )];
                    if with_checksums_asset {
                        assets.push(format!(r#"{{"name":"checksums.txt","size":100,"browser_download_url":"{base}/sums"}}"#));
                    }
                    (200, vec![], format!(r#"{{"tag_name":"v9.6.0","assets":[{}]}}"#, assets.join(",")).into_bytes())
                }
                "/sums" => (200, vec![], format!("{}  whatsapp_9.6.0_windows_amd64.zip\n", sum.clone().unwrap_or_default()).into_bytes()),
                "/redirect-to-zip" => (302, vec![("Location".into(), format!("{base}/zip"))], vec![]),
                "/zip" => (200, vec![], zip.clone()),
                _ => (404, vec![], vec![]),
            }
        });
        let latest = format!("http://127.0.0.1:{}/latest", server.port);
        (server, Source { latest_url: latest, allow: loopback_http })
    }

    async fn run(source: &Source, dir: &Path, replaced: &Arc<AtomicBool>, steps: &Arc<Mutex<Vec<Step>>>) -> Result<String, String> {
        let (r, s) = (replaced.clone(), steps.clone());
        install(source, dir, WINDOWS, &move |step| s.lock().unwrap().push(step), &move || r.store(true, Ordering::SeqCst)).await
    }

    #[tokio::test]
    async fn it_downloads_verifies_and_installs() {
        let zip = zip_bytes("windows-amd64.exe", EXE);
        let (_server, source) = github(zip.clone(), Some(sha(&zip)), true);
        let dir = temp_dir("install-ok");
        std::fs::write(dir.join(BINARY_NAME), b"old").unwrap();
        let (replaced, steps) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(vec![])));

        let version = run(&source, &dir, &replaced, &steps).await.unwrap();
        assert_eq!(version, "9.6.0");
        assert_eq!(std::fs::read(dir.join(BINARY_NAME)).unwrap(), EXE, "the new binary took the old one's place");
        assert!(replaced.load(Ordering::SeqCst), "the caller could stop the old one first");
        let seen = steps.lock().unwrap().clone();
        assert_eq!(seen.first(), Some(&Step::Checking));
        assert!(seen.contains(&Step::Verifying) && seen.contains(&Step::Installing));
        assert!(matches!(seen.iter().find(|s| matches!(s, Step::Downloading { .. })), Some(Step::Downloading { megabytes: 1 })));
        // Nothing else is left in the folder.
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, [BINARY_NAME]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// By hand: `cargo test -p coucou --lib real_github -- --ignored`. Downloads about 15 MB.
    #[tokio::test]
    #[ignore = "downloads GOWA from GitHub"]
    async fn real_github_release_installs_on_this_pc() {
        let dir = temp_dir("install-real");
        let (replaced, steps) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(vec![])));
        let version = run(&Source::github(), &dir, &replaced, &steps).await.unwrap();
        assert!(!version.is_empty() && !version.starts_with('v'));
        let binary = dir.join(BINARY_NAME);
        assert!(std::fs::metadata(&binary).unwrap().len() > 10 * 1024 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_download_that_is_not_the_published_file_installs_nothing() {
        let zip = zip_bytes("windows-amd64.exe", EXE);
        let (_server, source) = github(zip, Some("0".repeat(64)), true);
        let dir = temp_dir("install-bad-sum");
        std::fs::write(dir.join(BINARY_NAME), b"old").unwrap();
        let (replaced, steps) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(vec![])));

        let error = run(&source, &dir, &replaced, &steps).await.unwrap_err();
        assert_eq!(error, "The download doesn't match the file GOWA published: it was deleted, nothing was installed.");
        assert!(!replaced.load(Ordering::SeqCst), "the running GOWA was not touched");
        assert_eq!(std::fs::read(dir.join(BINARY_NAME)).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "the download was deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_checksum_at_all_means_nothing_is_installed() {
        let zip = zip_bytes("windows-amd64.exe", EXE);
        // No checksums asset, and this fake GitHub gives no digest either.
        let (_server, source) = github(zip, None, false);
        let dir = temp_dir("install-no-sum");
        let (replaced, steps) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(vec![])));

        let error = run(&source, &dir, &replaced, &steps).await.unwrap_err();
        assert_eq!(error, "GOWA's release has no checksum to check the download against: nothing was installed.");
        assert!(!dir.join(BINARY_NAME).exists());
        assert!(!steps.lock().unwrap().iter().any(|s| matches!(s, Step::Downloading { .. })), "nothing was even downloaded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_zip_without_the_binary_installs_nothing() {
        let zip = zip_bytes("something-else.exe", EXE);
        let (_server, source) = github(zip.clone(), Some(sha(&zip)), true);
        let dir = temp_dir("install-no-binary");
        let (replaced, steps) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(vec![])));

        let error = run(&source, &dir, &replaced, &steps).await.unwrap_err();
        assert!(error.starts_with("No GOWA build for this computer"), "{error}");
        assert!(!replaced.load(Ordering::SeqCst));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_redirect_to_another_host_is_not_followed() {
        let zip = zip_bytes("windows-amd64.exe", EXE);
        let server = FakeServer::start(move |req| {
            let base = format!("http://127.0.0.1:{}", req.port);
            match req.path.as_str() {
                "/latest" => (200, vec![], format!(
                    r#"{{"tag_name":"v9.6.0","assets":[{{"name":"whatsapp_9.6.0_windows_amd64.zip","size":10,"digest":"sha256:{}","browser_download_url":"{base}/zip"}}]}}"#,
                    sha(&zip)
                ).into_bytes()),
                "/zip" => (302, vec![("Location".into(), "http://evil.invalid/zip".into())], vec![]),
                _ => (404, vec![], vec![]),
            }
        });
        let source = Source { latest_url: format!("http://127.0.0.1:{}/latest", server.port), allow: loopback_http };
        let dir = temp_dir("install-redirect");
        let (replaced, steps) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(vec![])));
        let error = run(&source, &dir, &replaced, &steps).await.unwrap_err();
        assert_eq!(error, "The download didn't go through. Check your connection and try again.");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        // And a release address outside the allowed hosts is not even asked.
        let outside = Source { latest_url: "http://127.0.0.1:1/latest".into(), allow: github_only };
        assert!(install(&outside, &dir, WINDOWS, &|_| {}, &|| {}).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_named_entry_is_ever_written() {
        let dir = temp_dir("extract");
        // A zip whose entry tries to climb out of the folder.
        let zip = zip_bytes("../../windows-amd64.exe", EXE);
        let zip_path = dir.join("a.zip");
        std::fs::write(&zip_path, &zip).unwrap();
        let out = dir.join("out.exe");
        extract_binary(&zip_path, "windows-amd64.exe", &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), EXE);
        assert_eq!(extract_binary(&zip_path, "linux-amd64", &out), Err(Unpack::Missing));
        std::fs::write(&zip_path, b"not a zip").unwrap();
        assert_eq!(extract_binary(&zip_path, "windows-amd64.exe", &out), Err(Unpack::Broken));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn github_only_follows_githubs_https_hosts() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(github_only(&url("https://github.com/x")));
        assert!(github_only(&url("https://objects.githubusercontent.com/x")));
        assert!(!github_only(&url("http://github.com/x")));
        assert!(!github_only(&url("https://github.com.evil.io/x")));
    }
}
