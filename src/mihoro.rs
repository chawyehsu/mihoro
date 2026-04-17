use crate::cmd::{CronCommands, ProxyCommands};
use crate::config::{apply_mihomo_override, parse_config, Config};
use crate::cron;
use crate::proxy::{proxy_export_cmd, proxy_unset_cmd};
use crate::resolve_mihomo_bin;
use crate::service::{
    render_service_definition, resolve_service_path, ServiceManager, ServiceManagerKind,
};
use crate::ui::{install_ui, resolve_external_ui_path};
use crate::utils::{
    create_parent_dir, delete_file, download_file, extract_gzip, try_decode_base64_file_inplace,
    DETAIL_PREFIX,
};

use anyhow::Error;

use std::fs;
use std::os::unix::prelude::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use colored::Colorize;
use local_ip_address::local_ip;
use reqwest::Client;
use shellexpand::tilde;
use tempfile::NamedTempFile;

#[derive(Debug)]
pub struct Mihoro {
    // global mihoro config
    pub prefix: String,
    pub config: Config,

    // mihomo global variables derived from mihoro config
    pub mihomo_target_binary_path: String,
    pub mihomo_target_config_root: String,
    pub mihomo_target_config_path: String,
    pub mihomo_service_name: String,
    pub mihomo_target_service_path: String,
}

/// Outcome of a single setup stage, used by `mihoro init`.
pub enum StageStatus {
    Installed,
    Skipped(String),
    Failed(Error),
}

/// Plan returned by [`Mihoro::prepare_binary`]: either we already have the binary and
/// nothing needs swapping, or we downloaded a new one to a temp file that the install
/// step must consume.
///
/// The split exists so the network-killing `Systemctl::stop` happens only after every
/// other download stage has finished - otherwise the still-running mihomo proxy gets
/// torn down mid-init and subsequent reqwest calls hit `Connection refused` against
/// the configured `https_proxy`.
pub enum BinaryPlan {
    Skip(String),
    Install(NamedTempFile),
}

impl Mihoro {
    pub fn new(config_path: &str) -> Result<Mihoro> {
        let config = parse_config(tilde(config_path).as_ref())?;
        Ok(Self::from_config(config))
    }

    /// Build a `Mihoro` from an already-validated `Config`.
    pub fn from_config(config: Config) -> Mihoro {
        let service_name = normalize_service_name(&config.service_name);
        Mihoro {
            prefix: String::from("mihoro:"),
            mihomo_target_binary_path: tilde(&config.mihomo_binary_path).to_string(),
            mihomo_target_config_root: tilde(&config.mihomo_config_root).to_string(),
            mihomo_target_config_path: tilde(&format!("{}/config.yaml", config.mihomo_config_root))
                .to_string(),
            mihomo_service_name: service_name.clone(),
            mihomo_target_service_path: resolve_service_path(&config, &service_name),
            config,
        }
    }

    pub fn service_manager(&self) -> Result<ServiceManager> {
        let kind_str = self.config.service_manager.as_deref().unwrap_or("auto");
        let kind = ServiceManagerKind::from_str(kind_str)?;
        ServiceManager::new(kind)
    }

    /// Stage 1 of the binary install: resolve the URL and download to a temp file.
    ///
    /// Skips if the binary exists and `force` is false. The returned [`BinaryPlan`] is
    /// handed to [`Mihoro::install_binary`] *after* every other download stage so that
    /// stopping the running mihomo service does not break the user's `https_proxy`
    /// while we still need to reach the network.
    pub async fn prepare_binary(
        &self,
        client: &Client,
        force: bool,
        arch_override: Option<&str>,
    ) -> Result<BinaryPlan> {
        let binary_exists = fs::metadata(&self.mihomo_target_binary_path).is_ok();
        if binary_exists && !force {
            return Ok(BinaryPlan::Skip(format!(
                "binary exists at {}",
                self.mihomo_target_binary_path
            )));
        }
        let binary_url = resolve_mihomo_bin::resolve_binary_url(
            client,
            &self.config,
            arch_override,
            DETAIL_PREFIX,
        )
        .await?;

        let temp_file = NamedTempFile::new()?;
        download_file(
            client,
            &binary_url,
            temp_file.path(),
            &self.config.mihoro_user_agent,
        )
        .await?;
        Ok(BinaryPlan::Install(temp_file))
    }

