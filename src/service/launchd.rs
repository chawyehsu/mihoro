use std::env;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

use anyhow::{anyhow, bail, Context, Result};

pub fn default_plist_path(service: &str) -> Result<PathBuf> {
    let home = env::var("HOME").with_context(|| "HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", service_stem(service))))
}

pub fn service_stem(service: &str) -> &str {
    service.strip_suffix(".service").unwrap_or(service)
}

pub fn service_label(service: &str) -> String {
    service_stem(service).to_string()
}

pub fn build_plist(service: &str, mihomo_binary_path: &str, mihomo_config_root: &str) -> String {
    let label = service_label(service);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string>
    <string>-c</string>
    <string>{mihomo_binary_path} -d {mihomo_config_root} 2>&1 | logger</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>WorkingDirectory</key>
  <string>{mihomo_config_root}</string>
</dict>
</plist>
"#
    )
}

fn current_uid() -> Result<String> {
    if let Ok(uid) = env::var("UID") {
        if !uid.is_empty() {
            return Ok(uid);
        }
    }

    let output = Command::new("id")
        .arg("-u")
        .output()
        .with_context(|| "failed to run `id -u` for launchctl domain target")?;
    if !output.status.success() {
        bail!("failed to resolve uid for launchctl domain target");
    }
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uid.is_empty() {
        bail!("received empty uid from `id -u`");
    }
    Ok(uid)
}

fn domain_target() -> Result<String> {
    Ok(format!("gui/{}", current_uid()?))
}

fn service_target(service: &str) -> Result<String> {
    Ok(format!("{}/{}", domain_target()?, service_label(service)))
}

fn run_launchctl(args: &[&str]) -> Result<ExitStatus> {
    Command::new("launchctl")
        .args(args)
        .spawn()?
        .wait()
        .with_context(|| "failed to execute launchctl")
}

fn run_launchctl_output(args: &[&str]) -> Result<std::process::Output> {
    Command::new("launchctl")
        .args(args)
        .output()
        .with_context(|| "failed to execute launchctl")
}

fn stderr_contains(output: &std::process::Output, pattern: &str) -> bool {
    String::from_utf8_lossy(&output.stderr)
        .to_lowercase()
        .contains(&pattern.to_lowercase())
}

pub fn is_loaded(service: &str) -> bool {
    let target = match service_target(service) {
        Ok(t) => t,
        Err(_) => return false,
    };
    match run_launchctl_output(&["print", &target]) {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

pub fn enable(service: &str) -> Result<ExitStatus> {
    if is_loaded(service) {
        return Ok(ExitStatus::from_raw(0));
    }
    let domain = domain_target()?;
    let plist = default_plist_path(service)?;
    let plist_str = plist
        .to_str()
        .ok_or_else(|| anyhow!("invalid plist path: {}", plist.display()))?;
    let output = run_launchctl_output(&["bootstrap", &domain, plist_str])?;
    if output.status.success() {
        return Ok(output.status);
    }

    bail!(
        "launchctl bootstrap failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

#[inline(always)]
pub fn start(service: &str) -> Result<ExitStatus> {
    enable(service)
}

#[inline(always)]
pub fn stop(service: &str) -> Result<ExitStatus> {
    disable(service)
}

pub fn restart(service: &str) -> Result<ExitStatus> {
    if !is_loaded(service) {
        return start(service);
    }

    let target = service_target(service)?;
    run_launchctl(&["kickstart", "-k", &target])
}

pub fn status(service: &str) -> Result<ExitStatus> {
    let target = service_target(service)?;
    run_launchctl(&["print", &target])
}

pub fn disable(service: &str) -> Result<ExitStatus> {
    if !is_loaded(service) {
        return Ok(ExitStatus::from_raw(0));
    }

    let domain = domain_target()?;
    let target = service_target(service)?;
    let plist = default_plist_path(service)?;
    let plist_str = plist
        .to_str()
        .ok_or_else(|| anyhow!("invalid plist path: {}", plist.display()))?;

    let output = run_launchctl_output(&["bootout", &target])?;
    if output.status.success() {
        return Ok(output.status);
    }

    let fallback = run_launchctl_output(&["bootout", &domain, plist_str])?;
    if fallback.status.success() {
        return Ok(fallback.status);
    }

    let missing_target = stderr_contains(&output, "could not find service")
        || stderr_contains(&output, "not loaded")
        || stderr_contains(&output, "no such process");
    let missing_fallback = stderr_contains(&fallback, "could not find service")
        || stderr_contains(&fallback, "not loaded")
        || stderr_contains(&fallback, "no such process");
    if missing_target || missing_fallback {
        return Ok(fallback.status);
    }

    bail!(
        "launchctl bootout failed: {}",
        String::from_utf8_lossy(&fallback.stderr).trim()
    )
}

pub fn logs(_: &str) -> Result<ExitStatus> {
    Command::new("log")
        .arg("stream")
        .arg("--style")
        .arg("syslog")
        .arg("--predicate")
        .arg("process == \"logger\" AND composedMessage CONTAINS \"time=\" AND composedMessage CONTAINS \"msg=\"")
        .spawn()?
        .wait()
        .with_context(|| "failed to execute `log stream`")
}

pub fn is_active(service: &str) -> bool {
    let target = match service_target(service) {
        Ok(t) => t,
        Err(_) => return false,
    };
    match run_launchctl_output(&["list", &target]) {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_stem_and_label() {
        assert_eq!(service_stem("mihomo.service"), "mihomo");
        assert_eq!(service_stem("mihomo"), "mihomo");
        assert_eq!(service_label("mihomo.service"), "mihomo");
    }

    #[test]
    fn test_build_plist_contains_required_keys() {
        let plist = build_plist(
            "mihomo.service",
            "/tmp/test/mihomo",
            "/tmp/test/mihomo-config",
        );
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains("<string>mihomo</string>"));
        assert!(plist.contains("<key>ProgramArguments</key>"));
        assert!(plist.contains("<string>/bin/sh</string>"));
        assert!(plist.contains("<string>-c</string>"));
        assert!(plist.contains(
            "<string>/tmp/test/mihomo -d /tmp/test/mihomo-config 2>&1 | logger</string>"
        ));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
    }
}
