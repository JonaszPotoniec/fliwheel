//! Host side of a title's writable data, the iPod's `GameData_RW/<GameID>`.
//!
//! Games name their saves with bare relative paths, so everything lives under
//! one root. Writes collect in memory and reach the disk as a whole file when
//! the guest closes it or the emulator flushes: a write to `<name>.tmp` that
//! is synced and renamed into place, so losing power or sharing the card
//! leaves the previous save intact.
//!
//! The guest's handle tables stay with the import handlers and refer to files
//! by host path, which is the key used here.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Directories under an old save folder that hold extracted audio, not saves.
const CACHE_DIR_SUFFIX: &str = "-sfx";

pub struct SaveStore {
    root: PathBuf,
    dirty: HashMap<PathBuf, Vec<u8>>,
    commits: Arc<AtomicU64>,
}

impl SaveStore {
    pub fn new(root: PathBuf) -> Self {
        SaveStore {
            root,
            dirty: HashMap::new(),
            commits: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Counts files written to disk, so a host can see a game saving.
    pub fn commits(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.commits)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a guest path lives in the store. Nothing is created. Paths that
    /// would leave the root have no place in it.
    pub fn host_path(&self, guest_path: &str) -> Option<PathBuf> {
        let normalized = guest_path.replace('\\', "/");
        let relative = Path::new(normalized.trim_start_matches('/'));
        if relative.as_os_str().is_empty()
            || !relative
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            return None;
        }
        Some(self.root.join(relative))
    }

    pub fn owns(&self, host_path: &Path) -> bool {
        host_path.starts_with(&self.root)
    }

    /// The host path of a guest file that has saved content. An empty file
    /// counts as missing: earlier builds left empty placeholders behind, and
    /// an empty save is not one a game can parse.
    pub fn existing(&self, guest_path: &str) -> Option<PathBuf> {
        let path = self.host_path(guest_path)?;
        let has_content = self.dirty.get(&path).map(|data| !data.is_empty());
        let has_content = has_content.unwrap_or_else(|| {
            fs::metadata(&path).map_or(false, |meta| meta.is_file() && meta.len() > 0)
        });
        has_content.then_some(path)
    }

    /// Current contents, pending writes included. `None` for a missing file.
    pub fn read(&self, host_path: &Path) -> Option<Vec<u8>> {
        if let Some(data) = self.dirty.get(host_path) {
            return Some(data.clone());
        }
        fs::read(host_path).ok()
    }

    /// Start writing a file, keeping or discarding what it holds.
    pub fn begin_write(&mut self, host_path: &Path, truncate: bool) {
        if truncate {
            self.dirty.insert(host_path.to_path_buf(), Vec::new());
        } else if !self.dirty.contains_key(host_path) {
            let data = fs::read(host_path).unwrap_or_default();
            self.dirty.insert(host_path.to_path_buf(), data);
        }
    }

    /// Place `bytes` at `offset`, growing the file with zeros if needed.
    pub fn write_at(&mut self, host_path: &Path, offset: usize, bytes: &[u8]) {
        self.begin_write(host_path, false);
        let data = self
            .dirty
            .get_mut(host_path)
            .expect("begin_write added the file");
        let end = offset.saturating_add(bytes.len());
        if data.len() < end {
            data.resize(end, 0);
        }
        data[offset..end].copy_from_slice(bytes);
    }

    /// Write one file's pending data to disk. A file with nothing pending
    /// is already there.
    pub fn commit(&mut self, host_path: &Path) -> io::Result<()> {
        let Some(data) = self.dirty.get(host_path) else {
            return Ok(());
        };
        write_atomically(host_path, data)?;
        self.dirty.remove(host_path);
        self.commits.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Commit everything pending. Every file is attempted; the last failure
    /// is returned and the failed files stay pending.
    pub fn flush_all(&mut self) -> io::Result<()> {
        let mut result = Ok(());
        for path in self.dirty.keys().cloned().collect::<Vec<_>>() {
            if let Err(error) = self.commit(&path) {
                result = Err(error);
            }
        }
        result
    }

    pub fn delete(&mut self, host_path: &Path) -> io::Result<()> {
        self.dirty.remove(host_path);
        match fs::remove_file(host_path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Copy saves from an older location. A file already saved here wins, so
    /// running this again is harmless. The old files stay where they are.
    pub fn migrate_from(&self, old_root: &Path) -> io::Result<usize> {
        if old_root == self.root || !old_root.is_dir() {
            return Ok(0);
        }
        self.migrate_dir(old_root, &self.root)
    }

    fn migrate_dir(&self, from: &Path, to: &Path) -> io::Result<usize> {
        let mut copied = 0;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            let (source, target) = (entry.path(), to.join(entry.file_name()));
            let meta = entry.metadata()?;
            if meta.is_dir() {
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(CACHE_DIR_SUFFIX)
                {
                    copied += self.migrate_dir(&source, &target)?;
                }
            } else if meta.len() > 0 && !target.exists() {
                write_atomically(&target, &fs::read(&source)?)?;
                copied += 1;
            }
        }
        Ok(copied)
    }
}

impl Drop for SaveStore {
    fn drop(&mut self) {
        let _ = self.flush_all();
    }
}

fn write_atomically(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut file = File::create(&temp)?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "fliwheel-save-store-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            TempRoot(path)
        }

        fn store(&self) -> SaveStore {
            SaveStore::new(self.0.join("saves"))
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn nothing_reaches_the_disk_before_commit() {
        let temp = TempRoot::new();
        let mut store = temp.store();
        let path = store.host_path("game.sav").unwrap();
        store.write_at(&path, 0, b"abc");
        assert!(!path.exists());
        assert_eq!(store.read(&path).unwrap(), b"abc");
        store.commit(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"abc");
    }

    #[test]
    fn commit_creates_directories_and_leaves_no_temp_file() {
        let temp = TempRoot::new();
        let mut store = temp.store();
        let path = store.host_path("en/stats").unwrap();
        store.write_at(&path, 0, b"x");
        store.commit(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"x");
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["stats"]);
    }

    #[test]
    fn truncating_replaces_and_keeping_overlays() {
        let temp = TempRoot::new();
        let mut store = temp.store();
        let path = store.host_path("prefs").unwrap();
        store.write_at(&path, 0, b"abcdef");
        store.commit(&path).unwrap();

        store.write_at(&path, 1, b"XY");
        store.commit(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"aXYdef");

        store.begin_write(&path, true);
        store.write_at(&path, 0, b"z");
        store.commit(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"z");
    }

    #[test]
    fn writing_past_the_end_fills_with_zeros() {
        let temp = TempRoot::new();
        let mut store = temp.store();
        let path = store.host_path("gap").unwrap();
        store.write_at(&path, 2, b"a");
        assert_eq!(store.read(&path).unwrap(), [0, 0, b'a']);
    }

    #[test]
    fn missing_and_empty_files_do_not_exist() {
        let temp = TempRoot::new();
        let mut store = temp.store();
        assert_eq!(store.existing("nope"), None);
        let path = store.host_path("empty").unwrap();
        store.begin_write(&path, true);
        store.commit(&path).unwrap();
        assert!(path.exists());
        assert_eq!(store.existing("empty"), None);
        store.write_at(&path, 0, b"a");
        assert_eq!(store.existing("empty"), Some(path));
    }

    #[test]
    fn paths_cannot_leave_the_root() {
        let temp = TempRoot::new();
        let store = temp.store();
        assert_eq!(store.host_path("../secret"), None);
        assert_eq!(store.host_path("a/../../b"), None);
        assert_eq!(store.host_path(""), None);
        assert_eq!(store.host_path("/"), None);
        assert_eq!(
            store.host_path("\\en\\stats"),
            Some(store.root().join("en/stats"))
        );
    }

    #[test]
    fn flush_commits_every_pending_file() {
        let temp = TempRoot::new();
        let mut store = temp.store();
        let (a, b) = (
            store.host_path("a").unwrap(),
            store.host_path("b").unwrap(),
        );
        store.write_at(&a, 0, b"1");
        store.write_at(&b, 0, b"2");
        store.flush_all().unwrap();
        assert_eq!(fs::read(&a).unwrap(), b"1");
        assert_eq!(fs::read(&b).unwrap(), b"2");
    }

    #[test]
    fn dropping_the_store_flushes() {
        let temp = TempRoot::new();
        let path = {
            let mut store = temp.store();
            let path = store.host_path("late").unwrap();
            store.write_at(&path, 0, b"kept");
            path
        };
        assert_eq!(fs::read(&path).unwrap(), b"kept");
    }

    #[test]
    fn delete_forgets_pending_data_and_tolerates_missing_files() {
        let temp = TempRoot::new();
        let mut store = temp.store();
        let path = store.host_path("gone").unwrap();
        store.write_at(&path, 0, b"a");
        store.delete(&path).unwrap();
        assert_eq!(store.read(&path), None);
        store.flush_all().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn migration_copies_saves_but_not_placeholders_or_audio_caches() {
        let temp = TempRoot::new();
        let old = temp.0.join("old");
        fs::create_dir_all(old.join("en")).unwrap();
        fs::create_dir_all(old.join("holdem-sfx")).unwrap();
        fs::write(old.join("game.sav"), b"save").unwrap();
        fs::write(old.join("en/stats"), b"stats").unwrap();
        fs::write(old.join("placeholder"), b"").unwrap();
        fs::write(old.join("holdem-sfx/sfx-00.wav"), b"wav").unwrap();

        let store = temp.store();
        assert_eq!(store.migrate_from(&old).unwrap(), 2);
        assert_eq!(fs::read(store.root().join("game.sav")).unwrap(), b"save");
        assert_eq!(fs::read(store.root().join("en/stats")).unwrap(), b"stats");
        assert!(!store.root().join("placeholder").exists());
        assert!(!store.root().join("holdem-sfx").exists());
        assert_eq!(fs::read(old.join("game.sav")).unwrap(), b"save");
    }

    #[test]
    fn migration_keeps_newer_saves() {
        let temp = TempRoot::new();
        let old = temp.0.join("old");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("game.sav"), b"old").unwrap();

        let mut store = temp.store();
        let path = store.host_path("game.sav").unwrap();
        store.write_at(&path, 0, b"new");
        store.commit(&path).unwrap();
        assert_eq!(store.migrate_from(&old).unwrap(), 0);
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }
}
