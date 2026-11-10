//! An archive whose directory has been read once, so any number of its files can be read without
//! reading the directory again. Each file is read with requests for only its own bytes.
//!
//! [`RemoteArchive::open`] fetches the end of the file and the directory. After that,
//! [`RemoteArchive::open_entry`] reads one entry: a first request of at most 64 KiB checks its
//! local header (and holds all of a small file), then one streamed request runs to the exact end
//! of its data. The returned [`EntryReader`] decompresses and verifies the data as it's read.
//!
//! `RemoteArchive` is `Send + Sync`, so it can be shared between tasks in an [`Arc`], and an
//! `EntryReader` doesn't borrow it.
//!
//! ```
//! fn assert_send_sync<T: Send + Sync + 'static>() {}
//! assert_send_sync::<lazy_zip_core::archive::RemoteArchive>();
//! ```

use crate::error::{Error, Result};
use crate::http_reader::{ReaderConfig, RemoteFile, RemoteHttpReader};
use async_compression::tokio::bufread::{
    BzDecoder, Deflate64Decoder, DeflateDecoder, XzDecoder, ZstdDecoder,
};
// Deprecated in favour of the new `read1` module, which is a different API. Moving to it means
// re-checking every limit and test that depends on how the directory is parsed.
#[allow(deprecated)]
use async_zip::base::read::seek::ZipFileReader;
use async_zip::{Compression, StoredZipEntry, ZipString};
use std::cmp::min;
use std::collections::{HashMap, HashSet};
use std::io::SeekFrom;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncReadExt, AsyncSeekExt, BufReader, Take};
use tokio_util::compat::TokioAsyncReadCompatExt;

/// Deflate can't compress better than about 1032:1, so a higher claimed ratio is a lie.
const MAX_COMPRESSION_RATIO: u64 = 1100;
const RATIO_CHECK_MIN_BYTES: u64 = 1024 * 1024;

const LOCAL_HEADER_SIGNATURE: &[u8] = b"PK\x03\x04";
const LOCAL_HEADER_LENGTH: u64 = 30;
/// The first request for an entry: its local header, and all of a small file.
const FIRST_READ_BYTES: u64 = 64 * 1024;
const ENCRYPTED_FLAG: u16 = 0x0001;
const ENTRY_BUFFER_BYTES: usize = 64 * 1024;

/// One entry of the archive's directory, as the directory describes it. Nothing here has been
/// checked against the entry's data yet; [`RemoteArchive::open_entry`] does that.
#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    name: ZipString,
    /// The uncompressed size the directory claims.
    pub size: u64,
    /// The size of the stored data.
    pub compressed_size: u64,
    /// The CRC-32 of the uncompressed data.
    pub crc: u32,
    /// The compression method.
    pub compression: Compression,
    /// Where the entry's local header starts in the archive.
    pub header_offset: u64,
    /// Whether the entry is a directory, by its attributes or a trailing `/`.
    pub is_dir: bool,
    /// The modification time as stored, in local time: year, month, day, hour, minute, second.
    pub date_time: (u16, u8, u8, u8, u8, u8),
    /// The external file attributes, such as Unix permissions in the high 16 bits.
    pub external_attributes: u32,
}

impl ArchiveEntry {
    fn from_stored(entry: &StoredZipEntry) -> Self {
        let name = entry.filename().clone();
        let is_dir = entry.dir().unwrap_or(false) || name.as_bytes().ends_with(b"/");
        let date = entry.last_modification_date();
        Self {
            name,
            size: entry.uncompressed_size(),
            compressed_size: entry.compressed_size(),
            crc: entry.crc32(),
            compression: entry.compression(),
            header_offset: entry.header_offset(),
            is_dir,
            date_time: (
                date.year() as u16,
                date.month() as u8,
                date.day() as u8,
                date.hour() as u8,
                date.minute() as u8,
                date.second() as u8,
            ),
            external_attributes: entry.external_file_attribute(),
        }
    }

    /// The ZIP method number, such as 8 for Deflate.
    pub fn compression_method(&self) -> u16 {
        u16::from(&self.compression)
    }

