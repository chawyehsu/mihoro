mod cmd;
mod config;
mod cron;
mod mihoro;
mod proxy;
mod resolve_mihomo_bin;
mod service;
#[cfg(feature = "self_update")]
mod upgrade;
mod utils;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::{
    generate,
    shells::{Bash, Fish, Zsh},
};
use colored::Colorize;
use reqwest::Client;
use std::io;

use cmd::{Args, ClapShell, Commands};
use mihoro::Mihoro;

#[tokio::main]
async fn main() {
    if let Err(err) = cli().await {
        eprintln!("{} {}", "error:".bright_red().bold(), err);
        std::process::exit(1);
    }
}

async fn cli() -> Result<()> {
    let args = Args::parse();
    let client = Client::new();
    let mihoro = Mihoro::new(&args.mihoro_config)?;

    match &args.command {
        Some(Commands::Setup { overwrite, arch }) => {
            mihoro.setup(client, *overwrite, arch.as_deref()).await?
        }
        Some(Commands::Update {
            config,
            core,
            geodata,
            all,
            arch,
        }) => {
            if *all {
                // Update config (without restarting yet)
                println!(
                    "{} Updating config...",
                    mihoro.prefix.magenta().bold().italic()
                );
                if let Err(e) = mihoro.update_config(&client, false).await {
                    eprintln!("{} Failed to update config: {}", mihoro.prefix.yellow(), e);
                }
                // Update geodata
                println!(
                    "{} Updating geodata...",
                    mihoro.prefix.magenta().bold().italic()
                );
                if let Err(e) = mihoro.update_geodata(&client).await {
                    eprintln!("{} Failed to update geodata: {}", mihoro.prefix.yellow(), e);
                }
                // Update core (without restarting yet)
                println!(
                    "{} Updating core...",
                    mihoro.prefix.magenta().bold().italic()
                );
                if let Err(e) = mihoro.update_core(&client, arch.as_deref(), false).await {
                    eprintln!("{} Failed to update core: {}", mihoro.prefix.yellow(), e);
                }
                // Restart service once at the end
                println!(
                    "{} Restarting {}...",
                    mihoro.prefix.green().bold().italic(),
                    mihoro.mihomo_service_name
                );
                mihoro
                    .service_manager()?
                    .restart(&mihoro.mihomo_service_name)?;
            } else if *core {
                mihoro.update_core(&client, arch.as_deref(), true).await?;
            } else if *geodata {
                mihoro.update_geodata(&client).await?;
            } else if *config || (!*core && !*geodata) {
                // Explicit --config or default (no flags)
                mihoro.update_config(&client, true).await?;
            }
        }
        Some(Commands::Apply) => mihoro.apply().await?,
        Some(Commands::Uninstall) => mihoro.uninstall()?,
        Some(Commands::Proxy { proxy }) => mihoro.proxy_commands(proxy)?,

        Some(Commands::Start) => mihoro
            .service_manager()?
            .start(&mihoro.mihomo_service_name)
            .map(|_| {
                println!(
                    "{} Started {}",
                    mihoro.prefix.green(),
                    mihoro.mihomo_service_name
                );
            })?,

        Some(Commands::Status) => {
            mihoro
                .service_manager()?
                .status(&mihoro.mihomo_service_name)?;
        }

        Some(Commands::Stop) => mihoro
            .service_manager()?
            .stop(&mihoro.mihomo_service_name)
            .map(|_| {
                println!(
                    "{} Stopped {}",
                    mihoro.prefix.green(),
                    mihoro.mihomo_service_name
                );
            })?,

        Some(Commands::Restart) => mihoro
            .service_manager()?
            .restart(&mihoro.mihomo_service_name)
            .map(|_| {
                println!(
                    "{} Restarted {}",
                    mihoro.prefix.green(),
                    mihoro.mihomo_service_name
                );
            })?,

        Some(Commands::Log) => {
            mihoro
                .service_manager()?
                .logs(&mihoro.mihomo_service_name)?;
        }

        Some(Commands::Completions { shell }) => match shell {
            Some(ClapShell::Bash) => {
                generate(Bash, &mut Args::command(), "mihoro", &mut io::stdout())
            }
            Some(ClapShell::Zsh) => {
                generate(Zsh, &mut Args::command(), "mihoro", &mut io::stdout())
            }
            Some(ClapShell::Fish) => {
                generate(Fish, &mut Args::command(), "mihoro", &mut io::stdout())
            }
            _ => (),
        },

        Some(Commands::Cron { cron }) => mihoro.cron_commands(cron)?,

        #[cfg(feature = "self_update")]
        Some(Commands::Upgrade { yes, check, target }) => {
            if *check {
                match upgrade::check_for_update().await? {
                    Some(version) => {
                        println!(
                            "{} New version available: {}",
                            mihoro.prefix.yellow(),
                            version.bold().green()
                        );
                        println!(
                            "{} Run {} to update",
                            "->".dimmed(),
                            "mihoro upgrade".bold().underline()
                        );
                    }
                    None => {
                        println!(
                            "{} You're running the latest version",
                            mihoro.prefix.green()
                        );
                    }
                }
            } else {
                upgrade::run_upgrade(*yes, target.clone()).await?;
            }
        }

        #[cfg(not(feature = "self_update"))]
        Some(Commands::Upgrade { .. }) => {
            anyhow::bail!(
                "mihoro was built without self_update support, please use your package manager to upgrade"
            );
        }

        None => (),
    }
    Ok(())
}
