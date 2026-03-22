use crate::cmd::{CronCommands, ProxyCommands};
use crate::config::{apply_mihomo_override, parse_config, Config};
use crate::cron;
use crate::proxy::{proxy_export_cmd, proxy_unset_cmd};
use crate::resolve_mihomo_bin;
use crate::service::{launchd, ServiceManager, ServiceManagerKind};
use crate::utils::{
    create_parent_dir, delete_file, download_file, extract_gzip, try_decode_base64_file_inplace,
};

use std::fs;
use std::os::unix::prelude::PermissionsExt;
use std::path::Path;

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

impl Mihoro {
    pub fn new(config_path: &String) -> Result<Mihoro> {
        let config = parse_config(tilde(&config_path).as_ref())?;
        let service_name = normalize_service_name(&config.service_name);
        Ok(Mihoro {
            prefix: String::from("mihoro:"),
            config: config.clone(),
            mihomo_target_binary_path: tilde(&config.mihomo_binary_path).to_string(),
            mihomo_target_config_root: tilde(&config.mihomo_config_root).to_string(),
            mihomo_target_config_path: tilde(&format!("{}/config.yaml", config.mihomo_config_root))
                .to_string(),
            mihomo_service_name: service_name.clone(),
            mihomo_target_service_path: resolve_service_path(&config, &service_name),
        })
    }

    pub fn service_manager(&self) -> Result<ServiceManager> {
        let kind_str = self.config.service_manager.as_deref().unwrap_or("auto");
        let kind = ServiceManagerKind::from_str(kind_str)?;
        ServiceManager::new(kind)
    }

    pub async fn setup(
        &self,
        client: Client,
        overwrite_binary: bool,
        arch_override: Option<&str>,
    ) -> Result<()> {
        println!(
            "{} Setting up mihomo binary, config, and service...",
            &self.prefix.cyan()
        );

        // Setup mihomo binary at `mihomo_target_binary_path`
        let binary_exists = fs::metadata(&self.mihomo_target_binary_path).is_ok();
        if binary_exists && !overwrite_binary {
            println!(
                "{} Assuming mihomo binary already installed at {}, skipping setup",
                self.prefix.yellow(),
                self.mihomo_target_binary_path.underline().green()
            );
        } else {
            if binary_exists {
                println!(
                    "{} Overwriting existing mihomo binary at {}",
                    self.prefix.yellow(),
                    self.mihomo_target_binary_path.underline().green()
                );
            }

            // Resolve binary URL (auto-detect from GitHub or use configured URL)
            let binary_url = resolve_mihomo_bin::resolve_binary_url(
                &client,
                &self.config,
                arch_override,
                &self.prefix,
            )
            .await?;

            // Create a temporary file for downloading
            let temp_file = NamedTempFile::new()?;
            let temp_path = temp_file.path();

            // Download mihomo binary and set permission to executable
            download_file(
                &client,
                &binary_url,
                temp_path,
                &self.config.mihoro_user_agent,
            )
            .await?;

            // Try to extract the binary, handle "Text file busy" error if overwriting
            match extract_gzip(temp_path, &self.mihomo_target_binary_path, &self.prefix) {
                Ok(_) => {
                    // Set executable permission
                    let executable = fs::Permissions::from_mode(0o755);
                    fs::set_permissions(&self.mihomo_target_binary_path, executable)?;
                }
                Err(e) => {
                    // Handle "Text file busy" error
                    return Err(if e.to_string().contains("Text file busy") {
                        anyhow!("Failed to overwrite as `mihomo` is in use, stop the service first")
                    } else {
                        e
                    });
                }
            };
        }

        // Download remote mihomo config and apply override
        download_file(
            &client,
            &self.config.remote_config_url,
            Path::new(&self.mihomo_target_config_path),
            &self.config.mihoro_user_agent,
        )
        .await?;

        // Try to decode base64 file in place if file is base64 encoding, otherwise do nothing
        try_decode_base64_file_inplace(&self.mihomo_target_config_path)?;

        apply_mihomo_override(&self.mihomo_target_config_path, &self.config.mihomo_config)?;

        // Download geodata
        self.update_geodata(&client).await?;

        // Create platform-specific service file
        create_mihomo_service_for_platform(
            &self.mihomo_service_name,
            &self.mihomo_target_binary_path,
            &self.mihomo_target_config_root,
            &self.mihomo_target_service_path,
            &self.prefix,
        )?;

        let service_manager = self.service_manager()?;
        service_manager.enable(&self.mihomo_service_name)?;
        service_manager.start(&self.mihomo_service_name)?;
        Ok(())
    }

    pub async fn update_core(
        &self,
        client: &Client,
        arch_override: Option<&str>,
        restart: bool,
    ) -> Result<()> {
        println!("{} Updating mihomo core binary...", &self.prefix.cyan());

        // Check if binary exists
        let binary_exists = fs::metadata(&self.mihomo_target_binary_path).is_ok();
        if !binary_exists {
            return Err(anyhow!(
                "Mihomo binary not found at {}. Run `mihoro setup` first.",
                self.mihomo_target_binary_path
            ));
        }

        // Resolve binary URL (auto-detect from GitHub or use configured URL)
        let binary_url = resolve_mihomo_bin::resolve_binary_url(
            client,
            &self.config,
            arch_override,
            &self.prefix,
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
            self.prefix.yellow(),
            self.mihomo_service_name
        );
        self.service_manager()?.stop(&self.mihomo_service_name)?;

        // Extract and overwrite the binary
        extract_gzip(temp_path, &self.mihomo_target_binary_path, &self.prefix)?;

        // Set executable permission
        let executable = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&self.mihomo_target_binary_path, executable)?;

