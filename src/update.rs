//! Updates from the repo's public GitHub releases.
//! - macOS: downloads the DMG, verifies its signature, swaps `PTLabel.app` in place and reopens it.
//! - Windows: downloads `PTLabel-…-windows-x86_64.exe`, swaps the running `.exe` in place and relaunches it.

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
    /// Download URL of this platform's release asset (DMG on macOS, EXE on Windows).
    asset: String,
}

/// Remove a `.old` executable left behind by a previous Windows self-update.
pub fn cleanup_old() {
    #[cfg(target_os = "windows")]
    if let Ok(exe) = std::env::current_exe() {
        let old = old_exe_path(&exe);
        let _ = std::fs::remove_file(old);
    }
}

#[cfg(target_os = "windows")]
fn old_exe_path(exe: &Path) -> PathBuf {
    let mut s = exe.as_os_str().to_os_string();
    s.push(".old");
    PathBuf::from(s)
}

/// The installed app bundle / executable that can update itself. None for a bare `cargo run` build.
#[cfg(target_os = "macos")]
pub fn bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf)
}

#[cfg(target_os = "windows")]
pub fn bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // Skip dev builds inside Cargo's target/debug or target/release directory.
    let s = exe.to_string_lossy().to_ascii_lowercase();
    if s.contains(r"\target\debug\") || s.contains(r"\target\release\") {
        return None;
    }
    Some(exe)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn bundle() -> Option<PathBuf> {
    None
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

fn asset_matches(name: &str) -> bool {
    if !name.starts_with("PTLabel") {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        name.ends_with(".dmg")
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        name.ends_with("-windows-arm64.exe")
    }
    #[cfg(all(target_os = "windows", not(target_arch = "aarch64")))]
    {
        name.ends_with("-windows-x86_64.exe")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        false
    }
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
    let asset = v["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str().is_some_and(asset_matches))
        .and_then(|a| a["browser_download_url"].as_str())
        .with_context(|| format!("release {version} has no PTLabel package for this OS"))?;
    Ok(Some(Release { version, asset: asset.into() }))
}

/// `a` is a later version than `b`: "2.0.10" > "2.0.9".
fn newer(a: &str, b: &str) -> bool {
    let parts = |s: &str| s.split('.').map(|x| x.parse::<u64>().ok()).collect::<Option<Vec<_>>>();
    matches!((parts(a), parts(b)), (Some(a), Some(b)) if a > b)
}

/// Download `r` and put its app in place of `app`, the running bundle.
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
fn run(c: &mut Command) -> Result<()> {
    let o = c.output().with_context(|| format!("run {}", c.get_program().to_string_lossy()))?;
    if !o.status.success() {
        bail!("{}: {}", c.get_program().to_string_lossy(), String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}

/// Download `r` next to the running `.exe` and swap it in place (Windows allows renaming a running `.exe`).
#[cfg(target_os = "windows")]
pub fn install(r: &Release, exe: &Path) -> Result<()> {
    let mut staged_s = exe.as_os_str().to_os_string();
    staged_s.push(".new");
    let staged = PathBuf::from(staged_s);
    let old = old_exe_path(exe);
    let _ = std::fs::remove_file(&staged);
    let _ = std::fs::remove_file(&old);

    let mut res = agent(Duration::from_secs(600))
        .get(&r.asset)
        .call()
        .context("download")?;
    let mut f = std::fs::File::create(&staged).context("create update file")?;
    let bytes = std::io::copy(&mut res.body_mut().as_reader(), &mut f).context("download")?;
    drop(f);

    let mut hdr = [0u8; 2];
    let ok = bytes > 100_000
        && std::fs::File::open(&staged)
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut hdr))
            .is_ok()
        && &hdr == b"MZ";
    if !ok {
        let _ = std::fs::remove_file(&staged);
        bail!("downloaded file is not a valid Windows executable");
    }

    std::fs::rename(exe, &old).context("move the old executable aside")?;
    if let Err(e) = std::fs::rename(&staged, exe) {
        let _ = std::fs::rename(&old, exe);
        let _ = std::fs::remove_file(&staged);
        return Err(e).context("put the new executable in place");
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn install(_: &Release, _: &Path) -> Result<()> {
    bail!("auto-update is not supported on this OS")
}

/// Open `app` again once this process has quit.
#[cfg(target_os = "macos")]
pub fn relaunch(app: &Path) {
    let _ = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; exec /usr/bin/open "$2""#)
        .arg("sh")
        .arg(std::process::id().to_string())
        .arg(app)
        .spawn();
}

#[cfg(target_os = "windows")]
pub fn relaunch(exe: &Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let target = exe.display().to_string().replace('"', "");
    let _ = Command::new("cmd")
        .args(["/C", &format!("ping 127.0.0.1 -n 2 >NUL & start \"\" \"{target}\"")])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn relaunch(_: &Path) {}

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
