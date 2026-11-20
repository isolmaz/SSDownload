//! The extractor plugin the application hands yt-dlp (`yt_dlp_plugin/instagram_story_fallback.py`,
//! compiled into the executable) and the folder one yt-dlp run loads it from.

use super::*;
use std::ffi::OsString;
use std::io::{Seek, SeekFrom};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

const PLUGIN_SOURCE: &[u8] = include_bytes!("yt_dlp_plugin/instagram_story_fallback.py");
/// yt-dlp looks below each plugin folder for `<package>/yt_dlp_plugins/extractor/<module>.py`.
const PLUGIN_PATH: [&str; 4] = [
    "ssdownload",
    "yt_dlp_plugins",
    "extractor",
    "instagram_story_fallback.py",
];
pub(super) const PLUGIN_LEASE_PREFIX: &str = "ssdownload-plugins-";
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// A plugin folder of its own for one yt-dlp run, made in the profile's private cookie-jar folder
/// and removed when the lease drops, so concurrent runs and the self-test never share one; the
/// start-up sweep removes what a hard kill left.
///
/// yt-dlp imports every module, package and archive it finds below a plugin folder, so the
/// folder holds the application's file and nothing else: the folder is new, the file is created
/// in it (never opened when it exists), it stays open without write or delete sharing while the
/// lease lives, and [`PluginLease::folder`] checks the whole tree and the file's bytes each time
/// the folder is handed to a run. A lease that cannot be made, or a folder that fails the check,
/// is logged and that run goes without the plugin (still with `--no-plugin-dirs`), so no download
/// depends on it.
pub(super) struct PluginLease {
    // Fields drop in order: the file closes before its folder is removed.
    file: File,
    root: LeaseFolder,
}

struct LeaseFolder(PathBuf);

impl Drop for LeaseFolder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl PluginLease {
    pub(super) fn create() -> Result<Self> {
        Self::create_in(&crate::paths::cookie_jar_dir()?)
    }

