//! The two folders RawEditor keeps on disk, in one place.
//!
//! Every path below used to be rebuilt by hand at each call site
//! (`dirs::data_dir().or_else(dirs::home_dir)…join("raw-editor")`), with
//! slightly different fallbacks. Preferences › Folders opens these, so they
//! must be where the files actually go — hence one definition.
//!
//! | folder | Linux | macOS | Windows |
//! |---|---|---|---|
//! | [`data_dir`]  | `~/.local/share/raw-editor` | `~/Library/Application Support/raw-editor` | `%APPDATA%\raw-editor` |
//! | [`cache_dir`] | `~/.cache/raw-editor` | `~/Library/Caches/raw-editor` | `%LOCALAPPDATA%\raw-editor` |

use std::path::PathBuf;

const APP_DIR: &str = "raw-editor";

/// The catalogue (`raw_editor.db`), `settings.json`, DCP profiles
/// (`profiles/`), the user's own lensfun files (`lensfun/`) and the logs.
/// Everything here is the user's: losing it loses work.
pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR)
}

/// Generated previews (thumbnail / instant / working tiers and edited
/// proxies). Disposable: missing files are re-queued at the next launch.
/// `None` when the platform reports neither a cache nor a home directory.
pub fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().or_else(dirs::home_dir).map(|dir| dir.join(APP_DIR))
}

/// Where DCP camera profiles are looked up.
pub fn profiles_dir() -> PathBuf {
    data_dir().join("profiles")
}

/// One of the folders Preferences › Folders can open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppFolder {
    /// [`data_dir`]
    Data,
    /// [`cache_dir`]
    Cache,
}

impl AppFolder {
    pub fn path(self) -> Option<PathBuf> {
        match self {
            AppFolder::Data => Some(data_dir()),
            AppFolder::Cache => cache_dir(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two must never coincide: the cache is advertised as safe to
    /// delete, the data folder holds the catalogue.
    #[test]
    fn data_and_cache_are_separate_raw_editor_folders() {
        let data = data_dir();
        assert!(data.ends_with(APP_DIR));
        assert!(profiles_dir().starts_with(&data));
        if let Some(cache) = cache_dir() {
            assert!(cache.ends_with(APP_DIR));
            assert_ne!(cache, data);
        }
    }
}
