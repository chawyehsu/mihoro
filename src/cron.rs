use anyhow::{anyhow, Result};
use chrono::{DateTime, Local};
use colored::Colorize;
use std::env;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

const MIHORO_CRON_START_MARKER: &str = "# >>> mihoro auto-update >>>";
const MIHORO_CRON_END_MARKER: &str = "# <<< mihoro auto-update <<<";

fn current_uid_fallback() -> u32 {
    fs::metadata(".").map(|m| m.uid()).unwrap_or(1000)
}

fn resolve_runtime_root(os: &str, xdg_runtime_dir: Option<&str>, tmpdir: Option<&str>) -> PathBuf {
    match os {
        "macos" => PathBuf::from(tmpdir.filter(|value| !value.is_empty()).unwrap_or("/tmp")),
        _ => {
            if let Some(runtime_dir) = xdg_runtime_dir.filter(|value| !value.is_empty()) {
                PathBuf::from(runtime_dir)
            } else {
                let uid = current_uid_fallback();
                PathBuf::from(format!("/run/user/{uid}"))
            }
        }
    }
}

/// Get the path to the user's crontab file
fn crontab_path() -> PathBuf {
    let xdg_runtime_dir = env::var("XDG_RUNTIME_DIR").ok();
    let tmpdir = env::var("TMPDIR").ok();
    let root = resolve_runtime_root(
        std::env::consts::OS,
        xdg_runtime_dir.as_deref(),
        tmpdir.as_deref(),
    );
    root.join("mihoro-crontab")
}

/// Get the mihoro binary path from current executable
fn mihoro_bin_path() -> Result<String> {
    env::current_exe()?
        .to_str()
        .map(String::from)
        .ok_or_else(|| anyhow!("Failed to get mihoro binary path"))
}

/// Generate cron line for auto-update.
fn generate_cron_line(interval_hours: u16) -> Result<String> {
    let bin_path = mihoro_bin_path()?;
    Ok(format!("0 */{} * * * {} update", interval_hours, bin_path))
}

fn generate_managed_block(interval_hours: u16) -> Result<String> {
    let cron_line = generate_cron_line(interval_hours)?;
    Ok(format!(
        "{MIHORO_CRON_START_MARKER}\n{cron_line}\n{MIHORO_CRON_END_MARKER}\n"
    ))
}

fn read_installed_crontab() -> Result<String> {
    let output = Command::new("crontab")
        .arg("-l")
        .output()
        .map_err(|e| anyhow!("Failed to list current crontab: {}", e))?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    if stderr.contains("no crontab for") || stderr.contains("does not exist") {
        return Ok(String::new());
    }

    Err(anyhow!("Failed to list current crontab: {}", stderr.trim()))
}

fn install_crontab(content: &str, crontab_file: &Path) -> Result<()> {
    if let Some(parent) = crontab_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(crontab_file, content)?;
    let status = Command::new("crontab")
        .arg(crontab_file)
        .status()
        .map_err(|e| anyhow!("Failed to install crontab: {}", e))?;

    if !status.success() {
        return Err(anyhow!("Failed to install crontab"));
    }

    Ok(())
}

fn strip_mihoro_block(content: &str) -> (String, bool) {
    let lines: Vec<&str> = content.lines().collect();
    let mut cleaned: Vec<&str> = Vec::with_capacity(lines.len());
    let mut index = 0;
    let mut removed = false;

    while index < lines.len() {
        let line = lines[index];
        if line.trim() == MIHORO_CRON_START_MARKER {
            let mut found_end = None;
            let mut inner = index + 1;
            while inner < lines.len() {
                if lines[inner].trim() == MIHORO_CRON_END_MARKER {
                    found_end = Some(inner);
                    break;
                }
                inner += 1;
            }

            if let Some(end) = found_end {
                removed = true;
                index = end + 1;
                continue;
            }
        }

        cleaned.push(line);
        index += 1;
    }

    let mut result = cleaned.join("\n");
    if !result.is_empty() {
        result.push('\n');
    }
    (result, removed)
}

fn merge_with_mihoro_block(existing: &str, managed_block: &str) -> String {
    let (without_mihoro, _) = strip_mihoro_block(existing);
    let trimmed = without_mihoro.trim_end();
    if trimmed.is_empty() {
        managed_block.to_string()
    } else {
        format!("{trimmed}\n\n{managed_block}")
    }
}

fn find_mihoro_entry(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim() != MIHORO_CRON_START_MARKER {
            index += 1;
            continue;
        }

        let mut inner = index + 1;
        while inner < lines.len() {
            let line = lines[inner].trim();
            if line == MIHORO_CRON_END_MARKER {
                break;
            }
            if !line.is_empty() {
                return Some(line.to_string());
            }
            inner += 1;
        }
        index = inner.saturating_add(1);
    }
    None
}