    pub(super) fn create_in(parent: &Path) -> Result<Self> {
        let path = parent.join(format!("{PLUGIN_LEASE_PREFIX}{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path)?;
        let root = LeaseFolder(path);
        let (module, folders) = PLUGIN_PATH.split_last().expect("the path names a module");
        let mut path = root.0.clone();
        for folder in folders {
            path.push(folder);
            fs::create_dir(&path)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ)
            .open(path.join(module))?;
        file.write_all(PLUGIN_SOURCE)?;
        Ok(Self { file, root })
    }

    /// The folder for `--plugin-dirs`, after checking that it holds exactly the plugin.
    pub(super) fn folder(&self) -> Result<&Path> {
        check_tree(&self.root.0, &PLUGIN_PATH)?;
        let mut file = &self.file;
        let mut written = Vec::with_capacity(PLUGIN_SOURCE.len());
        file.seek(SeekFrom::Start(0))?;
        file.read_to_end(&mut written)?;
        if written != PLUGIN_SOURCE {
            bail!("the plugin file differs from the built-in copy");
        }
        Ok(&self.root.0)
    }
}

/// A lease for one yt-dlp run, or none (logged) when its folder cannot be made.
pub(super) fn plugin_lease() -> Option<PluginLease> {
    logged(PluginLease::create())
}

/// The lease's folder for `--plugin-dirs`, or none (logged) when it fails the check.
pub(super) fn plugin_folder(lease: Option<&PluginLease>) -> Option<&Path> {
    logged(lease?.folder())
}

/// Checks the lease again before a later launch with the same arguments and drops
/// `--plugin-dirs <folder>` from them when the folder no longer passes.
pub(super) fn recheck_plugin_dirs(args: &mut Vec<String>, lease: Option<&PluginLease>) {
    if let Some(index) = args.iter().position(|argument| argument == "--plugin-dirs") {
        if plugin_folder(lease).is_none() {
            args.drain(index..(index + 2).min(args.len()));
        }
    }
}

fn logged<T>(result: Result<T>) -> Option<T> {
    result
        .map_err(|error| {
            crate::logging::record(
                crate::logging::Event::warn("media.plugin_unavailable")
                    .detail(format!("{error:#}")),
            );
        })
        .ok()
}

/// `folder` holds one entry, `expected[0]`, a real folder or (last) a real file, and so on down.
fn check_tree(folder: &Path, expected: &[&str]) -> Result<()> {
    let Some((name, below)) = expected.split_first() else {
        return Ok(());
    };
    let entries = fs::read_dir(folder)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    if entries != [OsString::from(name)] {
        bail!("{} holds more than the plugin", folder.display());
    }
    let path = folder.join(name);
    let metadata = fs::symlink_metadata(&path)?;
    let expected_kind = if below.is_empty() {
        metadata.is_file()
    } else {
        metadata.is_dir()
    };
    if !expected_kind || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!("{} is not the plugin's own entry", path.display());
    }
    check_tree(&path, below)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("ssdownload-plugin-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn module_path(lease: &PluginLease) -> PathBuf {
        PLUGIN_PATH
            .iter()
            .fold(lease.root.0.clone(), |path, part| path.join(part))
    }

    fn arguments(plugins: Option<&Path>) -> Vec<String> {
        common_args(
            Path::new("deno.exe"),
            Path::new("ffmpeg.exe"),
            plugins,
            false,
        )
    }

    fn without_plugins(arguments: &[String]) -> bool {
        arguments.contains(&"--no-plugin-dirs".to_owned())
            && !arguments.contains(&"--plugin-dirs".to_owned())
    }

    #[test]
    fn the_folder_holds_exactly_the_embedded_plugin() {
        let parent = scratch();
        let lease = PluginLease::create_in(&parent).unwrap();
        let folder = lease.folder().unwrap().to_path_buf();

        assert_eq!(folder.parent(), Some(parent.as_path()));
        assert!(folder
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(PLUGIN_LEASE_PREFIX));
        let source = include_str!("yt_dlp_plugin/instagram_story_fallback.py");
        assert!(source.contains("plugin_name='ssdownload'"));
        assert_eq!(fs::read(module_path(&lease)).unwrap(), source.as_bytes());

        drop(lease);
        assert!(!folder.exists());
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn concurrent_runs_get_folders_of_their_own() {
        let parent = scratch();
        let first = PluginLease::create_in(&parent).unwrap();
        let second = PluginLease::create_in(&parent).unwrap();

        assert_ne!(first.folder().unwrap(), second.folder().unwrap());
        drop(first);
        assert!(second.folder().is_ok());
        drop(second);
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn the_plugin_cannot_be_changed_or_deleted_while_the_lease_lives() {
        let parent = scratch();
        let lease = PluginLease::create_in(&parent).unwrap();
        let module = module_path(&lease);

        assert!(OpenOptions::new().write(true).open(&module).is_err());
        assert!(fs::remove_file(&module).is_err());
        assert_eq!(fs::read(&module).unwrap(), PLUGIN_SOURCE);
        assert!(lease.folder().is_ok());
        drop(lease);
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn an_extra_file_anywhere_in_the_folder_is_refused() {
        let parent = scratch();
        let lease = PluginLease::create_in(&parent).unwrap();
        let module = module_path(&lease);
        let extractor = module.parent().unwrap().to_path_buf();
        let extras = [
            lease.root.0.join("other.zip"),
            lease.root.0.join("ssdownload").join("extra.txt"),
            extractor.join("planted.py"),
            extractor.join("__pycache__"),
        ];
        for extra in &extras {
            if extra.extension().is_some() {
                fs::write(extra, b"print('planted')").unwrap();
            } else {
                fs::create_dir(extra).unwrap();
            }
            assert!(lease.folder().is_err(), "{}", extra.display());
            assert!(
                without_plugins(&arguments(plugin_folder(Some(&lease)))),
                "{}",
                extra.display()
            );
            if extra.is_dir() {
                fs::remove_dir(extra).unwrap();
            } else {
                fs::remove_file(extra).unwrap();
            }
            assert!(lease.folder().is_ok(), "{}", extra.display());
        }
        drop(lease);
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn a_changed_plugin_is_refused() {
        let parent = scratch();
        let lease = PluginLease::create_in(&parent).unwrap();
        let mut file = &lease.file;
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b"X").unwrap();

        assert!(lease.folder().is_err());
        assert!(without_plugins(&arguments(plugin_folder(Some(&lease)))));
        drop(lease);
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn a_lease_that_cannot_be_made_leaves_the_run_without_plugins() {
        let missing = scratch().join("missing");
        let lease = logged(PluginLease::create_in(&missing));

        assert!(lease.is_none());
        assert!(without_plugins(&arguments(plugin_folder(lease.as_ref()))));
    }

    #[test]
    fn a_later_launch_drops_the_plugin_folder_once_it_fails_the_check() {
        let parent = scratch();
        let lease = PluginLease::create_in(&parent).unwrap();
        let mut args = arguments(plugin_folder(Some(&lease)));
        let passing = args.clone();
        recheck_plugin_dirs(&mut args, Some(&lease));
        assert_eq!(args, passing);

        fs::write(lease.root.0.join("planted.zip"), b"PK").unwrap();
        recheck_plugin_dirs(&mut args, Some(&lease));
        assert!(without_plugins(&args), "{args:?}");
        let mut expected = passing.clone();
        let index = expected.iter().position(|a| a == "--plugin-dirs").unwrap();
        expected.drain(index..index + 2);
        assert_eq!(args, expected);
        drop(lease);
        fs::remove_dir_all(parent).unwrap();
    }
}
