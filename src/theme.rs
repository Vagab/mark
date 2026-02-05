use crate::config::Config;
use anyhow::{Context, Result};
use ratatui::style::Color;
use std::path::PathBuf;
use syntect::highlighting::{Theme, ThemeSet};

pub struct ThemeManager {
    theme_set: ThemeSet,
    theme_names: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct UiPalette {
    pub base_fg: Color,
    pub base_bg: Option<Color>,
    pub accent: Color,
    pub muted: Color,
    pub code_bg: Option<Color>,
    pub border: Color,
}

impl ThemeManager {
    pub fn load(config: &Config) -> Result<Self> {
        let mut theme_set = ThemeSet::load_defaults();

        if let Some(dir) = resolve_bat_theme_dir(config) {
            if dir.exists() {
                let extra = ThemeSet::load_from_folder(&dir)
                    .with_context(|| format!("Failed to load themes from {}", dir.display()))?;
                theme_set.themes.extend(extra.themes);
            }
        }

        let mut theme_names: Vec<String> = theme_set.themes.keys().cloned().collect();
        theme_names.sort();

        Ok(Self {
            theme_set,
            theme_names,
        })
    }

    pub fn theme_names(&self) -> &[String] {
        &self.theme_names
    }

    pub fn get(&self, name: &str) -> &syntect::highlighting::Theme {
        self.theme_set
            .themes
            .get(name)
            .unwrap_or_else(|| self.fallback_theme())
    }

    pub fn ui_palette(&self, name: &str) -> UiPalette {
        let theme = self.get(name);
        palette_from_theme(theme)
    }

    pub fn fallback_name(&self) -> &str {
        self.theme_names
            .first()
            .map(|s| s.as_str())
            .unwrap_or("base16-ocean.dark")
    }

    fn fallback_theme(&self) -> &syntect::highlighting::Theme {
        let name = self.fallback_name();
        self.theme_set
            .themes
            .get(name)
            .unwrap_or_else(|| self.theme_set.themes.values().next().expect("themeset empty"))
    }
}

fn resolve_bat_theme_dir(config: &Config) -> Option<PathBuf> {
    if let Some(dir) = &config.bat_theme_dir {
        return Some(dir.clone());
    }
    default_bat_theme_dir()
}

fn default_bat_theme_dir() -> Option<PathBuf> {
    let base = dirs::config_dir()?;
    Some(base.join("bat").join("themes"))
}

fn palette_from_theme(theme: &Theme) -> UiPalette {
    let settings = &theme.settings;
    let base_fg = settings
        .foreground
        .map(to_ratatui)
        .unwrap_or(Color::Gray);
    let base_bg = settings.background.map(to_ratatui);
    let accent = settings
        .selection_foreground
        .or(settings.caret)
        .or(settings.foreground)
        .map(to_ratatui)
        .unwrap_or(Color::Cyan);
    let muted = settings
        .gutter_foreground
        .or(settings.foreground)
        .map(to_ratatui)
        .unwrap_or(Color::DarkGray);
    let code_bg = settings
        .line_highlight
        .or(settings.selection)
        .or(settings.background)
        .map(to_ratatui);

    UiPalette {
        base_fg,
        base_bg,
        accent,
        muted,
        code_bg,
        border: muted,
    }
}

fn to_ratatui(color: syntect::highlighting::Color) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}
