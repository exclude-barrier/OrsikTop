//! Minimal filesystem access for telemetry providers.
//!
//! Real sysfs/procfs reads and fixture-driven tests share one interface so
//! hardware parsing can be tested without the matching physical device.
//! Only the operations providers actually need are exposed; providers keep
//! the parsing logic themselves so it stays unit-testable.

#[cfg(test)]
use std::collections::BTreeMap;
use std::io;
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
    fn symlink_target(&self, path: &Path) -> Option<String>;
}

/// Production implementation backed by the local filesystem.
pub struct RealSys;

impl Sys for RealSys {
    fn read_to_string(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn read_dir(&self, path: &Path) -> Option<Vec<SysEntry>> {
        let entries = std::fs::read_dir(path).ok()?;
        Some(collect_dir_entries(entries.map(classify_entry)))
    }

    fn symlink_target(&self, path: &Path) -> Option<String> {
        std::fs::read_link(path)
            .ok()
            .map(|target| target.to_string_lossy().into_owned())
    }
}

/// Classify one raw directory entry. A single unreadable entry — or one whose
/// metadata cannot be read — is reported as an error and later dropped, so one
/// bad entry can never blank the whole listing.
fn classify_entry(entry: io::Result<std::fs::DirEntry>) -> io::Result<SysEntry> {
    let entry = entry?;
    let file_type = entry.file_type()?;
    Ok(SysEntry {
        name: entry.file_name().to_string_lossy().into_owned(),
        is_symlink: file_type.is_symlink(),
        is_dir: file_type.is_dir(),
    })
}

/// Keep the readable entries of a directory scan and drop per-entry failures.
fn collect_dir_entries<I: Iterator<Item = io::Result<SysEntry>>>(entries: I) -> Vec<SysEntry> {
    entries.filter_map(Result::ok).collect()
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

    #[test]
    fn per_entry_failures_drop_only_the_failed_entries() {
        // Deterministic simulation of a directory scan where one entry cannot
        // be read: only that entry is dropped, the readable ones survive.
        let card = SysEntry {
            name: "card0".to_string(),
            is_symlink: true,
            is_dir: false,
        };
        let render = SysEntry {
            name: "renderD128".to_string(),
            is_symlink: true,
            is_dir: false,
        };
        let entries = vec![
            Ok(card.clone()),
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unreadable",
            )),
            Ok(render.clone()),
        ];

        let kept = collect_dir_entries(entries.into_iter());
        assert_eq!(kept, vec![card, render]);
    }

    #[test]
    fn real_read_dir_distinguishes_unreadable_from_empty() {
        let sys = RealSys;
        // A missing path is unreadable: `None`, not an empty listing.
        assert_eq!(
            sys.read_dir(Path::new("/orsiktop-definitely-missing-directory")),
            None
        );

        let dir = std::env::temp_dir().join(format!("orsiktop-readdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A successfully read, empty directory is `Some([])`.
        assert_eq!(sys.read_dir(&dir), Some(Vec::new()));

        // A regular file is not a readable directory either.
        let file = dir.join("a-file");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(sys.read_dir(&file), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
