//! Host side of RetailOS's asynchronous file service.
//!
//! Titles built on the Sims engine talk to `AsyncFileIO` the way RetailOS's
//! own worker answers it: an open publishes a small stream id, reads and seeks
//! move a per-stream position, and every operation is finished later through
//! the request's completion callback. This table only tracks the host files
//! and their positions; the guest-memory side of each request lives with the
//! import handler.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct OpenFile {
    data: Arc<[u8]>,
    pos: usize,
}

/// Open streams, indexed by the 1-based id the guest receives.
#[derive(Default)]
pub struct RetailFiles {
    files: Vec<OpenFile>,
    /// File contents shared between streams. Sims Bowling reopens its 20 MB
    /// `gameLib.rlb` for every resource it streams, and the Y2 cannot hold a
    /// copy per open.
    cache: HashMap<PathBuf, Arc<[u8]>>,
    /// The stream most recently opened through each guest file object. A
    /// request object is reused across files, so its stream field can be
    /// stale; the file object is refreshed on every open.
    by_object: HashMap<u32, u32>,
}

impl RetailFiles {
    /// Open `path` for reading and return its stream id, or `None` if the
    /// file cannot be read. An empty file counts as missing: fliwheel's other
    /// file paths create empty placeholders for saves that were never written,
    /// and an empty save is not one the game can parse.
    pub fn open(&mut self, path: &Path, object: u32) -> Option<u32> {
        let data = match self.cache.get(path) {
            Some(data) => data.clone(),
            None => {
                let data: Arc<[u8]> = fs::read(path).ok()?.into();
                self.cache.insert(path.to_path_buf(), data.clone());
                data
            }
        };
        if data.is_empty() {
            return None;
        }
        self.files.push(OpenFile { data, pos: 0 });
        let stream = self.files.len() as u32;
        if object != 0 {
            self.by_object.insert(object, stream);
        }
        Some(stream)
    }

    /// Resolve a request to its stream: the file object first, then the
    /// stream id the request carries.
    pub fn stream_for(&self, object: u32, stream: u32) -> Option<u32> {
        self.by_object
            .get(&object)
            .copied()
            .filter(|&s| s != 0)
            .or_else(|| self.file(stream).map(|_| stream))
    }

    pub fn len(&self, stream: u32) -> u32 {
        self.file(stream).map_or(0, |f| f.data.len() as u32)
    }

    pub fn pos(&self, stream: u32) -> u32 {
        self.file(stream).map_or(0, |f| f.pos as u32)
    }

    pub fn rewind(&mut self, stream: u32) {
        if let Some(f) = self.file_mut(stream) {
            f.pos = 0;
        }
    }

    /// Take up to `len` bytes from the current position and advance past them.
    pub fn read(&mut self, stream: u32, len: u32) -> Vec<u8> {
        let Some(f) = self.file_mut(stream) else {
            return Vec::new();
        };
        let start = f.pos.min(f.data.len());
        let end = start + (len as usize).min(f.data.len() - start);
        f.pos = end;
        f.data[start..end].to_vec()
    }

    /// Move forward by up to `by` bytes, returning how far the position moved.
    pub fn skip(&mut self, stream: u32, by: u32) -> u32 {
        let Some(f) = self.file_mut(stream) else {
            return 0;
        };
        let moved = (by as usize).min(f.data.len().saturating_sub(f.pos));
        f.pos += moved;
        moved as u32
    }

    /// `lseek`: `whence` 0 is the start, 1 the current position, 2 the end.
    /// The result is clamped to the file and returned.
    pub fn seek(&mut self, stream: u32, offset: i32, whence: u8) -> u32 {
        let Some(f) = self.file_mut(stream) else {
            return 0;
        };
        let base = match whence {
            1 => f.pos as i64,
            2 => f.data.len() as i64,
            _ => 0,
        };
        f.pos = (base + offset as i64).clamp(0, f.data.len() as i64) as usize;
        f.pos as u32
    }

    fn file(&self, stream: u32) -> Option<&OpenFile> {
        (stream as usize).checked_sub(1).and_then(|i| self.files.get(i))
    }

    fn file_mut(&mut self, stream: u32) -> Option<&mut OpenFile> {
        let index = (stream as usize).checked_sub(1)?;
        self.files.get_mut(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_with(bytes: &[u8]) -> (RetailFiles, u32, tempfile_path::Guard) {
        let guard = tempfile_path::Guard::new(bytes);
        let mut files = RetailFiles::default();
        let stream = files.open(&guard.0, 0x1000).unwrap();
        (files, stream, guard)
    }

    #[test]
    fn reads_advance_and_stop_at_the_end() {
        let (mut files, s, _g) = table_with(b"abcdef");
        assert_eq!(files.read(s, 4), b"abcd");
        assert_eq!(files.read(s, 4), b"ef");
        assert_eq!(files.pos(s), 6);
        assert!(files.read(s, 4).is_empty());
    }

    #[test]
    fn seek_honours_whence_and_clamps() {
        let (mut files, s, _g) = table_with(b"abcdef");
        assert_eq!(files.seek(s, 2, 0), 2);
        assert_eq!(files.seek(s, 1, 1), 3);
        assert_eq!(files.seek(s, -1, 2), 5);
        assert_eq!(files.seek(s, 100, 0), 6);
        assert_eq!(files.seek(s, -100, 1), 0);
        assert_eq!(files.skip(s, 4), 4);
        assert_eq!(files.skip(s, 4), 2);
    }

    #[test]
    fn reopening_shares_data_but_not_position() {
        let (mut files, a, g) = table_with(b"abcdef");
        files.read(a, 3);
        let b = files.open(&g.0, 0x2000).unwrap();
        assert_ne!(a, b);
        assert_eq!(files.pos(b), 0);
        assert_eq!(files.len(b), 6);
        assert_eq!(files.stream_for(0x2000, 0), Some(b));
        assert_eq!(files.stream_for(0x9999, a), Some(a));
        assert_eq!(files.stream_for(0x9999, 77), None);
    }

    #[test]
    fn empty_files_count_as_missing() {
        let g = tempfile_path::Guard::new(b"");
        assert_eq!(RetailFiles::default().open(&g.0, 0), None);
    }

    mod tempfile_path {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicUsize, Ordering};

        pub struct Guard(pub PathBuf);

        impl Guard {
            pub fn new(bytes: &[u8]) -> Self {
                static N: AtomicUsize = AtomicUsize::new(0);
                let path = std::env::temp_dir().join(format!(
                    "fliwheel-retail-files-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::write(&path, bytes).unwrap();
                Guard(path)
            }
        }

        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
    }
}
