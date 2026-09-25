//! Where a store's bytes live: a real file, or a byte vector in memory.
//!
//! The store addresses its storage by byte offset and never relies on a shared
//! file cursor, so every operation is positional.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The byte interface a store reads and writes through.
///
/// Implementations must make everything written before a successful
/// [`sync`](Storage::sync) durable, and must report a failed sync as an error:
/// the store treats that as fatal. Nothing else is ordered: a power cut may
/// keep any subset of the writes since the last sync.
///
/// ```
/// use kladde_store::{MemoryStorage, Storage};
///
/// let mut s = MemoryStorage::new();
/// s.set_len(8)?;
/// s.write_at(b"kladde", 1)?;
/// s.sync()?;
/// let mut buf = [0u8; 8];
/// s.read_at(&mut buf, 0)?;
/// assert_eq!(&buf, b"\0kladde\0");
/// # Ok::<(), std::io::Error>(())
/// ```
pub trait Storage: Send {
    /// The current length in bytes.
    fn len(&self) -> io::Result<u64>;
    /// Reads exactly `buf.len()` bytes at `offset`, which must lie within the
    /// current length.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    /// Writes `buf` at `offset`, which must lie within the current length.
    fn write_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()>;
    /// Grows or shrinks to `len` bytes; bytes a growth exposes read as zero.
    fn set_len(&mut self, len: u64) -> io::Result<()>;
    /// Makes everything written so far durable.
    fn sync(&mut self) -> io::Result<()>;
}

/// A [`Storage`] over a file on disk.
///
/// Extends the file with `set_len`, which leaves the new region unallocated
/// until it is written: on a nearly full disk, running out of space then
/// surfaces at the next sync rather than when the file grows.
///
/// ```
/// use kladde_store::{FileStorage, Storage};
///
/// let path = std::env::temp_dir().join("kladde-file-storage-doc-example");
/// let mut s = FileStorage::create(&path)?;
/// s.set_len(4096)?;
/// s.write_at(b"hello", 0)?;
/// s.sync()?;
/// drop(s);
/// let s = FileStorage::open(&path)?;
/// assert_eq!(s.len()?, 4096);
/// # std::fs::remove_file(&path)?;
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
pub struct FileStorage {
    file: File,
}

impl FileStorage {
    /// Opens an existing file for reading and writing.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(FileStorage { file })
    }

    /// Creates a file, truncating it if it exists.
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(FileStorage { file })
    }
}

impl Storage for FileStorage {
    fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    #[cfg(unix)]
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(buf, offset)
    }

    #[cfg(windows)]
    fn read_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            let n = self.file.seek_read(buf, offset)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            buf = &mut buf[n..];
            offset += n as u64;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn write_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.write_all_at(buf, offset)
    }

    #[cfg(windows)]
    fn write_at(&mut self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            let n = self.file.seek_write(buf, offset)?;
            buf = &buf[n..];
            offset += n as u64;
        }
        Ok(())
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }
}

/// A [`Storage`] held in memory.
///
/// Clones share one image, so a test can keep a handle, drop the store, and
/// inspect or reopen what it left behind. With
/// [`with_crash_tracking`](MemoryStorage::with_crash_tracking) it also keeps
/// every write since the last sync apart, so that
/// [`crash_image`](MemoryStorage::crash_image) can produce what a power cut
/// might leave: the image as of the last sync, plus any subset of the writes
/// since.
///
/// ```
/// use kladde_store::{MemoryStorage, Storage};
///
/// let mut s = MemoryStorage::with_crash_tracking();
/// s.set_len(4)?;
/// s.write_at(b"ab", 0)?;
/// s.sync()?;
/// s.write_at(b"cd", 2)?;
/// // The write since the last sync may or may not have reached the disk.
/// assert_eq!(s.crash_image(|_| false), b"ab\0\0");
/// assert_eq!(s.crash_image(|_| true), b"abcd");
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Clone, Default, Debug)]
pub struct MemoryStorage {
    inner: Arc<Mutex<MemImage>>,
}

#[derive(Default, Debug)]
struct MemImage {
    current: Vec<u8>,
    track: bool,
    durable: Vec<u8>,
    since_sync: Vec<Op>,
    fail_syncs: bool,
    /// Writes left before every operation fails, as if the power went out.
    writes_left: Option<u64>,
}

impl MemImage {
    fn dead(&mut self) -> io::Result<()> {
        match &mut self.writes_left {
            Some(0) => Err(io::Error::other("injected power cut")),
            Some(n) => {
                *n -= 1;
                Ok(())
            }
            None => Ok(()),
        }
    }
}