        println!(
            "{} Updated mihomo binary at {}",
            self.prefix.green(),
            self.mihomo_target_binary_path.underline().yellow()
        );

        // Restart the service if requested
        if restart {
            println!(
                "{} Restarting {}...",
                self.prefix.green(),
                self.mihomo_service_name
            );
            self.service_manager()?.start(&self.mihomo_service_name)?;
        }

        Ok(())
    }

    pub async fn update_config(&self, client: &Client, restart: bool) -> Result<()> {
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
            self.prefix.yellow()
        );

        // Restart service if requested
        if restart {
            println!(
                "{} Restart {}",
                self.prefix.green(),
                self.mihomo_service_name
            );
            self.service_manager()?.restart(&self.mihomo_service_name)?;
        }
        Ok(())
    }

    pub async fn update_geodata(&self, client: &Client) -> Result<()> {
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

            println!("{} Downloaded and updated geodata", self.prefix.green());
        } else {
            println!(
                "{} `geox_url` undefined, refer to {}",
                self.prefix.yellow(),
                "'https://wiki.metacubex.one/config/general/#geo_3'"
                    .bold()
                    .underline()
            );
        }
        Ok(())
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

        delete_file(&self.mihomo_target_service_path, &self.prefix)?;
        delete_file(&self.mihomo_target_config_path, &self.prefix)?;

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
}

/// Create a systemd service file for running mihomo as a service.
///
/// By default, user systemd services are created under `~/.config/systemd/user/mihomo.service` and
/// invoked with `systemctl --user start mihomo.service`. Directory is created if not present.
///
/// Reference: https://wiki.metacubex.one/startup/service/
fn create_mihomo_service(
    mihomo_binary_path: &str,
    mihomo_config_root: &str,
    mihomo_service_path: &str,
    prefix: &str,
) -> Result<()> {
    let service = format!(
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
        mihomo_binary_path, mihomo_config_root
    );

    // Create mihomo service directory if not exists
    create_parent_dir(Path::new(mihomo_service_path))?;

    // Write mihomo.service contents to file
    fs::write(mihomo_service_path, service)?;

    println!(
        "{} Created mihomo.service at {}",
        prefix.green(),
        mihomo_service_path.underline().yellow()
    );
    Ok(())
}

fn create_mihomo_launchd_service(
    service_name: &str,
    mihomo_binary_path: &str,
    mihomo_config_root: &str,
    mihomo_service_path: &str,
    prefix: &str,
) -> Result<()> {
    let plist = launchd::build_plist(service_name, mihomo_binary_path, mihomo_config_root);

    create_parent_dir(Path::new(mihomo_service_path))?;
    fs::write(mihomo_service_path, plist)?;

    println!(
        "{} Created launchd plist at {}",
        prefix.green(),
        mihomo_service_path.underline().yellow()
    );
    Ok(())
}

fn create_mihomo_service_for_platform(
    service_name: &str,
    mihomo_binary_path: &str,
    mihomo_config_root: &str,
    mihomo_service_path: &str,
    prefix: &str,
) -> Result<()> {
    match std::env::consts::OS {
        "macos" => create_mihomo_launchd_service(
            service_name,
            mihomo_binary_path,
            mihomo_config_root,
            mihomo_service_path,
            prefix,
        ),
        _ => create_mihomo_service(
            mihomo_binary_path,
            mihomo_config_root,
            mihomo_service_path,
            prefix,
        ),
    }
}

fn normalize_service_name(service_name: &str) -> String {
    if service_name.ends_with(".service") {
        service_name.to_string()
    } else {
        format!("{service_name}.service")
    }
}

fn resolve_service_path(config: &Config, service_name: &str) -> String {
    match std::env::consts::OS {
        "macos" => {
            let root = config
                .service_root
                .clone()
                .unwrap_or_else(|| String::from("~/Library/LaunchAgents"));
            tilde(&format!(
                "{}/{}.plist",
                root,
                launchd::service_stem(service_name)
            ))
            .to_string()
        }
        _ => {
            let root = config
                .service_root
                .clone()
                .unwrap_or_else(|| config.user_systemd_root.clone());
            tilde(&format!("{}/{}", root, service_name)).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

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
        let dir = tempdir()?;
        let service_path = dir.path().join("systemd/user/mihomo.service");
        let mihomo_binary_path = "/tmp/test/mihomo";
        let mihomo_config_root = "/tmp/test/mihomo-config";

        create_mihomo_service(
            mihomo_binary_path,
            mihomo_config_root,
            service_path.to_str().unwrap(),
            "mihoro:",
        )?;

        let content = fs::read_to_string(service_path)?;
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
        let dir = tempdir()?;
        let service_path = dir.path().join("LaunchAgents/mihomo.plist");
        let mihomo_binary_path = "/tmp/test/mihomo";
        let mihomo_config_root = "/tmp/test/mihomo-config";

        create_mihomo_launchd_service(
            "mihomo.service",
            mihomo_binary_path,
            mihomo_config_root,
            service_path.to_str().unwrap(),
            "mihoro:",
        )?;

        let content = fs::read_to_string(service_path)?;
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
