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
    /// Read a text file, replacing invalid UTF-8 instead of failing. `/proc`
    /// `cmdline`/`comm` are arbitrary bytes, not guaranteed UTF-8, so process
    /// discovery must not drop a valid process over an encoding error.
    fn read_to_string_lossy(&self, path: &Path) -> Option<String> {
        self.read_to_string(path)
    }
    /// List a directory. `None` if the directory is missing or unreadable.
    fn read_dir(&self, path: &Path) -> Option<Vec<SysEntry>>;
    /// Direct symlink target (not resolved). `None` if not a symlink or
    fn symlink_target(&self, path: &Path) -> Option<String>;
    /// List only the entry **names** of a directory, without reading per-entry
    /// metadata. The DRM fd scan only needs the fd numbers and reads each
    /// symlink target itself, so it can skip the per-entry classification.
    /// `None` if the directory is missing or unreadable. Defaults to
    /// [`Sys::read_dir`].
    fn read_dir_names(&self, path: &Path) -> Option<Vec<String>> {
        self.read_dir(path)
            .map(|entries| entries.into_iter().map(|entry| entry.name).collect())
    }
    /// Whether the symlink at `path` points at a target that starts with the
    /// raw byte `prefix`, without materializing the target as an owned
    /// `String`. The DRM fd scan uses this to test the `/dev/dri/` prefix on
    /// every open fd without one allocation per fd. Defaults to
    /// [`Sys::symlink_target`].
    fn symlink_target_starts_with(&self, path: &Path, prefix: &[u8]) -> bool {
        self.symlink_target(path)
            .is_some_and(|target| target.as_bytes().starts_with(prefix))
    }
}

/// Production implementation backed by the local filesystem.
pub struct RealSys;

impl Sys for RealSys {
    fn read_to_string(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn read_to_string_lossy(&self, path: &Path) -> Option<String> {
        std::fs::read(path)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
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

    fn read_dir_names(&self, path: &Path) -> Option<Vec<String>> {
        let entries = std::fs::read_dir(path).ok()?;
        Some(
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect(),
        )
    }

    fn symlink_target_starts_with(&self, path: &Path, prefix: &[u8]) -> bool {
        use std::os::unix::ffi::OsStrExt;
        std::fs::read_link(path)
            .map(|target| target.as_os_str().as_bytes().starts_with(prefix))
            .unwrap_or(false)
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

    #[test]
    fn real_read_dir_names_lists_entry_names_without_metadata() {
        let sys = RealSys;
        // A missing path is unreadable: `None`, not an empty listing.
        assert_eq!(
            sys.read_dir_names(Path::new("/orsiktop-definitely-missing-directory")),
            None
        );

        let dir = std::env::temp_dir().join(format!("orsiktop-readnames-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A successfully read, empty directory is `Some([])`.
        assert_eq!(sys.read_dir_names(&dir), Some(Vec::new()));

        // A populated directory returns exactly the entry names, no metadata.
        std::fs::write(dir.join("42"), "").unwrap();
        std::fs::write(dir.join("43"), "").unwrap();
        let mut names = sys.read_dir_names(&dir).unwrap();
        names.sort();
        assert_eq!(names, vec!["42".to_string(), "43".to_string()]);

        // A regular file is not a readable directory.
        assert_eq!(sys.read_dir_names(&dir.join("42")), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn real_symlink_target_starts_with_compares_raw_prefix() {
        use std::os::unix::fs::symlink;

        let sys = RealSys;
        let dir = std::env::temp_dir().join(format!("orsiktop-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A (possibly dangling) symlink to a DRM node matches the prefix.
        let dri = dir.join("renderD128");
        symlink("/dev/dri/renderD128", &dri).unwrap();
        assert!(sys.symlink_target_starts_with(&dri, b"/dev/dri/"));

        // A symlink to a different target does not match `/dev/dri/`, but its
        // own prefix does — the comparison is on the raw target bytes.
        let other = dir.join("other");
        symlink("/tmp/orsiktop-somewhere", &other).unwrap();
        assert!(!sys.symlink_target_starts_with(&other, b"/dev/dri/"));
        assert!(sys.symlink_target_starts_with(&other, b"/tmp/"));

        // A regular file is not a symlink: never a match, even for an empty
        // prefix that every string would start with.
        let plain = dir.join("plain");
        std::fs::write(&plain, "x").unwrap();
        assert!(!sys.symlink_target_starts_with(&plain, b"/dev/dri/"));
        assert!(!sys.symlink_target_starts_with(&plain, b""));

        // A missing path is never a matching symlink.
        assert!(!sys.symlink_target_starts_with(&dir.join("missing"), b"/dev/dri/"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn default_and_real_naht_methods_agree_on_equivalent_data() {
        use std::os::unix::fs::symlink;

        // The override over a temp directory ...
        let real = RealSys;
        let dir = std::env::temp_dir().join(format!("orsiktop-naht-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("42"), "").unwrap();
        std::fs::write(dir.join("43"), "").unwrap();
        symlink("/dev/dri/renderD128", dir.join("render")).unwrap();
        symlink("/tmp/other", dir.join("other")).unwrap();

        // ... and the `Sys` default impl (via `FixtureSys`) describing the same
        // logical directory must agree, so default and override cannot diverge.
        let mut fixture = FixtureSys::default();
        fixture
            .dir_entry("/proc/123/fd", "42", false, true)
            .dir_entry("/proc/123/fd", "43", false, true)
            .dir_entry("/proc/123/fd", "render", false, true)
            .dir_entry("/proc/123/fd", "other", false, true)
            .symlink("/proc/123/fd/render", "/dev/dri/renderD128")
            .symlink("/proc/123/fd/other", "/tmp/other");

        let mut real_names = real.read_dir_names(&dir).unwrap();
        let mut default_names = fixture.read_dir_names(Path::new("/proc/123/fd")).unwrap();
        real_names.sort();
        default_names.sort();
        assert_eq!(real_names, default_names);

        for (fd, real_path) in [("render", dir.join("render")), ("other", dir.join("other"))] {
            let fixture_path = format!("/proc/123/fd/{fd}");
            for prefix in [
                b"/dev/dri/".as_slice(),
                b"/tmp/".as_slice(),
                b"/none/".as_slice(),
            ] {
                assert_eq!(
                    real.symlink_target_starts_with(&real_path, prefix),
                    fixture.symlink_target_starts_with(Path::new(&fixture_path), prefix),
                    "prefix disagreement on {fixture_path} for {prefix:?}"
                );
            }
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
