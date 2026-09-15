//! Minimal filesystem access for telemetry providers.
//!
//! Real sysfs/procfs reads and fixture-driven tests share one interface so
//! hardware parsing can be tested without the matching physical device.
//! Only the operations providers actually need are exposed; providers keep
//! the parsing logic themselves so it stays unit-testable.

#[cfg(test)]
use std::collections::BTreeMap;
use std::path::Path;

/// One directory entry, with only the properties providers need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SysEntry {
    pub name: String,
    pub is_symlink: bool,
    pub is_dir: bool,
}

pub trait Sys {
    /// Read a text file. `None` if the file is missing or unreadable.
    fn read_to_string(&self, path: &Path) -> Option<String>;
    /// List a directory. `None` if the directory is missing or unreadable.
    fn read_dir(&self, path: &Path) -> Option<Vec<SysEntry>>;
    /// Direct symlink target (not resolved). `None` if not a symlink or
    /// the target cannot be read.
    #[allow(dead_code)]
    fn symlink_target(&self, path: &Path) -> Option<String>;
}

/// Production implementation backed by the local filesystem.
pub struct RealSys;

impl Sys for RealSys {
    fn read_to_string(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn read_dir(&self, path: &Path) -> Option<Vec<SysEntry>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path).ok()? {
            let entry = entry.ok()?;
            let file_type = entry.file_type().ok()?;
            entries.push(SysEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_symlink: file_type.is_symlink(),
                is_dir: file_type.is_dir(),
            });
        }
        Some(entries)
    }

    fn symlink_target(&self, path: &Path) -> Option<String> {
        std::fs::read_link(path)
            .ok()
            .map(|target| target.to_string_lossy().into_owned())
    }
}
/// In-memory filesystem for fixture-driven provider tests.
#[cfg(test)]
#[derive(Default)]
pub struct FixtureSys {
    files: BTreeMap<String, String>,
    dirs: BTreeMap<String, Vec<SysEntry>>,
    symlinks: BTreeMap<String, String>,
}
#[cfg(test)]
impl FixtureSys {
    pub fn file(&mut self, path: &str, content: impl Into<String>) -> &mut Self {
        self.files.insert(path.to_string(), content.into());
        self
    }

    pub fn dir_entry(
        &mut self,
        path: &str,
        name: &str,
        is_dir: bool,
        is_symlink: bool,
    ) -> &mut Self {
        self.dirs
            .entry(path.to_string())
            .or_default()
            .push(SysEntry {
                name: name.to_string(),
                is_dir,
                is_symlink,
            });
        self
    }

    pub fn symlink(&mut self, path: &str, target: &str) -> &mut Self {
        self.symlinks.insert(path.to_string(), target.to_string());
        self
    }
}
#[cfg(test)]
impl Sys for FixtureSys {
    fn read_to_string(&self, path: &Path) -> Option<String> {
        self.files
            .get(&path.to_string_lossy().into_owned())
            .cloned()
    }

    fn read_dir(&self, path: &Path) -> Option<Vec<SysEntry>> {
        self.dirs.get(&path.to_string_lossy().into_owned()).cloned()
    }

    fn symlink_target(&self, path: &Path) -> Option<String> {
        self.symlinks
            .get(&path.to_string_lossy().into_owned())
            .cloned()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn fixture_round_trips_files_dirs_and_symlinks() {
        let mut fixture = FixtureSys::default();
        fixture
            .file("/proc/cpuinfo", "vendor_id : GenuineIntel\n")
            .dir_entry("/sys/class/drm", "card0", false, true)
            .dir_entry("/sys/class/drm", "card0-HDMI-A-1", false, true)
            .dir_entry("/sys/class/drm", "version", false, false)
            .symlink(
                "/sys/class/drm/card0",
                "../../devices/pci0000:00/0000:00:02.0",
            );

        let sys: &dyn Sys = &fixture;
        assert_eq!(
            sys.read_to_string(&PathBuf::from("/proc/cpuinfo"))
                .as_deref(),
            Some("vendor_id : GenuineIntel\n")
        );
        assert_eq!(sys.read_to_string(&PathBuf::from("/proc/missing")), None);

        let entries = sys.read_dir(&PathBuf::from("/sys/class/drm")).unwrap();
        assert_eq!(entries.len(), 3);
        let card0 = entries.iter().find(|entry| entry.name == "card0").unwrap();
        assert!(card0.is_symlink);
        assert!(!card0.is_dir);

        assert_eq!(
            sys.symlink_target(&PathBuf::from("/sys/class/drm/card0"))
                .as_deref(),
            Some("../../devices/pci0000:00/0000:00:02.0")
        );
        assert_eq!(sys.read_dir(&PathBuf::from("/missing")), None);
    }
}