#[derive(Debug, Clone)]
enum Op {
    Write { offset: u64, bytes: Vec<u8> },
    SetLen(u64),
}

impl MemoryStorage {
    /// An empty image.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty image that remembers writes since the last sync.
    pub fn with_crash_tracking() -> Self {
        let s = Self::default();
        s.inner.lock().unwrap().track = true;
        s
    }

    /// An image holding `bytes`, all of them durable.
    ///
    /// Together with [`image`](Self::image), this reopens what a store left
    /// behind as if the process had restarted.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Storage};
    ///
    /// let mut a = MemoryStorage::new();
    /// a.set_len(2)?;
    /// a.write_at(b"hi", 0)?;
    /// let b = MemoryStorage::from_image(a.image());
    /// assert_eq!(b.len()?, 2);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn from_image(bytes: Vec<u8>) -> Self {
        let s = Self::default();
        {
            let mut m = s.inner.lock().unwrap();
            m.durable = bytes.clone();
            m.current = bytes;
        }
        s
    }

    /// Like [`from_image`](Self::from_image), remembering writes from now on.
    pub fn from_image_with_crash_tracking(bytes: Vec<u8>) -> Self {
        let s = Self::from_image(bytes);
        s.inner.lock().unwrap().track = true;
        s
    }

    /// The current bytes, as a reader would see them.
    pub fn image(&self) -> Vec<u8> {
        self.inner.lock().unwrap().current.clone()
    }

    /// How many writes and resizes happened since the last sync. Only counted
    /// with crash tracking on.
    pub fn unsynced_ops(&self) -> usize {
        self.inner.lock().unwrap().since_sync.len()
    }

    /// The bytes a power cut could leave: the image as of the last sync, plus
    /// the operations since then for which `keep(index)` is true, applied in
    /// order. Requires crash tracking.
    pub fn crash_image(&self, mut keep: impl FnMut(usize) -> bool) -> Vec<u8> {
        let m = self.inner.lock().unwrap();
        assert!(
            m.track,
            "crash_image needs a MemoryStorage with crash tracking"
        );
        let mut img = m.durable.clone();
        for (i, op) in m.since_sync.iter().enumerate() {
            if !keep(i) {
                continue;
            }
            match op {
                Op::Write { offset, bytes } => {
                    let end = *offset as usize + bytes.len();
                    if img.len() < end {
                        img.resize(end, 0);
                    }
                    img[*offset as usize..end].copy_from_slice(bytes);
                }
                Op::SetLen(len) => img.resize(*len as usize, 0),
            }
        }
        img
    }

    /// Makes every later sync fail, to exercise the fatal path.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Storage};
    ///
    /// let mut s = MemoryStorage::new();
    /// s.fail_syncs();
    /// assert!(s.sync().is_err());
    /// ```
    pub fn fail_syncs(&self) {
        self.inner.lock().unwrap().fail_syncs = true;
    }

    /// Lets `n` more writes and resizes through, then fails every operation,
    /// as a power cut would stop them.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Storage};
    ///
    /// let mut s = MemoryStorage::with_crash_tracking();
    /// s.fail_after(1);
    /// s.set_len(4)?;
    /// assert!(s.write_at(b"x", 0).is_err());
    /// assert!(s.sync().is_err());
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn fail_after(&self, n: u64) {
        self.inner.lock().unwrap().writes_left = Some(n);
    }
}

impl Storage for MemoryStorage {
    fn len(&self) -> io::Result<u64> {
        Ok(self.inner.lock().unwrap().current.len() as u64)
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let m = self.inner.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        if end > m.current.len() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.copy_from_slice(&m.current[start..end]);
        Ok(())
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        let mut m = self.inner.lock().unwrap();
        m.dead()?;
        let start = offset as usize;
        let end = start + buf.len();
        if end > m.current.len() {
            return Err(io::Error::other("write past the end of a MemoryStorage"));
        }
        m.current[start..end].copy_from_slice(buf);
        if m.track {
            m.since_sync.push(Op::Write {
                offset,
                bytes: buf.to_vec(),
            });
        }
        Ok(())
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        let mut m = self.inner.lock().unwrap();
        m.dead()?;
        m.current.resize(len as usize, 0);
        if m.track {
            m.since_sync.push(Op::SetLen(len));
        }
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        let mut m = self.inner.lock().unwrap();
        if m.fail_syncs || m.writes_left == Some(0) {
            return Err(io::Error::other("injected fsync failure"));
        }
        if m.track {
            m.durable = m.current.clone();
            m.since_sync.clear();
        }
        Ok(())
    }
}
