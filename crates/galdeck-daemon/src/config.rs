//! TOML profile format. See `config/galdeck.example.toml` in the repo.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Panel brightness 0-100.
    #[serde(default = "default_brightness")]
    pub brightness: u8,
    /// TTF/OTF font for key labels and the info screen; when unset, a list
    /// of common system font paths is tried.
    #[serde(default)]
    pub font: Option<PathBuf>,
    /// Default info-screen text (pages can override with `lcd_text`).
    #[serde(default)]
    pub lcd_text: Option<String>,
    #[serde(default)]
    pub pages: Vec<Page>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub name: String,
    #[serde(default)]
    pub lcd_text: Option<String>,
    #[serde(default)]
    pub keys: Vec<KeyConfig>,
    #[serde(default)]
    pub encoders: Vec<EncoderConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    /// Key position 0-11, row-major from the top-left of the key grid.
    pub key: u8,
    #[serde(default)]
    pub label: Option<String>,
    /// Background color, `#rrggbb`.
    #[serde(default)]
    pub color: Option<String>,
    /// Icon image path (png/jpeg), scaled to fit.
    #[serde(default)]
    pub image: Option<PathBuf>,
    /// Shell command to run on press.
    #[serde(default)]
    pub exec: Option<String>,
    /// Page to switch to on press (instead of / after exec).
    #[serde(default)]
    pub page: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncoderConfig {
    /// 0 = left, 1 = right.
    pub encoder: u8,
    #[serde(default)]
    pub press: Option<String>,
    /// Shell command per clockwise detent.
    #[serde(default)]
    pub cw: Option<String>,
    #[serde(default)]
    pub ccw: Option<String>,
    /// Ring LED color, `#rrggbb`.
    #[serde(default)]
    pub ring: Option<String>,
}

fn default_brightness() -> u8 {
    60
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Built-in profile used when no config file exists yet.
    pub fn fallback() -> Self {
        Config {
            brightness: default_brightness(),
            font: None,
            lcd_text: Some("galdeck — no config, see galdeck.example.toml".into()),
            pages: vec![Page {
                name: "main".into(),
                lcd_text: None,
                keys: Vec::new(),
                encoders: Vec::new(),
            }],
        }
    }

    fn validate(&self) -> Result<()> {
        if self.brightness > 100 {
            bail!("brightness must be 0-100");
        }
        if self.pages.is_empty() {
            bail!("config needs at least one [[pages]] entry");
        }
        for page in &self.pages {
            for key in &page.keys {
                if key.key >= galdeck::Buttons::COUNT {
                    bail!("page {:?}: key {} out of range 0-11", page.name, key.key);
                }
                if let Some(color) = &key.color {
                    check_color(color)?;
                }
                if let Some(target) = &key.page {
                    if !self.pages.iter().any(|p| &p.name == target) {
                        bail!(
                            "page {:?}: key {} switches to unknown page {target:?}",
                            page.name,
                            key.key
                        );
                    }
                }
            }
            for encoder in &page.encoders {
                if encoder.encoder >= galdeck::Encoders::COUNT {
                    bail!(
                        "page {:?}: encoder {} out of range 0-1",
                        page.name,
                        encoder.encoder
                    );
                }
                if let Some(color) = &encoder.ring {
                    check_color(color)?;
                }
            }
        }
        Ok(())
    }
}

/// Validate a `#rrggbb` color string.
fn check_color(value: &str) -> Result<()> {
    if galdeck::Rgb::from_hex(value).is_none() {
        bail!("expected a color like #rrggbb, got {value:?}");
    }
    Ok(())
}

/// Default config file location: `$XDG_CONFIG_HOME/galdeck/config.toml`.
pub fn default_config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config")
        });
    base.join("galdeck").join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_example_config() {
        let text = include_str!("../../../config/galdeck.example.toml");
        let config: Config = toml::from_str(text).unwrap();
        config.validate().unwrap();
        assert!(!config.pages.is_empty());
    }

    #[test]
    fn rejects_malformed_colors() {
        let bad = r##"
            [[pages]]
            name = "main"
            [[pages.keys]]
            key = 0
            color = "#f80"
        "##;
        let config: Config = toml::from_str(bad).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_bad_references() {
        let text = r#"
            [[pages]]
            name = "main"
            [[pages.keys]]
            key = 3
            page = "nope"
        "#;
        let config: Config = toml::from_str(text).unwrap();
        assert!(config.validate().is_err());

        let text = r#"
            [[pages]]
            name = "main"
            [[pages.keys]]
            key = 12
        "#;
        let config: Config = toml::from_str(text).unwrap();
        assert!(config.validate().is_err());
    }
}