    /// The entry's name, if it's valid UTF-8.
    pub fn name(&self) -> Option<&str> {
        self.name.as_str().ok()
    }

    /// The name for display, with invalid UTF-8 replaced.
    pub fn lossy_name(&self) -> String {
        String::from_utf8_lossy(self.name.as_bytes()).into_owned()
    }

    /// The name exactly as stored in the directory, which the local header must repeat. It's the
    /// only way to address a name that isn't valid UTF-8.
    pub fn raw_name(&self) -> &[u8] {
        self.name
            .alternative()
            .unwrap_or_else(|| self.name.as_bytes())
    }
}

/// What an entry's directory record promises, checked as its data is read. This keeps a zip bomb
/// from writing more than the size shown to the user.
#[derive(Clone, Copy, Debug)]
struct EntryCheck {
    size: u64,
    crc: u32,
}

impl EntryCheck {
    fn new(entry: &ArchiveEntry, path: &str) -> Result<Self> {
        let (size, compressed) = (entry.size, entry.compressed_size);
        let suspicious = |reason| Error::SuspiciousEntry {
            path: path.to_string(),
            reason,
        };
        // ZIP's LZMA framing differs from what the decoder expects, so it would only ever fail.
        if entry.compression == Compression::Lzma {
            return Err(Error::UnsupportedCompression {
                path: path.to_string(),
                method: "LZMA",
            });
        }
        if entry.compression == Compression::Stored && size != compressed {
            return Err(suspicious("an uncompressed file has two different sizes"));
        }
        if size >= RATIO_CHECK_MIN_BYTES && size / compressed.max(1) > MAX_COMPRESSION_RATIO {
            return Err(suspicious("it claims an impossible compression ratio"));
        }
        Ok(Self {
            size,
            crc: entry.crc,
        })
    }

    /// Reading one byte more than promised is enough to detect a larger entry.
    fn read_limit(&self) -> u64 {
        self.size.saturating_add(1)
    }
}

type Decoder = Pin<Box<dyn AsyncRead + Send>>;

fn decoder<R>(reader: R, compression: Compression, path: &str) -> Result<Decoder>
where
    R: AsyncBufRead + Send + 'static,
{
    Ok(match compression {
        Compression::Stored => Box::pin(reader),
        Compression::Deflate => Box::pin(DeflateDecoder::new(reader)),
        Compression::Deflate64 => Box::pin(Deflate64Decoder::new(reader)),
        Compression::Bz => Box::pin(BzDecoder::new(reader)),
        Compression::Zstd => Box::pin(ZstdDecoder::new(reader)),
        Compression::Xz => Box::pin(XzDecoder::new(reader)),
        _ => {
            return Err(Error::UnsupportedCompression {
                path: path.to_string(),
                method: "unknown",
            })
        }
    })
}

/// An entry's decompressed data. Reads fail as soon as the data breaks a promise the directory
/// made, and the final read checks the size and checksum.
///
/// Data returned before the final read hasn't been verified yet, so don't act on it until
/// [`read`](Self::read) has returned 0. Made by [`RemoteArchive::open_entry`].
pub struct EntryReader {
    inner: Take<Decoder>,
    check: EntryCheck,
    hasher: crc32fast::Hasher,
    total: u64,
    path: String,
}

impl EntryReader {
    /// Reads into `buf`, returning 0 only once the whole entry has been read and verified.
    ///
    /// # Errors
    ///
    /// [`Error::SizeMismatch`] or [`Error::CrcMismatch`] if the data doesn't match the directory,
    /// and network or decompression errors as they happen.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = self.inner.read(buf).await?;
        if n == 0 {
            if self.total != self.check.size {
                return Err(self.size_mismatch());
            }
            if self.hasher.clone().finalize() != self.check.crc {
                return Err(Error::CrcMismatch {
                    path: self.path.clone(),
                });
            }
            return Ok(0);
        }
        self.total += n as u64;
        if self.total > self.check.size {
            return Err(self.size_mismatch());
        }
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn size_mismatch(&self) -> Error {
        Error::SizeMismatch {
            path: self.path.clone(),
            expected: self.check.size,
            actual: self.total,
        }
    }
}

