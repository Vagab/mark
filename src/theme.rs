use crate::config::Config;
use anyhow::{Context, Result};
use ratatui::style::Color;
use std::path::PathBuf;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect_assets::assets::HighlightingAssets;

pub struct ThemeManager {
    assets: HighlightingAssets,
    extra: ThemeSet,
    theme_names: Vec<String>,
    syntax_set: SyntaxSet,
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
        let assets = HighlightingAssets::from_binary();
        let syntax_set = assets
            .get_syntax_set()
            .context("Failed to load syntect syntax set")?
            .clone();
        let mut extra = ThemeSet::new();

        if let Some(dir) = resolve_bat_theme_dir(config) {
            if dir.exists() {
                extra
                    .add_from_folder(&dir)
                    .with_context(|| format!("Failed to load themes from {}", dir.display()))?;
            }
        }

        let mut theme_names: Vec<String> =
            assets.themes().map(|name| name.to_string()).collect();
        theme_names.extend(extra.themes.keys().cloned());
        theme_names.sort();
        theme_names.dedup();

        Ok(Self {
            assets,
            extra,
            theme_names,
            syntax_set,
        })
    }

    pub fn theme_names(&self) -> &[String] {
        &self.theme_names
    }

    pub fn get(&self, name: &str) -> &syntect::highlighting::Theme {
        if let Some(theme) = self.extra.themes.get(name) {
            return theme;
        }
        self.assets.get_theme(name)
    }

    pub fn ui_palette(&self, name: &str) -> UiPalette {
        let theme = self.get(name);
        palette_from_theme(theme)
    }

    pub fn fallback_name(&self) -> &str {
        let default_name = HighlightingAssets::default_theme();
        if self.theme_names.iter().any(|name| name == default_name) {
            return default_name;
        }
        self.theme_names
            .first()
            .map(|s| s.as_str())
            .unwrap_or(default_name)
    }

    pub fn syntax_set(&self) -> &SyntaxSet {
        &self.syntax_set
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
