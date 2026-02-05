mod app;
mod config;
mod markdown;
mod theme;
mod themes;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "mark", version, about = "Markdown reader for the terminal")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Markdown file to open
    file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Open the config file in $EDITOR (default: nvim)
    Config,
    /// Manage themes
    Themes {
        #[command(subcommand)]
        command: ThemeCommands,
    },
}

#[derive(Subcommand)]
enum ThemeCommands {
    /// Install themes (default: bat)
    Install {
        /// Theme source (only "bat" supported for now)
        source: Option<String>,
    },
    /// List available themes
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(command) = cli.command {
        match command {
            Commands::Config => return config::open_config_in_editor(),
            Commands::Themes { command } => match command {
                ThemeCommands::Install { source } => {
                    let source = source.unwrap_or_else(|| "bat".to_string());
                    if source != "bat" {
                        return Err(anyhow::anyhow!(
                            "Unknown theme source: {source}. Try `mark themes install bat`."
                        ));
                    }
                    let cfg = config::load_config()?;
                    let (dir, count) = themes::install_bat_themes(&cfg)?;
                    println!("Installed {count} themes into {}", dir.display());
                    return Ok(());
                }
                ThemeCommands::List => {
                    let cfg = config::load_config()?;
                    let manager = theme::ThemeManager::load(&cfg)?;
                    for name in manager.theme_names() {
                        println!("{name}");
                    }
                    return Ok(());
                }
            },
        }
    }

    let file = cli
        .file
        .ok_or_else(|| anyhow::anyhow!("No file provided. Try `mark <file.md>`."))?;

    let cfg = config::load_config()?;
    app::run_app(file, cfg)
}