/// An archive's directory, read once and shared by every file read from it.
///
/// It keeps the end of the file that was fetched to open it, so reading an entry stored there
/// makes no requests.
///
/// ```no_run
/// use lazy_zip_core::archive::RemoteArchive;
/// use lazy_zip_core::http_reader::ReaderConfig;
///
/// # async fn example() -> lazy_zip_core::Result<()> {
/// let archive = RemoteArchive::open("https://example.com/data.zip", ReaderConfig::new()?).await?;
/// for entry in archive.entries().iter().filter(|entry| !entry.is_dir) {
///     println!("{} {}", entry.lossy_name(), entry.size);
/// }
/// # Ok(())
/// # }
/// ```
pub struct RemoteArchive {
    file: RemoteFile,
    entries: Vec<ArchiveEntry>,
    /// Every distinct local header offset, sorted. An entry's data must end before the next one.
    header_offsets: Vec<u64>,
    /// Local header offsets claimed by more than one entry.
    shared_offsets: HashSet<u64>,
    /// Bytes fetched to read the directory.
    fetched_size: u64,
}

impl RemoteArchive {
    /// Fetches the end of the archive at `url` and reads its directory, usually with two requests.
    ///
    /// Every later request uses `config`'s client, headers and retry policy.
    ///
    /// # Errors
    ///
    /// Fails if the server doesn't support Range requests, the file isn't a ZIP archive, its
    /// directory breaks the limits in [`crate::archive_limits`], or an entry uses an unknown
    /// compression method.
    pub async fn open(url: &str, config: ReaderConfig) -> Result<Self> {
        Self::from_reader(RemoteHttpReader::open(url, config).await?).await
    }

    /// Reads the directory through `reader`, which must not have been read from yet.
    #[allow(deprecated)]
    pub async fn from_reader(mut reader: RemoteHttpReader) -> Result<Self> {
        let fetch_limit = reader.fetch_limit();
        let zip = ZipFileReader::new(BufReader::new(&mut reader).compat()).await?;
        fetch_limit.lift();

        let entries: Vec<ArchiveEntry> = zip
            .file()
            .entries()
            .iter()
            .map(ArchiveEntry::from_stored)
            .collect();
        drop(zip);

        let mut header_offsets: Vec<u64> = entries.iter().map(|e| e.header_offset).collect();
        header_offsets.sort_unstable();
        let shared_offsets = header_offsets
            .windows(2)
            .filter(|pair| pair[0] == pair[1])
            .map(|pair| pair[0])
            .collect();
        header_offsets.dedup();

        Ok(Self {
            file: reader.file().clone(),
            entries,
            header_offsets,
            shared_offsets,
            fetched_size: reader.get_network_bytes(),
        })
    }