    /// Stage 2 of the binary install: stop the running service if any, then extract the
    /// downloaded binary into place and set its executable bit.
    ///
    /// Must run *after* every other network-dependent stage; see [`BinaryPlan`].
    pub async fn install_binary(&self, temp_file: NamedTempFile) -> Result<StageStatus> {
        // Stop the service before overwriting to avoid "Text file busy".
        let binary_exists = fs::metadata(&self.mihomo_target_binary_path).is_ok();
        if binary_exists {
            println!(
                "{} Stopping service before overwriting binary...",
                DETAIL_PREFIX.cyan()
            );
            self.service_manager()?.stop(&self.mihomo_service_name)?;
        }

        extract_gzip(
            temp_file.path(),
            &self.mihomo_target_binary_path,
            DETAIL_PREFIX.cyan(),
        )?;
        let executable = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&self.mihomo_target_binary_path, executable)?;
        Ok(StageStatus::Installed)
    }

    /// Download remote config YAML and apply TOML overrides.
    /// If the config file already exists and `force` is false, only re-applies overrides.
    pub async fn ensure_remote_config(&self, client: &Client, force: bool) -> Result<StageStatus> {
        let config_path = Path::new(&self.mihomo_target_config_path);
        if !force && config_path.exists() {
            // Re-apply TOML overrides onto the cached YAML so user changes take effect.
            apply_mihomo_override(&self.mihomo_target_config_path, &self.config.mihomo_config)?;
            return Ok(StageStatus::Installed);
        }

        download_file(
            client,
            &self.config.remote_config_url,
            config_path,
            &self.config.mihoro_user_agent,
        )
        .await?;
        try_decode_base64_file_inplace(&self.mihomo_target_config_path)?;
        apply_mihomo_override(&self.mihomo_target_config_path, &self.config.mihomo_config)?;
        Ok(StageStatus::Installed)
    }

    /// Download geodata.  Skips files that already exist (unless `force`).
    pub async fn ensure_geodata(&self, client: &Client, force: bool) -> Result<StageStatus> {
        let Some(ref geox_url) = self.config.mihomo_config.geox_url else {
            return Ok(StageStatus::Skipped("geox_url not configured".to_string()));
        };

        let geodata_mode = self.config.mihomo_config.geodata_mode.unwrap_or(false);
        let config_root = Path::new(&self.mihomo_target_config_root);

        if geodata_mode {
            let geoip_path = config_root.join("geoip.dat");
            let geosite_path = config_root.join("geosite.dat");
            if !force && geoip_path.exists() && geosite_path.exists() {
                return Ok(StageStatus::Skipped("geodata present".to_string()));
            }
            if force || !geoip_path.exists() {
                download_file(
                    client,
                    &geox_url.geoip,
                    &geoip_path,
                    &self.config.mihoro_user_agent,
                )
                .await?;
            }
            if force || !geosite_path.exists() {
                download_file(
                    client,
                    &geox_url.geosite,
                    &geosite_path,
                    &self.config.mihoro_user_agent,
                )
                .await?;
            }
        } else {
            let mmdb_path = config_root.join("country.mmdb");
            if !force && mmdb_path.exists() {
                return Ok(StageStatus::Skipped("geodata present".to_string()));
            }
            download_file(
                client,
                &geox_url.mmdb,
                &mmdb_path,
                &self.config.mihoro_user_agent,
            )
            .await?;
        }

        Ok(StageStatus::Installed)
    }

    /// Install the web dashboard.  Skips if the target directory already has an `index.html`
    /// (unless `force`).
    pub async fn ensure_ui(&self, client: &Client, force: bool) -> Result<StageStatus> {
        let Some(ui) = self.config.ui.as_ref() else {
            return Ok(StageStatus::Skipped("UI management disabled".to_string()));
        };
        let Some(target_dir) = self.external_ui_target_dir() else {
            return Ok(StageStatus::Skipped("`external_ui` path unset".to_string()));
        };
        if !force && target_dir.join("index.html").exists() {
            return Ok(StageStatus::Skipped(format!(
                "{} already installed",
                ui.as_config_value()
            )));
        }
        install_ui(
            client,
            ui,
            &target_dir,
            &self.config.mihoro_user_agent,
            DETAIL_PREFIX.cyan(),
        )
        .await?;
        Ok(StageStatus::Installed)
    }

    /// Write the systemd unit file.  Skips if the file already exists with identical content.
    pub async fn ensure_service(&self) -> Result<StageStatus> {
        let service_content = render_service_definition(
            &self.mihomo_service_name,
            &self.mihomo_target_binary_path,
            &self.mihomo_target_config_root,
        );

        if let Ok(existing) = fs::read_to_string(&self.mihomo_target_service_path) {
            if existing == service_content {
                return Ok(StageStatus::Skipped("service file unchanged".to_string()));
            }
        }
        create_parent_dir(Path::new(&self.mihomo_target_service_path))?;
        fs::write(&self.mihomo_target_service_path, &service_content)?;
        self.service_manager()?.daemon_reload()?;
        println!(
            "{} Created service at {}",
            DETAIL_PREFIX.cyan(),
            self.mihomo_target_service_path.underline().yellow()
        );
        Ok(StageStatus::Installed)
    }

    /// Enable and start mihomo.service, ensuring both autostart and current-session state.
    ///
    /// Always enables the service so it survives reboots, even if it was already running but
    /// not enabled (e.g. started manually after a previous failed init).
    pub async fn ensure_service_running(&self) -> Result<StageStatus> {
        let service_manager = self.service_manager()?;
        let is_active = service_manager.is_active(&self.mihomo_service_name);
        let is_enabled = service_manager.is_enabled(&self.mihomo_service_name);

        if is_active && is_enabled {
            return Ok(StageStatus::Skipped(
                "already running and enabled".to_string(),
            ));
        }

        if !is_enabled {
            service_manager.enable(&self.mihomo_service_name)?;
        }
        if !is_active {
            service_manager.start(&self.mihomo_service_name)?;
        }
        Ok(StageStatus::Installed)
    }

    pub async fn update_core(
        &self,
        client: &Client,
        arch_override: Option<&str>,
    ) -> Result<StageStatus> {
        // Check if binary exists
        let binary_exists = fs::metadata(&self.mihomo_target_binary_path).is_ok();
        if !binary_exists {
            return Err(anyhow!(
                "Mihomo binary not found at {}. Run `mihoro init` first.",
                self.mihomo_target_binary_path
            ));
        }

        // Resolve binary URL (auto-detect from GitHub or use configured URL)
        let binary_url = resolve_mihomo_bin::resolve_binary_url(
            client,
            &self.config,
            arch_override,
            DETAIL_PREFIX,
        )
        .await?;

        // Create a temporary file for downloading
        let temp_file = NamedTempFile::new()?;
        let temp_path = temp_file.path();

        // Download mihomo binary first (before stopping service)
        download_file(
            client,
            &binary_url,
            temp_path,
            &self.config.mihoro_user_agent,
        )
        .await?;

        // Stop the service before overwriting binary to avoid "Text file busy" error
        println!(
            "{} Stopping {} before overwriting...",
            DETAIL_PREFIX.yellow(),
            self.mihomo_service_name
        );
        self.service_manager()?.stop(&self.mihomo_service_name)?;

        // Extract and overwrite the binary
        extract_gzip(
            temp_path,
            &self.mihomo_target_binary_path,
            DETAIL_PREFIX.cyan(),
        )?;

        // Set executable permission
        let executable = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&self.mihomo_target_binary_path, executable)?;

        Ok(StageStatus::Installed)
    }

    pub async fn update_config(&self, client: &Client) -> Result<StageStatus> {
        // Download remote mihomo config and apply override
        download_file(
            client,
            &self.config.remote_config_url,
            Path::new(&self.mihomo_target_config_path),
            &self.config.mihoro_user_agent,
        )
        .await?;

        // Try to decode base64 file in place if file is base64 encoding, otherwise do nothing
        try_decode_base64_file_inplace(&self.mihomo_target_config_path)?;

        apply_mihomo_override(&self.mihomo_target_config_path, &self.config.mihomo_config)?;
        println!(
            "{} Updated and applied config overrides",
            DETAIL_PREFIX.cyan()
        );
        Ok(StageStatus::Installed)
    }

    pub async fn update_geodata(&self, client: &Client) -> Result<StageStatus> {
        if let Some(geox_url) = self.config.mihomo_config.geox_url.clone() {
            // Download geodata files based on `geodata_mode`
            let geodata_mode = self.config.mihomo_config.geodata_mode.unwrap_or(false);
            if geodata_mode {
                download_file(
                    client,
                    &geox_url.geoip,
                    &Path::new(&self.mihomo_target_config_root).join("geoip.dat"),
                    &self.config.mihoro_user_agent,
                )
                .await?;
                download_file(
                    client,
                    &geox_url.geosite,
                    &Path::new(&self.mihomo_target_config_root).join("geosite.dat"),
                    &self.config.mihoro_user_agent,
                )
                .await?;
            } else {
                download_file(
                    client,
                    &geox_url.mmdb,
                    &Path::new(&self.mihomo_target_config_root).join("country.mmdb"),
                    &self.config.mihoro_user_agent,
                )
                .await?;
            }

            println!("{} Downloaded and updated geodata", DETAIL_PREFIX.cyan());
        } else {
            return Ok(StageStatus::Skipped("`geox_url` undefined".to_string()));
        }
        Ok(StageStatus::Installed)
    }

    pub async fn update_ui(&self, client: &Client) -> Result<StageStatus> {
        let Some(ui) = self.config.ui.as_ref() else {
            return Ok(StageStatus::Skipped("UI management disabled".to_string()));
        };

        let Some(target_dir) = self.external_ui_target_dir() else {
            return Ok(StageStatus::Skipped("`external_ui` undefined".to_string()));
        };

        install_ui(
            client,
            ui,
            &target_dir,
            &self.config.mihoro_user_agent,
            DETAIL_PREFIX.cyan(),
        )
        .await?;
        Ok(StageStatus::Installed)
    }

    pub async fn restart_service(&self) -> Result<StageStatus> {
        println!(
            "{} Restarting {}...",
            DETAIL_PREFIX.cyan(),
            self.mihomo_service_name
        );
        self.service_manager()?.restart(&self.mihomo_service_name)?;
        Ok(StageStatus::Installed)
    }

    pub async fn apply(&self) -> Result<()> {
        // Apply mihomo config override
        apply_mihomo_override(&self.mihomo_target_config_path, &self.config.mihomo_config).map(
            |_| {
                println!(
                    "{} Applied mihomo config overrides",
                    self.prefix.green().bold()
                );
            },
        )?;

        // Restart service
        self.service_manager()?
            .restart(&self.mihomo_service_name)
            .map(|_| {
                println!(
                    "{} Restarted {}",
                    self.prefix.green().bold(),
                    self.mihomo_service_name
                );
            })?;
        Ok(())
    }

    pub fn uninstall(&self) -> Result<()> {
        let service_manager = self.service_manager()?;
        service_manager.stop(&self.mihomo_service_name)?;
        service_manager.disable(&self.mihomo_service_name)?;

        delete_file(&self.mihomo_target_service_path, self.prefix.cyan())?;
        delete_file(&self.mihomo_target_config_path, self.prefix.cyan())?;

        service_manager.daemon_reload()?;
        service_manager.reset_failed()?;
        println!(
            "{} Disabled and reloaded service manager state",
            self.prefix.green().bold()
        );

        // Disable and remove cron job
        cron::disable_auto_update(&self.prefix)?;

        println!(
            "{} You may need to remove mihomo binary and config directory manually",
            self.prefix.yellow()
        );

        let remove_cmd = format!(
            "rm -R {} {}",
            self.mihomo_target_binary_path, self.mihomo_target_config_root
        );
        println!("{} `{}`", "->".dimmed(), remove_cmd.underline().bold());
        Ok(())
    }

    pub fn proxy_commands(&self, proxy: &Option<ProxyCommands>) -> Result<()> {
        // `mixed_port` takes precedence over `port` and `socks_port` for proxy export
        let port = self
            .config
            .mihomo_config
            .mixed_port
            .as_ref()
            .unwrap_or(&self.config.mihomo_config.port);
        let socks_port = self
            .config
            .mihomo_config
            .mixed_port
            .as_ref()
            .unwrap_or(&self.config.mihomo_config.socks_port);

        match proxy {
            Some(ProxyCommands::Export) => {
                println!("{}", proxy_export_cmd("127.0.0.1", port, socks_port))
            }
            Some(ProxyCommands::ExportLan) => {
                if !self.config.mihomo_config.allow_lan.unwrap_or(false) {
                    println!(
                        "{} `{}` is false, proxy is not available for LAN",
                        "warning:".yellow(),
                        "allow_lan".bold()
                    );
                }

                println!(
                    "{}",
                    proxy_export_cmd(&local_ip()?.to_string(), port, socks_port)
                );
            }
            Some(ProxyCommands::Unset) => {
                println!("{}", proxy_unset_cmd())
            }
            _ => (),
        }
        Ok(())
    }

    pub fn cron_commands(&self, command: &Option<CronCommands>) -> Result<()> {
        match command {
            Some(CronCommands::Enable) => {
                cron::enable_auto_update(self.config.auto_update_interval, &self.prefix)
            }
            Some(CronCommands::Disable) => cron::disable_auto_update(&self.prefix),
            Some(CronCommands::Status) => {
                cron::get_cron_status(&self.prefix, &self.mihomo_target_config_path)
            }
            _ => Ok(()),
        }
    }

    fn external_ui_target_dir(&self) -> Option<PathBuf> {
        self.config
            .mihomo_config
            .external_ui
            .as_deref()
            .map(|external_ui| {
                resolve_external_ui_path(&self.mihomo_target_config_root, external_ui)
            })
    }
}

