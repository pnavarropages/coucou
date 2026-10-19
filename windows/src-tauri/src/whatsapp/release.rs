// GOWA's own releases on GitHub, for "Install WhatsApp" (one click, no terminal): which zip fits
// this PC and the SHA-256 GOWA publishes for it. GOWA's release workflow (GoReleaser,
// .github/workflows/release.yml in its repo) names the archives
// `whatsapp_<version>_<os>_<arch>.zip` (`windows_amd64`, `linux_amd64`…; the binary inside is
// `<os>-<arch>[.exe]`) next to `checksums.txt` (`checksums-macos.txt` for macOS).

use serde_json::Value;

pub const LATEST_URL: &str = "https://api.github.com/repos/aldinokemal/go-whatsapp-web-multidevice/releases/latest";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// "sha256:<hex>" when GitHub gives it.
    pub digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// Without the leading "v".
    pub version: String,
    pub assets: Vec<Asset>,
}

/// `GET /releases/latest` → `{tag_name, assets: [{name, browser_download_url, size, digest?}]}`.
pub fn parse(body: &[u8]) -> Option<Release> {
    let root: Value = serde_json::from_slice(body).ok()?;
    let tag = root.get("tag_name")?.as_str()?;
    let assets = root
        .get("assets")?
        .as_array()?
        .iter()
        .filter_map(|a| {
            Some(Asset {
                name: a.get("name")?.as_str()?.to_string(),
                url: a.get("browser_download_url")?.as_str()?.to_string(),
                size: a.get("size").and_then(Value::as_u64).unwrap_or(0),
                digest: a.get("digest").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect();
    Some(Release { version: tag.strip_prefix('v').unwrap_or(tag).to_string(), assets })
}

/// What this build downloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub os: &'static str,
    pub arch: &'static str,
}

impl Target {
    /// The PC Coucou runs on, when GOWA publishes a build for it.
    pub fn current() -> Option<Target> {
        let os = match std::env::consts::OS {
            "windows" => "windows",
            "linux" => "linux",
            _ => return None,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            _ => return None,
        };
        Some(Target { os, arch })
    }

    /// The binary's name inside the zip.
    pub fn binary_name(&self) -> String {
        let exe = if self.os == "windows" { ".exe" } else { "" };
        format!("{}-{}{exe}", self.os, self.arch)
    }

    fn checksums_name(&self) -> &'static str {
        "checksums.txt"
    }
}

/// The zip for this PC and the checksums file.
pub fn pick(assets: &[Asset], target: Target) -> Option<(&Asset, Option<&Asset>)> {
    let suffix = format!("_{}_{}.zip", target.os, target.arch);
    let zip = assets.iter().find(|a| a.name.starts_with("whatsapp_") && a.name.ends_with(&suffix))?;
    Some((zip, assets.iter().find(|a| a.name == target.checksums_name())))
}

/// The SHA-256 (lowercase hex) of `asset` in a GoReleaser checksums file ("<hex>  <name>" per
/// line), else from GitHub's own "sha256:<hex>" digest. None when neither is there: nothing is
/// installed without a checksum.
pub fn expected_sha256(asset: &Asset, checksums: Option<&str>) -> Option<String> {
    for line in checksums.unwrap_or("").lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name), None) = (parts.next(), parts.next(), parts.next()) else { continue };
        // Binary-mode checksums write "*name".
        if name.trim_start_matches('*') == asset.name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    let digest = asset.digest.as_deref()?.strip_prefix("sha256:")?;
    (digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit())).then(|| digest.to_ascii_lowercase())
}

