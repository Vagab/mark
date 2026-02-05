use crate::config::Config;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn install_bat_themes(cfg: &Config) -> Result<(PathBuf, usize)> {
    let target_dir = cfg
        .bat_theme_dir
        .clone()
        .or_else(default_bat_theme_dir)
        .context("No theme directory configured")?;
    fs::create_dir_all(&target_dir)
        .with_context(|| format!("Failed to create {}", target_dir.display()))?;

    let temp_dir = temp_path("mark-bat-themes");
    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)
            .with_context(|| format!("Failed to clean {}", temp_dir.display()))?;
    }

    let status = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "https://github.com/sharkdp/bat",
            temp_dir.to_string_lossy().as_ref(),
        ])
        .status()
        .context("Failed to run git (is it installed?)")?;
    if !status.success() {
        bail!("git clone failed with status {}", status);
    }

    let status = Command::new("git")
        .args([
            "-C",
            temp_dir.to_string_lossy().as_ref(),
            "submodule",
            "update",
            "--init",
            "--depth",
            "1",
            "--recursive",
            "assets/themes",
        ])
        .status()
        .context("Failed to init bat theme submodules")?;
    if !status.success() {
        bail!("git submodule update failed with status {}", status);
    }

    let theme_src = temp_dir.join("assets").join("themes");
    if !theme_src.exists() {
        bail!(
            "Expected theme folder not found in {}",
            theme_src.display()
        );
    }

    let mut copied = 0usize;
    let mut seen = std::collections::HashSet::new();
    copy_theme_files(&theme_src, &theme_src, &target_dir, &mut seen, &mut copied)?;

    let _ = fs::remove_dir_all(&temp_dir);

    Ok((target_dir, copied))
}

fn default_bat_theme_dir() -> Option<PathBuf> {
    let base = dirs::config_dir()?;
    Some(base.join("bat").join("themes"))
}

fn temp_path(prefix: &str) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    std::env::temp_dir().join(format!("{prefix}-{now}"))
}

fn copy_theme_files(
    root: &Path,
    current: &Path,
    target_dir: &Path,
    seen: &mut std::collections::HashSet<String>,
    copied: &mut usize,
) -> Result<()> {
    for entry in fs::read_dir(current).with_context(|| format!("Failed to read {}", current.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            copy_theme_files(root, &path, target_dir, seen, copied)?;
            continue;
        }
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if ext != "tmTheme" && ext != "sublime-color-scheme" {
            continue;
        }

        let rel = path.strip_prefix(root).unwrap_or(&path);
        let rel_name = rel
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .collect::<Vec<_>>()
            .join("_");
        let file_name = if rel_name.is_empty() {
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("theme.tmTheme")
                .to_string()
        } else {
            rel_name
        };

        let mut dest_name = file_name;
        if seen.contains(&dest_name) {
            let mut i = 2;
            loop {
                let candidate = format!("{dest_name}-{i}");
                if !seen.contains(&candidate) {
                    dest_name = candidate;
                    break;
                }
                i += 1;
            }
        }
        seen.insert(dest_name.clone());
        let dest = target_dir.join(dest_name);
        fs::copy(&path, &dest).with_context(|| format!("Failed to copy {}", dest.display()))?;
        *copied += 1;
    }
    Ok(())
}
