//! Updates from the repo's public GitHub releases (as in SMDMAP, minus the token). The app
//! downloads the new version itself: macOS doesn't quarantine what an app downloads this way, so
//! it doesn't ask again about an app from an unidentified developer (it does for a DMG from the
//! browser).

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const REPO: &str = "Ntrondle/ptlabel";

/// A release newer than this build.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    /// The download URL of its DMG.
    asset: String,
}

/// The app bundle this runs from (…/PTLabel.app). None for a bare binary (cargo run): nothing to
/// update.
pub fn bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf)
}

fn agent(timeout: Duration) -> ureq::Agent {
    use ureq::tls::{TlsConfig, TlsProvider};
    // The download redirects to GitHub's storage; ureq follows it.
    ureq::Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).build())
        .timeout_global(Some(timeout))
        .user_agent("ptlabel")
        .build()
        .into()
}

/// The latest release, if it is newer than this build. Anonymous: 60 requests/hour per IP is
/// plenty for a check every few hours.
pub fn latest() -> Result<Option<Release>> {
    let v: Value = agent(Duration::from_secs(20))
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .context("GitHub releases")?
        .body_mut()
        .read_json()
        .context("GitHub's reply")?;
    let version = v["tag_name"].as_str().unwrap_or_default().trim_start_matches('v').to_string();
    if !newer(&version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    // The release workflow attaches PTLabel-v<version>-macos-arm64.dmg.
    let asset = v["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str().is_some_and(|n| n.starts_with("PTLabel") && n.ends_with(".dmg")))
        .and_then(|a| a["browser_download_url"].as_str())
        .with_context(|| format!("release {version} has no PTLabel DMG"))?;
    Ok(Some(Release { version, asset: asset.into() }))
}

/// `a` is a later version than `b`: "2.0.10" > "2.0.9".
fn newer(a: &str, b: &str) -> bool {
    let parts = |s: &str| s.split('.').map(|x| x.parse::<u64>().ok()).collect::<Option<Vec<_>>>();
    matches!((parts(a), parts(b)), (Some(a), Some(b)) if a > b)
}

/// Download `r` and put its app in place of `app`, the running bundle.
pub fn install(r: &Release, app: &Path) -> Result<()> {
    let dir = std::env::temp_dir().join(format!("ptlabel-update-{}", r.version));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let dmg = dir.join("PTLabel.dmg");
    let mut res = agent(Duration::from_secs(600))
        .get(&r.asset)
        .call()
        .context("download")?;
    let mut f = std::fs::File::create(&dmg)?;
    std::io::copy(&mut res.body_mut().as_reader(), &mut f).context("download")?;
    drop(f);
    let mnt = dir.join("mnt");
    run(Command::new("hdiutil").args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"]).arg(&mnt).arg(&dmg))?;
    let done = replace(&mnt.join("PTLabel.app"), app);
    let _ = Command::new("hdiutil").args(["detach", "-quiet"]).arg(&mnt).status();
    let _ = std::fs::remove_dir_all(&dir);
    done
}

/// Copy `new` next to `app`, then swap them by renaming (same folder, so the swap is a rename,
/// undone if its second half fails).
fn replace(new: &Path, app: &Path) -> Result<()> {
    if !new.join("Contents/MacOS/PTLabel").exists() {
        bail!("no PTLabel.app in the update");
    }
    run(Command::new("codesign").args(["--verify", "--deep"]).arg(new)).context("the update's signature")?;
    let dir = app.parent().context("the app's folder")?;
    let (staged, old) = (dir.join(".PTLabel-update.app"), dir.join(".PTLabel-old.app"));
    let _ = std::fs::remove_dir_all(&staged);
    let _ = std::fs::remove_dir_all(&old);
    run(Command::new("ditto").arg(new).arg(&staged))?;
    std::fs::rename(app, &old).context("move the old app aside")?;
    if let Err(e) = std::fs::rename(&staged, app) {
        let _ = std::fs::rename(&old, app);
        let _ = std::fs::remove_dir_all(&staged);
        return Err(e).context("put the new app in place");
    }
    let _ = std::fs::remove_dir_all(&old);
    let _ = Command::new("xattr").args(["-dr", "com.apple.quarantine"]).arg(app).status();
    Ok(())
}

fn run(c: &mut Command) -> Result<()> {
    let o = c.output().with_context(|| format!("run {}", c.get_program().to_string_lossy()))?;
    if !o.status.success() {
        bail!("{}: {}", c.get_program().to_string_lossy(), String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}

/// Open `app` again once this process has quit.
pub fn relaunch(app: &Path) {
    let _ = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; exec /usr/bin/open "$2""#)
        .arg("sh")
        .arg(std::process::id().to_string())
        .arg(app)
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(newer("2.0.10", "2.0.9"));
        assert!(newer("2.1.0", "2.0.99"));
        assert!(newer("3.0.0", "2.9.9"));
        assert!(!newer("2.0.0", "2.0.0"));
        assert!(!newer("1.0.0", "2.0.0"));
        assert!(!newer("", "2.0.0"));
        assert!(!newer("v2.0.1", "2.0.0"), "the caller strips the v");
        assert!(!newer("2.0.1-rc1", "2.0.0"));
    }
}
