use std::process::{Command, ExitStatus};

use anyhow::{Context, Result};

pub struct Systemctl {
    systemctl: Command,
    args: Vec<String>,
}

impl Systemctl {
    pub fn new() -> Self {
        Self {
            systemctl: Command::new("systemctl"),
            args: Vec::new(),
        }
    }

    pub fn enable(&mut self, service: &str) -> &mut Self {
        self.systemctl.arg("--user").arg("enable").arg(service);
        self.args
            .extend(["--user", "enable", service].map(String::from));
        self
    }

    pub fn start(&mut self, service: &str) -> &mut Self {
        self.systemctl.arg("--user").arg("start").arg(service);
        self.args
            .extend(["--user", "start", service].map(String::from));
        self
    }

    pub fn stop(&mut self, service: &str) -> &mut Self {
        self.systemctl.arg("--user").arg("stop").arg(service);
        self.args
            .extend(["--user", "stop", service].map(String::from));
        self
    }

    pub fn restart(&mut self, service: &str) -> &mut Self {
        self.systemctl.arg("--user").arg("restart").arg(service);
        self.args
            .extend(["--user", "restart", service].map(String::from));
        self
    }

    pub fn status(&mut self, service: &str) -> &mut Self {
        self.systemctl.arg("--user").arg("status").arg(service);
        self.args
            .extend(["--user", "status", service].map(String::from));
        self
    }

    pub fn disable(&mut self, service: &str) -> &mut Self {
        self.systemctl.arg("--user").arg("disable").arg(service);
        self.args
            .extend(["--user", "disable", service].map(String::from));
        self
    }

    pub fn daemon_reload(&mut self) -> &mut Self {
        self.systemctl.arg("--user").arg("daemon-reload");
        self.args
            .extend(["--user", "daemon-reload"].map(String::from));
        self
    }

    pub fn reset_failed(&mut self) -> &mut Self {
        self.systemctl.arg("--user").arg("reset-failed");
        self.args
            .extend(["--user", "reset-failed"].map(String::from));
        self
    }

    #[allow(unused)]
    pub fn command_parts(&self) -> (&str, &[String]) {
        ("systemctl", &self.args)
    }

    pub fn execute(&mut self) -> Result<ExitStatus> {
        self.systemctl
            .spawn()?
            .wait()
            .with_context(|| "failed to execute systemctl")
    }

    /// Returns `true` if the given user service is currently active.
    pub fn is_active(&mut self, service: &str) -> bool {
        self.systemctl
            .arg("--user")
            .arg("is-active")
            .arg("--quiet")
            .arg(service)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Returns `true` if the given user service is enabled for autostart.
    pub fn is_enabled(&mut self, service: &str) -> bool {
        self.systemctl
            .arg("--user")
            .arg("is-enabled")
            .arg("--quiet")
            .arg(service)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// Render the systemd unit file content for the mihomo service.
///
/// Reference: https://wiki.metacubex.one/startup/service/
pub fn render_service_string(binary_path: &str, config_root: &str) -> String {
    format!(
        "[Unit]
Description=mihomo Daemon, Another Clash Kernel.
After=network.target NetworkManager.service systemd-networkd.service iwd.service

[Service]
Type=simple
LimitNPROC=4096
LimitNOFILE=65536
Restart=always
ExecStartPre=/usr/bin/sleep 1s
ExecStart={} -d {}
ExecReload=/bin/kill -HUP $MAINPID

[Install]
WantedBy=default.target",
        binary_path, config_root
    )
}

pub fn journalctl_logs(service: &str) -> Result<ExitStatus> {
    Command::new("journalctl")
        .arg("--user")
        .arg("-xeu")
        .arg(service)
        .arg("-n")
        .arg("10")
        .arg("-f")
        .spawn()?
        .wait()
        .with_context(|| "failed to execute journalctl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_start_command_contract() {
        let mut systemctl = Systemctl::new();
        systemctl.start("mihomo.service");
        let (program, args) = systemctl.command_parts();
        assert_eq!(program, "systemctl");
        assert_eq!(args, &["--user", "start", "mihomo.service"]);
    }

    #[test]
    fn test_status_command_contract() {
        let mut systemctl = Systemctl::new();
        systemctl.status("mihomo.service");
        let (program, args) = systemctl.command_parts();
        assert_eq!(program, "systemctl");
        assert_eq!(args, &["--user", "status", "mihomo.service"]);
    }

    #[test]
    fn test_restart_command_contract() {
        let mut systemctl = Systemctl::new();
        systemctl.restart("mihomo.service");
        let (program, args) = systemctl.command_parts();
        assert_eq!(program, "systemctl");
        assert_eq!(args, &["--user", "restart", "mihomo.service"]);
    }

    #[test]
    fn test_uninstall_related_commands_contract() {
        let mut systemctl = Systemctl::new();
        systemctl
            .stop("mihomo.service")
            .disable("mihomo.service")
            .daemon_reload()
            .reset_failed();
        let (program, args) = systemctl.command_parts();
        assert_eq!(program, "systemctl");
        assert_eq!(
            args,
            &[
                "--user",
                "stop",
                "mihomo.service",
                "--user",
                "disable",
                "mihomo.service",
                "--user",
                "daemon-reload",
                "--user",
                "reset-failed",
            ]
        );
    }
}