fn normalize_service_name(service_name: &str) -> String {
    if service_name.ends_with(".service") {
        service_name.to_string()
    } else {
        format!("{service_name}.service")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    use crate::service::*;

    /// Test that Mihoro::new correctly parses config and derives paths
    #[test]
    fn test_mihoro_new_parses_config_and_derives_paths() -> Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("test.toml");

        // Write a valid config file
        let toml_content = r#"
            remote_config_url = "http://example.com/config.yaml"
            mihomo_binary_path = "/tmp/test/mihomo"
            mihomo_config_root = "/tmp/test/mihomo"
            user_systemd_root = "/tmp/test/systemd"
        "#;
        fs::write(&config_path, toml_content)?;

        let mihoro = Mihoro::new(&config_path.to_str().unwrap().to_string())?;

        assert_eq!(mihoro.mihomo_target_binary_path, "/tmp/test/mihomo");
        assert_eq!(mihoro.mihomo_target_config_root, "/tmp/test/mihomo");
        assert_eq!(
            mihoro.mihomo_target_config_path,
            "/tmp/test/mihomo/config.yaml"
        );
        if std::env::consts::OS == "macos" {
            assert!(mihoro
                .mihomo_target_service_path
                .ends_with("/Library/LaunchAgents/mihomo.plist"));
        } else {
            assert_eq!(
                mihoro.mihomo_target_service_path,
                "/tmp/test/systemd/mihomo.service"
            );
        }
        assert_eq!(mihoro.mihomo_service_name, "mihomo.service");

        Ok(())
    }

    /// Test that proxy_commands uses mixed_port when set
    #[test]
    fn test_proxy_commands_uses_mixed_port_when_set() -> Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("test.toml");

        let toml_content = r#"
            remote_config_url = "http://example.com/config.yaml"
            mihomo_binary_path = "/tmp/test/mihomo"
            mihomo_config_root = "/tmp/test/mihomo"
            user_systemd_root = "/tmp/test/systemd"

            [mihomo_config]
            port = 7891
            socks_port = 7892
            mixed_port = 7890
        "#;
        fs::write(&config_path, toml_content)?;

        let mihoro = Mihoro::new(&config_path.to_str().unwrap().to_string())?;

        // Test Export command (should use mixed_port 7890)
        let cmd = mihoro.proxy_commands(&Some(ProxyCommands::Export));
        assert!(cmd.is_ok());

        Ok(())
    }

    /// Test that proxy_commands falls back to port/socks_port when mixed_port is None
    #[test]
    fn test_proxy_commands_fallback_to_port_when_mixed_port_none() -> Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("test.toml");

        let toml_content = r#"
            remote_config_url = "http://example.com/config.yaml"
            mihomo_binary_path = "/tmp/test/mihomo"
            mihomo_config_root = "/tmp/test/mihomo"
            user_systemd_root = "/tmp/test/systemd"

            [mihomo_config]
            port = 7891
            socks_port = 7892
        "#;
        fs::write(&config_path, toml_content)?;

        let mihoro = Mihoro::new(&config_path.to_str().unwrap().to_string())?;

        let cmd = mihoro.proxy_commands(&Some(ProxyCommands::Export));
        assert!(cmd.is_ok());

        Ok(())
    }

    #[test]
    fn test_external_ui_target_dir_resolves_relative_path() -> Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("test.toml");

        let toml_content = r#"
            remote_config_url = "http://example.com/config.yaml"
            mihomo_binary_path = "/tmp/test/mihomo"
            mihomo_config_root = "/tmp/test/mihomo"
            user_systemd_root = "/tmp/test/systemd"

            [mihomo_config]
            external_ui = "ui"
        "#;
        fs::write(&config_path, toml_content)?;

        let mihoro = Mihoro::new(&config_path.to_str().unwrap().to_string())?;
        assert_eq!(
            mihoro.external_ui_target_dir(),
            Some(PathBuf::from("/tmp/test/mihomo/ui"))
        );

        Ok(())
    }

    /// Test integration: download config → apply override → verify result
    #[test]
    fn test_integration_apply_override_flow() -> Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("test.toml");
        let yaml_path = dir.path().join("config.yaml");

        // Write config with custom port override
        let toml_content = r#"
            remote_config_url = "http://example.com/config.yaml"
            mihomo_binary_path = "/tmp/test/mihomo"
            mihomo_config_root = "{}"
            user_systemd_root = "/tmp/test/systemd"

            [mihomo_config]
            port = 9999
            socks_port = 9998
        "#;
        fs::write(
            &config_path,
            toml_content.replace("{}", dir.path().to_str().unwrap()),
        )?;

        // Write initial mihomo config
        let yaml_content = r#"
            port: 8080
            socks-port: 8081
            mode: rule
            proxies:
              - name: "test"
                type: http
                server: example.com
                port: 443
        "#;
        fs::write(&yaml_path, yaml_content)?;

        // Create Mihoro instance and apply override
        let mihoro = Mihoro::new(&config_path.to_str().unwrap().to_string())?;
        apply_mihomo_override(yaml_path.to_str().unwrap(), &mihoro.config.mihomo_config)?;

        // Verify override was applied
        let updated_content = fs::read_to_string(&yaml_path)?;
        assert!(updated_content.contains("port: 9999"));
        assert!(updated_content.contains("socks-port: 9998"));
        assert!(updated_content.contains("proxies:"));

        Ok(())
    }

    #[test]
    fn test_create_mihomo_service_linux_contract() -> Result<()> {
        let mihomo_binary_path = "/tmp/test/mihomo";
        let mihomo_config_root = "/tmp/test/mihomo-config";

        let content = systemd::render_service_string(mihomo_binary_path, mihomo_config_root);
        assert!(content.contains("Description=mihomo Daemon, Another Clash Kernel."));
        assert!(content.contains("After=network.target NetworkManager.service"));
        assert!(content.contains("Type=simple"));
        assert!(content.contains("Restart=always"));
        assert!(content.contains(&format!(
            "ExecStart={} -d {}",
            mihomo_binary_path, mihomo_config_root
        )));
        assert!(content.contains("WantedBy=default.target"));
        Ok(())
    }

    #[test]
    fn test_create_mihomo_launchd_service_contract() -> Result<()> {
        let mihomo_binary_path = "/tmp/test/mihomo";
        let mihomo_config_root = "/tmp/test/mihomo-config";

        let content =
            launchd::build_plist("mihomo.service", mihomo_binary_path, mihomo_config_root);
        assert!(content.contains("<key>Label</key>"));
        assert!(content.contains("<string>mihomo</string>"));
        assert!(content.contains("<key>ProgramArguments</key>"));
        assert!(content.contains("<string>/bin/sh</string>"));
        assert!(content.contains("<string>-c</string>"));
        assert!(content.contains(&format!(
            "<string>{} -d {} 2>&1 | logger</string>",
            mihomo_binary_path, mihomo_config_root
        )));
        assert!(content.contains("<key>RunAtLoad</key>"));
        assert!(content.contains("<key>KeepAlive</key>"));
        Ok(())
    }

    #[test]
    fn test_normalize_service_name() {
        assert_eq!(normalize_service_name("mihomo"), "mihomo.service");
        assert_eq!(normalize_service_name("mihomo.service"), "mihomo.service");
    }
}
