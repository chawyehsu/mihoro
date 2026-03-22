pub mod launchd;
pub mod systemd;

use std::process::ExitStatus;

use anyhow::{bail, Result};

use self::systemd::Systemctl;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceManagerKind {
    Auto,
    Systemd,
    Launchd,
}

impl ServiceManagerKind {
    pub fn from_str(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "systemd" => Ok(Self::Systemd),
            "launchd" => Ok(Self::Launchd),
            _ => bail!("unsupported service manager: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectedServiceManager {
    Systemd,
    Launchd,
}

pub struct ServiceManager {
    selected: SelectedServiceManager,
}

impl ServiceManager {
    pub fn new(kind: ServiceManagerKind) -> Result<Self> {
        let selected = select_service_manager(kind, std::env::consts::OS)?;
        Ok(Self { selected })
    }

    pub fn enable(&self, service: &str) -> Result<ExitStatus> {
        match self.selected {
            SelectedServiceManager::Systemd => Systemctl::new().enable(service).execute(),
            SelectedServiceManager::Launchd => launchd::enable(service),
        }
    }

    pub fn start(&self, service: &str) -> Result<ExitStatus> {
        match self.selected {
            SelectedServiceManager::Systemd => Systemctl::new().start(service).execute(),
            SelectedServiceManager::Launchd => launchd::start(service),
        }
    }

    pub fn stop(&self, service: &str) -> Result<ExitStatus> {
        match self.selected {
            SelectedServiceManager::Systemd => Systemctl::new().stop(service).execute(),
            SelectedServiceManager::Launchd => launchd::stop(service),
        }
    }

    pub fn restart(&self, service: &str) -> Result<ExitStatus> {
        match self.selected {
            SelectedServiceManager::Systemd => Systemctl::new().restart(service).execute(),
            SelectedServiceManager::Launchd => launchd::restart(service),
        }
    }

    pub fn status(&self, service: &str) -> Result<ExitStatus> {
        match self.selected {
            SelectedServiceManager::Systemd => Systemctl::new().status(service).execute(),
            SelectedServiceManager::Launchd => launchd::status(service),
        }
    }

    pub fn disable(&self, service: &str) -> Result<ExitStatus> {
        match self.selected {
            SelectedServiceManager::Systemd => Systemctl::new().disable(service).execute(),
            SelectedServiceManager::Launchd => launchd::disable(service),
        }
    }

    pub fn daemon_reload(&self) -> Result<ExitStatus> {
        match self.selected {
            SelectedServiceManager::Systemd => Systemctl::new().daemon_reload().execute(),
            SelectedServiceManager::Launchd => Ok(std::process::Command::new("true").status()?),
        }
    }

    pub fn reset_failed(&self) -> Result<ExitStatus> {
        match self.selected {
            SelectedServiceManager::Systemd => Systemctl::new().reset_failed().execute(),
            SelectedServiceManager::Launchd => Ok(std::process::Command::new("true").status()?),
        }
    }

    pub fn logs(&self, service: &str) -> Result<ExitStatus> {
        match self.selected {
            SelectedServiceManager::Systemd => systemd::journalctl_logs(service),
            SelectedServiceManager::Launchd => launchd::logs(service),
        }
    }
}

fn select_service_manager(kind: ServiceManagerKind, os: &str) -> Result<SelectedServiceManager> {
    match kind {
        ServiceManagerKind::Systemd => Ok(SelectedServiceManager::Systemd),
        ServiceManagerKind::Launchd => Ok(SelectedServiceManager::Launchd),
        ServiceManagerKind::Auto => match os {
            "linux" => Ok(SelectedServiceManager::Systemd),
            "macos" => Ok(SelectedServiceManager::Launchd),
            _ => bail!("unsupported operating system for auto service manager: {os}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_service_manager_kind() {
        assert_eq!(
            ServiceManagerKind::from_str("auto").unwrap(),
            ServiceManagerKind::Auto
        );
        assert_eq!(
            ServiceManagerKind::from_str("systemd").unwrap(),
            ServiceManagerKind::Systemd
        );
        assert_eq!(
            ServiceManagerKind::from_str("launchd").unwrap(),
            ServiceManagerKind::Launchd
        );
        assert!(ServiceManagerKind::from_str("invalid").is_err());
    }

    #[test]
    fn test_select_explicit_launchd_succeeds() {
        let result = ServiceManager::new(ServiceManagerKind::Launchd);
        assert!(result.is_ok());
    }

    #[test]
    fn test_select_auto_linux() {
        let selected = select_service_manager(ServiceManagerKind::Auto, "linux").unwrap();
        assert_eq!(selected, SelectedServiceManager::Systemd);
    }

    #[test]
    fn test_select_auto_macos() {
        let selected = select_service_manager(ServiceManagerKind::Auto, "macos").unwrap();
        assert_eq!(selected, SelectedServiceManager::Launchd);
    }
}
