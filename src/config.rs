use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: String,
    pub show_outline: bool,
    pub outline_width: u16,
    pub wrap: bool,
    pub search_case_sensitive: bool,
    pub bat_theme_dir: Option<PathBuf>,
    pub tab_width: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "base16-ocean.dark".to_string(),
            show_outline: true,
            outline_width: 28,
            wrap: true,
            search_case_sensitive: false,
            bat_theme_dir: dirs::config_dir().map(|dir| dir.join("bat").join("themes")),
            tab_width: 4,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct PartialConfig {
    theme: Option<String>,
    show_outline: Option<bool>,
    outline_width: Option<u16>,
    wrap: Option<bool>,
    search_case_sensitive: Option<bool>,
    bat_theme_dir: Option<PathBuf>,
    tab_width: Option<usize>,
}

impl PartialConfig {
    fn apply_defaults(self) -> (Config, bool) {
        let defaults = Config::default();
        let mut changed = false;

        let theme = match self.theme {
            Some(v) => v,
            None => {
                changed = true;
                defaults.theme
            }
        };
        let show_outline = match self.show_outline {
            Some(v) => v,
            None => {
                changed = true;
                defaults.show_outline
            }
        };
        let outline_width = match self.outline_width {
            Some(v) => v,
            None => {
                changed = true;
                defaults.outline_width
            }
        };
        let wrap = match self.wrap {
            Some(v) => v,
            None => {
                changed = true;
                defaults.wrap
            }
        };
        let search_case_sensitive = match self.search_case_sensitive {
            Some(v) => v,
            None => {
                changed = true;
                defaults.search_case_sensitive
            }
        };
        let bat_theme_dir = match self.bat_theme_dir {
            Some(v) => Some(v),
            None => {
                changed = true;
                defaults.bat_theme_dir
            }
        };
        let tab_width = match self.tab_width {
            Some(v) => v,
            None => {
                changed = true;
                defaults.tab_width
            }
        };

        (
            Config {
                theme,
                show_outline,
                outline_width,
                wrap,
                search_case_sensitive,
                bat_theme_dir,
                tab_width,
            },
            changed,
        )
    }
}

pub fn config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("Could not determine config directory")?;
    Ok(base.join("mark").join("config.toml"))
}

pub fn ensure_config_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    Ok(())
}

pub fn load_config() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        let cfg = Config::default();
        write_config(&cfg)?;
        return Ok(cfg);
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let partial: PartialConfig = toml::from_str(&raw)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let (cfg, changed) = partial.apply_defaults();
    if changed {
        write_config(&cfg)?;
    }
    Ok(cfg)
}

pub fn write_config(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    ensure_config_dir(&path)?;
    let text = toml::to_string_pretty(cfg).context("Failed to serialize config")?;
    fs::write(&path, text).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

pub fn open_config_in_editor() -> Result<()> {
    let path = config_path()?;
    if !path.exists() {
        let cfg = Config::default();
        write_config(&cfg)?;
    }

    let editor = env::var("EDITOR").unwrap_or_else(|_| "nvim".to_string());
    let mut parts = match shell_words::split(&editor) {
        Ok(p) if !p.is_empty() => p,
        _ => vec![editor],
    };
    let cmd = parts.remove(0);
    let status = Command::new(cmd)
        .args(parts)
        .arg(&path)
        .status()
        .with_context(|| format!("Failed to launch editor for {}", path.display()))?;
    if !status.success() {
        anyhow::bail!("Editor exited with status {}", status);
    }
    Ok(())
}