/// Only GitHub serves the download (its assets redirect to githubusercontent.com).
pub fn allowed_host(host: Option<&str>) -> bool {
    let Some(h) = host else { return false };
    let h = h.to_ascii_lowercase();
    h == "github.com" || h == "api.github.com" || h.ends_with(".githubusercontent.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOWS: Target = Target { os: "windows", arch: "amd64" };

    // Shape of GET /repos/aldinokemal/go-whatsapp-web-multidevice/releases/latest, asset names as
    // GOWA's GoReleaser workflow writes them (checked against v9.6.0).
    fn release() -> Release {
        let json = r#"{"tag_name":"v9.6.0","assets":[
            {"name":"checksums-macos.txt","size":196,"browser_download_url":"https://github.com/aldinokemal/go-whatsapp-web-multidevice/releases/download/v9.6.0/checksums-macos.txt"},
            {"name":"checksums.txt","size":582,"browser_download_url":"https://github.com/aldinokemal/go-whatsapp-web-multidevice/releases/download/v9.6.0/checksums.txt"},
            {"name":"whatsapp_9.6.0_darwin_arm64.zip","size":11505540,"browser_download_url":"https://github.com/x/darwin_arm64.zip"},
            {"name":"whatsapp_9.6.0_linux_amd64.zip","size":12252905,"browser_download_url":"https://github.com/x/linux_amd64.zip"},
            {"name":"whatsapp_9.6.0_windows_386.zip","size":12254948,"browser_download_url":"https://github.com/x/windows_386.zip"},
            {"name":"whatsapp_9.6.0_windows_amd64.zip","size":12517486,"digest":"sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","browser_download_url":"https://github.com/aldinokemal/go-whatsapp-web-multidevice/releases/download/v9.6.0/whatsapp_9.6.0_windows_amd64.zip"}]}"#;
        parse(json.as_bytes()).unwrap()
    }

    #[test]
    fn the_version_comes_without_the_v() {
        assert_eq!(release().version, "9.6.0");
        assert!(parse(b"{}").is_none());
        assert!(parse(b"nope").is_none());
    }

    #[test]
    fn the_windows_zip_and_its_checksums_file() {
        let r = release();
        let (zip, sums) = pick(&r.assets, WINDOWS).unwrap();
        assert_eq!(zip.name, "whatsapp_9.6.0_windows_amd64.zip");
        assert_eq!(sums.map(|a| a.name.as_str()), Some("checksums.txt"));
        assert_eq!(WINDOWS.binary_name(), "windows-amd64.exe");
        assert_eq!(Target { os: "linux", arch: "amd64" }.binary_name(), "linux-amd64");
    }

    #[test]
    fn never_another_systems_zip() {
        let r = release();
        let only_other: Vec<Asset> = r.assets.iter().filter(|a| !a.name.contains("windows")).cloned().collect();
        assert!(pick(&only_other, WINDOWS).is_none());
        let (zip, _) = pick(&r.assets, Target { os: "windows", arch: "386" }).unwrap();
        assert_eq!(zip.name, "whatsapp_9.6.0_windows_386.zip");
        assert!(pick(&r.assets, Target { os: "windows", arch: "arm64" }).is_none());
    }

    #[test]
    fn the_checksum_comes_from_gowas_file_else_githubs_digest_else_nothing() {
        let r = release();
        let (zip, _) = pick(&r.assets, WINDOWS).unwrap();
        let sums = format!(
            "{}  whatsapp_9.6.0_windows_386.zip\n{} *whatsapp_9.6.0_windows_amd64.zip\n",
            "1".repeat(64),
            "2".repeat(64).to_uppercase()
        );
        assert_eq!(expected_sha256(zip, Some(&sums)), Some("2".repeat(64)));
        assert_eq!(expected_sha256(zip, None), Some("a".repeat(64)), "else GitHub's digest");
        let bare = Asset { name: "x.zip".into(), url: "https://github.com".into(), size: 1, digest: None };
        assert_eq!(expected_sha256(&bare, Some(&sums)), None, "no checksum at all, nothing installed");
        // A line that is not a checksum for this file is no checksum.
        assert_eq!(expected_sha256(zip, Some("deadbeef  whatsapp_9.6.0_windows_amd64.zip\n")), Some("a".repeat(64)));
        let short = Asset { digest: Some("sha256:abc".into()), ..bare };
        assert_eq!(expected_sha256(&short, None), None);
    }

    #[test]
    fn the_download_may_only_follow_githubs_own_hosts() {
        assert!(allowed_host(Some("github.com")));
        assert!(allowed_host(Some("api.github.com")));
        assert!(allowed_host(Some("objects.githubusercontent.com")));
        assert!(allowed_host(Some("Release-Assets.GitHubUserContent.com")));
        assert!(!allowed_host(Some("github.com.evil.io")));
        assert!(!allowed_host(Some("evilgithubusercontent.com")));
        assert!(!allowed_host(Some("127.0.0.1")));
        assert!(!allowed_host(None));
    }
}
