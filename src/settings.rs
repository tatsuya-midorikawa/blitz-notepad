use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{BlitzError, Result};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EditorSettings {
    pub word_wrap: bool,
    pub status_bar_visible: bool,
    pub font_family: String,
    pub font_style: FontStyle,
    pub font_size: u16,
    pub script: String,
    pub zoom_percent: u16,
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self {
            word_wrap: false,
            status_bar_visible: true,
            font_family: "Consolas".to_owned(),
            font_style: FontStyle::Regular,
            font_size: 11,
            script: "Western".to_owned(),
            zoom_percent: 100,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FontStyle {
    Regular,
    Italic,
    Bold,
    BoldItalic,
}

impl FontStyle {
    pub fn label(self) -> &'static str {
        match self {
            FontStyle::Regular => "Regular",
            FontStyle::Italic => "Italic",
            FontStyle::Bold => "Bold",
            FontStyle::BoldItalic => "Bold Italic",
        }
    }
}

impl EditorSettings {
    pub fn load() -> Result<Self> {
        let Some(path) = settings_path() else {
            return Ok(Self::default());
        };
        Self::load_from_path(path)
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = fs::read(&path).map_err(|error| BlitzError::Io {
            path: path.to_path_buf(),
            source: error,
        })?;
        serde_json::from_slice(&bytes).map_err(|error| BlitzError::Settings(error.to_string()))
    }

    pub fn save(&self) -> Result<()> {
        let Some(path) = settings_path() else {
            return Ok(());
        };
        self.save_to_path(path)
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| BlitzError::Io {
                path: parent.to_path_buf(),
                source: error,
            })?;
        }
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| BlitzError::Settings(error.to_string()))?;
        fs::write(&path, bytes).map_err(|error| BlitzError::Io {
            path: path.to_path_buf(),
            source: error,
        })
    }

    pub fn set_zoom_percent(&mut self, zoom_percent: u16) {
        self.zoom_percent = zoom_percent.clamp(10, 500);
    }
}

pub fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|directory| directory.join("blitz-notepad").join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_to_path() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("settings.json");
        let mut settings = EditorSettings::default();
        settings.word_wrap = true;
        settings.status_bar_visible = false;
        settings.set_zoom_percent(250);

        settings.save_to_path(&path).expect("save settings");
        let loaded = EditorSettings::load_from_path(&path).expect("load settings");

        assert_eq!(loaded.word_wrap, true);
        assert_eq!(loaded.status_bar_visible, false);
        assert_eq!(loaded.zoom_percent, 250);
    }

    #[test]
    fn zoom_percent_is_clamped() {
        let mut settings = EditorSettings::default();
        settings.set_zoom_percent(1);
        assert_eq!(settings.zoom_percent, 10);
        settings.set_zoom_percent(999);
        assert_eq!(settings.zoom_percent, 500);
    }
}