    /// Every entry, in directory order. An index into this slice identifies an entry.
    pub fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
    }

    /// The URL the archive was opened from.
    pub fn url(&self) -> &str {
        self.file.url()
    }

    /// The archive's size in bytes.
    pub fn content_length(&self) -> u64 {
        self.file.len()
    }

    /// Bytes fetched to open the archive and read its directory.
    pub fn fetched_size(&self) -> u64 {
        self.fetched_size
    }

    /// The counter of the config the archive was opened with.
    pub fn network_counter(&self) -> Arc<AtomicU64> {
        self.file.network_counter()
    }

    /// The index of the entry named `path`. The last duplicate wins, matching the listing.
    pub fn find(&self, path: &str) -> Option<usize> {
        self.entries.iter().rposition(|e| e.name() == Some(path))
    }

    /// Like [`Self::find`], by the name's raw bytes, which needn't be valid UTF-8.
    pub fn find_raw(&self, name: &[u8]) -> Option<usize> {
        self.entries.iter().rposition(|e| e.raw_name() == name)
    }

    /// The index of each of `paths` that's in the archive, found in one pass over the directory.
    pub fn find_all<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a str>,
    ) -> HashMap<&'a str, usize> {
        let wanted: HashMap<&str, &'a str> = paths.into_iter().map(|p| (p, p)).collect();
        let mut found = HashMap::with_capacity(wanted.len());
        for (index, entry) in self.entries.iter().enumerate() {
            if let Some(&path) = entry.name().and_then(|name| wanted.get(name)) {
                found.insert(path, index);
            }
        }
        found
    }

    /// Where the data starting at `offset` must end: the next local header, or the directory.
    fn data_boundary(&self, offset: u64) -> u64 {
        let next = self.header_offsets.partition_point(|&o| o <= offset);
        let directory = self.file.directory_offset();
        self.header_offsets
            .get(next)
            .map_or(directory, |&o| min(o, directory))
    }

    /// Opens the entry at `index` for reading, counting network use in `network_bytes`.
    ///
    /// Like Python's `zipfile`, this refuses entries whose local header names a different file or
    /// whose data overlaps another entry or the directory, which is how overlapping zip bombs work.
    ///
    /// The entry's name isn't checked for safety as a file path. Use
    /// [`safe_path::destination`](crate::safe_path::destination) before writing it to disk.
    ///
    /// # Errors
    ///
    /// [`Error::FileNotFound`] for an index past the end, [`Error::SuspiciousEntry`],
    /// [`Error::Encrypted`] or [`Error::UnsupportedCompression`] for refused entries, and network
    /// errors.
    pub async fn open_entry(
        &self,
        index: usize,
        network_bytes: Arc<AtomicU64>,
    ) -> Result<EntryReader> {
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| Error::FileNotFound(index.to_string()))?;
        let path = entry.lossy_name();
        let check = EntryCheck::new(entry, &path)?;
        let overlaps = || Error::SuspiciousEntry {
            path: path.clone(),
            reason: "its data overlaps another file",
        };

        let start = entry.header_offset;
        let boundary = self.data_boundary(start);
        if self.shared_offsets.contains(&start)
            || start.saturating_add(LOCAL_HEADER_LENGTH + entry.compressed_size) > boundary
        {
            return Err(overlaps());
        }

        // A small file comes in this first request. A larger one waits until its header has been
        // checked, so a refused file costs little.
        let mut reader = self.file.reader_counting(network_bytes);
        reader.limit_to(min(boundary, start.saturating_add(FIRST_READ_BYTES)));
        reader.seek(SeekFrom::Start(start)).await?;
        let mut reader = BufReader::with_capacity(ENTRY_BUFFER_BYTES, reader);

        let mut header = [0u8; LOCAL_HEADER_LENGTH as usize];
        reader.read_exact(&mut header).await?;
        if &header[..4] != LOCAL_HEADER_SIGNATURE {
            return Err(Error::Zip(format!("{path} has no local header")));
        }
        let flags = u16::from_le_bytes([header[6], header[7]]);
        let name_length = u64::from(u16::from_le_bytes([header[26], header[27]]));
        let extra_length = u64::from(u16::from_le_bytes([header[28], header[29]]));
        let data_start = start + LOCAL_HEADER_LENGTH + name_length + extra_length;
        if data_start.saturating_add(entry.compressed_size) > boundary {
            return Err(overlaps());
        }
        if flags & ENCRYPTED_FLAG != 0 {
            return Err(Error::Encrypted { path });
        }
        reader.get_mut().limit_to(data_start);

        let mut local_name = vec![0u8; name_length as usize];
        reader.read_exact(&mut local_name).await?;
        if local_name != entry.raw_name() {
            return Err(Error::SuspiciousEntry {
                path,
                reason: "its local header names a different file",
            });
        }
        tokio::io::copy(
            &mut (&mut reader).take(extra_length),
            &mut tokio::io::sink(),
        )
        .await?;
        reader
            .get_mut()
            .limit_to(data_start + entry.compressed_size);

        let data = decoder(reader.take(entry.compressed_size), entry.compression, &path)?;
        Ok(EntryReader {
            inner: data.take(check.read_limit()),
            check,
            hasher: crc32fast::Hasher::new(),
            total: 0,
            path,
        })
    }
}