/// Enable auto-update by installing cron job
pub fn enable_auto_update(interval_hours: u16, prefix: &str) -> Result<()> {
    if interval_hours == 0 {
        println!(
            "{} Auto-update interval is 0, disabling auto-update",
            prefix.yellow()
        );
        return disable_auto_update(prefix);
    }

    if interval_hours > 24 {
        anyhow::bail!("Auto-update interval must be between 1 and 24 hours");
    }

    let existing = read_installed_crontab()?;
    let managed_block = generate_managed_block(interval_hours)?;
    let crontab_content = merge_with_mihoro_block(&existing, &managed_block);
    let crontab_file = crontab_path();
    install_crontab(&crontab_content, &crontab_file)?;

    println!(
        "{} Auto-update enabled with interval: {} hours",
        prefix.green().bold(),
        interval_hours.to_string().yellow()
    );
    println!(
        "{} Cron entry: {}",
        "->".dimmed(),
        generate_cron_line(interval_hours)?
    );

    Ok(())
}

/// Disable auto-update by removing cron job
pub fn disable_auto_update(prefix: &str) -> Result<()> {
    let existing = read_installed_crontab()?;
    let (without_mihoro, had_mihoro_block) = strip_mihoro_block(&existing);

    if had_mihoro_block {
        let crontab_file = crontab_path();
        install_crontab(&without_mihoro, &crontab_file)?;
        println!("{} Auto-update disabled", prefix.green().bold());
    } else {
        println!(
            "{} Auto-update disabled (no active cron job)",
            prefix.yellow()
        );
    }

    Ok(())
}

/// Format Unix timestamp to local datetime string.
fn format_datetime(secs: u64) -> String {
    let ts = UNIX_EPOCH + Duration::from_secs(secs);
    let local: DateTime<Local> = DateTime::from(ts);
    local.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Get current cron status
pub fn get_cron_status(_prefix: &str, mihomo_config_path: &str) -> Result<()> {
    let installed = read_installed_crontab()?;
    let cron_entry = find_mihoro_entry(&installed);

    if cron_entry.is_none() {
        println!("{} Auto-update is disabled", "status:".yellow().bold());
        return Ok(());
    }

    println!("{} Auto-update is enabled", "status:".green().bold());
    println!(
        "{} {}",
        "->".dimmed(),
        cron_entry.unwrap_or_default().dimmed()
    );

    // Show last updated time from mihomo config file
    let config_path = Path::new(mihomo_config_path);
    if let Ok(metadata) = fs::metadata(config_path) {
        if let Ok(modified) = metadata.modified() {
            if let Ok(duration) = modified.duration_since(UNIX_EPOCH) {
                let secs = duration.as_secs();
                let datetime = format_datetime(secs);
                println!("{} Last updated: {}", "->".dimmed(), datetime.dimmed());
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_cron_line() {
        let line = generate_cron_line(12).unwrap();
        assert!(line.contains("0 */12 * * *"));
        assert!(line.contains("update"));
    }

    #[test]
    fn test_generate_managed_block() {
        let block = generate_managed_block(6).unwrap();
        assert!(block.contains(MIHORO_CRON_START_MARKER));
        assert!(block.contains("0 */6 * * *"));
        assert!(block.contains(MIHORO_CRON_END_MARKER));
    }

    #[test]
    fn test_format_datetime_from_unix_secs() {
        let formatted = format_datetime(1_700_000_000);
        assert!(!formatted.is_empty());
        assert!(formatted.contains('-'));
        assert!(formatted.contains(':'));
    }

    #[test]
    fn test_resolve_runtime_root_macos_uses_tmpdir() {
        let root = resolve_runtime_root("macos", Some("/run/user/1000"), Some("/var/tmp"));
        assert_eq!(root, PathBuf::from("/var/tmp"));
    }

    #[test]
    fn test_resolve_runtime_root_macos_fallback_to_tmp() {
        let root = resolve_runtime_root("macos", None, None);
        assert_eq!(root, PathBuf::from("/tmp"));
    }

    #[test]
    fn test_strip_mihoro_block_keeps_unrelated_entries() {
        let input = "MAILTO=user@example.com\n# >>> mihoro auto-update >>>\n0 */12 * * * /usr/bin/mihoro update\n# <<< mihoro auto-update <<<\n0 1 * * * /usr/bin/backup\n";
        let (cleaned, removed) = strip_mihoro_block(input);
        assert!(removed);
        assert!(cleaned.contains("MAILTO=user@example.com"));
        assert!(cleaned.contains("0 1 * * * /usr/bin/backup"));
        assert!(!cleaned.contains("mihoro update"));
    }

    #[test]
    fn test_merge_with_mihoro_block_is_idempotent() {
        let block = generate_managed_block(12).unwrap();
        let first = merge_with_mihoro_block("0 1 * * * /usr/bin/backup\n", &block);
        let second = merge_with_mihoro_block(&first, &block);
        assert_eq!(first, second);
    }

    #[test]
    fn test_find_mihoro_entry_from_block() {
        let block = generate_managed_block(12).unwrap();
        let content = format!("0 1 * * * /usr/bin/backup\n\n{block}");
        let entry = find_mihoro_entry(&content);
        assert!(entry.is_some());
        assert!(entry.unwrap_or_default().contains("mihoro"));
    }
}
