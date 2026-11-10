//! Checks an archive's end records before they're parsed, because the parser allocates from the
//! entry count they claim. A small hostile file could otherwise abort the whole process.
//!
//! These records are always read from the bytes fetched when the archive is opened, so a server
//! can't show this check one thing and the parser another.

use crate::error::{Error, Result};

/// The most data read while parsing the directory, and the largest directory accepted.
pub const MAX_DIRECTORY_BYTES: u64 = 512 * 1024 * 1024;
/// The most entries an archive may claim.
pub const MAX_ENTRIES: u64 = 5_000_000;

const EOCD_SIGNATURE: &[u8] = b"PK\x05\x06";
const EOCD_LENGTH: usize = 22;
const MAX_COMMENT_LENGTH: usize = u16::MAX as usize;
const ZIP64_LOCATOR_SIGNATURE: &[u8] = b"PK\x06\x07";
const ZIP64_LOCATOR_LENGTH: usize = 20;
const ZIP64_RECORD_LENGTH: usize = 56;
const MIN_DIRECTORY_RECORD_LENGTH: u64 = 46;
/// A record's name, extra field and comment are each at most `u16::MAX` bytes long.
const MAX_DIRECTORY_RECORD_LENGTH: u64 = MIN_DIRECTORY_RECORD_LENGTH + 3 * u16::MAX as u64;

/// The parser searches backwards in steps this long, so it can look this far past the records.
const PARSER_SEARCH_STEP: usize = 2048;

/// Enough of the end of a file to contain all of the end records and the parser's search for them.
/// Opening an archive fetches this much of its end, about 66 KiB.
pub const TAIL_BYTES: u64 = (EOCD_LENGTH
    + MAX_COMMENT_LENGTH
    + ZIP64_LOCATOR_LENGTH
    + ZIP64_RECORD_LENGTH
    + PARSER_SEARCH_STEP) as u64;

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut value = [0; 8];
    value.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(value)
}

/// The directory as claimed by end records that passed the checks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DirectoryClaim {
    pub offset: u64,
    pub size: u64,
    pub entries: u64,
}

impl DirectoryClaim {
    /// Where the directory's records end, if the claim is true. A size larger than the claimed
    /// number of records could fill is ignored.
    pub fn end(&self) -> u64 {
        let most = self.entries.saturating_mul(MAX_DIRECTORY_RECORD_LENGTH);
        self.offset.saturating_add(self.size.min(most))
    }
}

/// Validates the directory claimed by `tail`, the last bytes of a file `file_len` bytes long, and
/// returns where it starts.
///
/// `tail` should be the last [`TAIL_BYTES`] of the file, or all of it if it's shorter.
///
/// # Errors
///
/// [`Error::NotAZip`] if there's no usable end record, and [`Error::LimitExceeded`] if the
/// records claim more than [`MAX_ENTRIES`] entries or a directory over [`MAX_DIRECTORY_BYTES`].
///
/// # Examples
///
/// ```
/// use lazy_zip_core::archive_limits::check_directory;
/// use lazy_zip_core::ErrorKind;
///
/// // An end of central directory record, without a comment.
/// fn end_record(entries: u16, directory_size: u32, directory_offset: u32) -> Vec<u8> {
///     let mut record = b"PK\x05\x06".to_vec();
///     record.extend_from_slice(&[0; 4]); // disk numbers
///     record.extend_from_slice(&entries.to_le_bytes()); // entries on this disk
///     record.extend_from_slice(&entries.to_le_bytes()); // entries in total
///     record.extend_from_slice(&directory_size.to_le_bytes());
///     record.extend_from_slice(&directory_offset.to_le_bytes());
///     record.extend_from_slice(&0u16.to_le_bytes()); // comment length
///     record
/// }
///
/// // The last 42 bytes of a 168-byte archive whose one-entry directory starts at byte 100.
/// let mut tail = vec![0u8; 20];
/// tail.extend_from_slice(&end_record(1, 46, 100));
/// assert_eq!(check_directory(&tail, 168).unwrap(), 100);
///
/// // A directory of 600 MiB is refused before anything allocates it.
/// let mut tail = vec![0u8; 20];
/// tail.extend_from_slice(&end_record(1, 600 << 20, 100));
/// let error = check_directory(&tail, 1 << 30).unwrap_err();
/// assert_eq!(error.kind(), ErrorKind::LimitExceeded);
/// ```
pub fn check_directory(tail: &[u8], file_len: u64) -> Result<u64> {
    check_end_records(tail, file_len).map(|claim| claim.offset)
}

/// Like [`check_directory`], returning everything the end records claim about the directory.
pub(crate) fn check_end_records(tail: &[u8], file_len: u64) -> Result<DirectoryClaim> {
    // The parser takes the last signature in the file, so this must too.
    let eocd = tail
        .windows(EOCD_SIGNATURE.len())
        .rposition(|window| window == EOCD_SIGNATURE)
        .ok_or(Error::NotAZip)?;
    if eocd + EOCD_LENGTH > tail.len() {
        return Err(Error::NotAZip);
    }

    let tail_start = file_len
        .checked_sub(tail.len() as u64)
        .ok_or(Error::InvalidResponse("more data than the file's size"))?;
    let eocd_position = tail_start + eocd as u64;

    let mut entries = u64::from(u16_at(tail, eocd + 10));
    let mut directory_size = u64::from(u32_at(tail, eocd + 12));
    let mut directory_offset = u64::from(u32_at(tail, eocd + 16));

    if eocd_position >= ZIP64_LOCATOR_LENGTH as u64 {
        let Some(locator) = eocd.checked_sub(ZIP64_LOCATOR_LENGTH) else {
            // Only a comment longer than the format allows could put the locator out of reach.
            return Err(Error::NotAZip);
        };

        if &tail[locator..locator + 4] == ZIP64_LOCATOR_SIGNATURE {
            let record = u64_at(tail, locator + 8)
                .checked_sub(tail_start)
                .map(|offset| offset as usize)
                .filter(|offset| offset.saturating_add(ZIP64_RECORD_LENGTH) <= tail.len())
                .ok_or(Error::LimitExceeded("its ZIP64 record is out of place"))?;

            if u16_at(tail, eocd + 10) == u16::MAX {
                entries = u64_at(tail, record + 32);
            }
            if u32_at(tail, eocd + 12) == u32::MAX {
                directory_size = u64_at(tail, record + 40);
            }
            if u32_at(tail, eocd + 16) == u32::MAX {
                directory_offset = u64_at(tail, record + 48);
            }
        }
    }

    if entries > MAX_ENTRIES {
        return Err(Error::LimitExceeded("it claims too many entries"));
    }
    if directory_size > MAX_DIRECTORY_BYTES {
        return Err(Error::LimitExceeded("its directory is too large"));
    }
    if entries.saturating_mul(MIN_DIRECTORY_RECORD_LENGTH) > directory_size
        || directory_offset > file_len
    {
        return Err(Error::NotAZip);
    }
    Ok(DirectoryClaim {
        offset: directory_offset,
        size: directory_size,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eocd(entries: u16, size: u32, offset: u32, comment: &[u8]) -> Vec<u8> {
        let mut bytes = EOCD_SIGNATURE.to_vec();
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&size.to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
        bytes.extend_from_slice(&(comment.len() as u16).to_le_bytes());
        bytes.extend_from_slice(comment);
        bytes
    }

    /// A real tail always includes the bytes where a ZIP64 locator would be.
    fn with_preceding_bytes(record: Vec<u8>) -> Vec<u8> {
        let mut tail = vec![0u8; ZIP64_LOCATOR_LENGTH];
        tail.extend_from_slice(&record);
        tail
    }

    fn zip64_end(entries: u64, size: u64, offset: u64, record_offset: u64) -> Vec<u8> {
        let mut bytes = b"PK\x06\x06".to_vec();
        bytes.extend_from_slice(&44u64.to_le_bytes());
        bytes.extend_from_slice(&[0; 12]);
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&size.to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
        bytes.extend_from_slice(ZIP64_LOCATOR_SIGNATURE);
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&record_offset.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&eocd(u16::MAX, u32::MAX, u32::MAX, b""));
        bytes
    }

    #[test]
    fn accepts_an_ordinary_end_record() {
        let tail = with_preceding_bytes(eocd(2, 100, 0, b""));
        assert!(check_directory(&tail, 1000).is_ok());
        assert!(check_directory(&eocd(0, 0, 0, b""), 22).is_ok());
    }

    #[test]
    fn returns_where_the_directory_starts() {
        let tail = with_preceding_bytes(eocd(2, 100, 500, b""));
        assert_eq!(check_directory(&tail, 1000).unwrap(), 500);

        let file_len = 10_000_000u64;
        let record_offset = file_len - (56 + 20 + 22);
        let tail = zip64_end(300_000, 22_000_000, 7_000_000, record_offset);
        assert_eq!(check_directory(&tail, file_len).unwrap(), 7_000_000);
    }

    #[test]
    fn the_claimed_directory_ends_where_its_records_can_reach() {
        let tail = with_preceding_bytes(eocd(2, 100, 500, b""));
        let claim = check_end_records(&tail, 1000).unwrap();
        assert_eq!(
            claim,
            DirectoryClaim {
                offset: 500,
                size: 100,
                entries: 2
            }
        );
        assert_eq!(claim.end(), 600);

        let tail = with_preceding_bytes(eocd(2, 8_000_000, 0, b""));
        let claim = check_end_records(&tail, 9_000_000).unwrap();
        assert_eq!(claim.end(), 2 * MAX_DIRECTORY_RECORD_LENGTH);

        let empty = check_end_records(&eocd(0, 0, 0, b""), 22).unwrap();
        assert_eq!(empty.end(), 0);
    }

    #[test]
    fn accepts_a_maximum_length_comment() {
        let tail = with_preceding_bytes(eocd(1, 46, 0, &vec![b'x'; MAX_COMMENT_LENGTH]));
        assert!(check_directory(&tail, tail.len() as u64 + 100).is_ok());
    }

    #[test]
    fn rejects_data_with_no_end_record() {
        assert!(matches!(
            check_directory(b"this is not a zip", 17),
            Err(Error::NotAZip)
        ));
        assert!(matches!(check_directory(b"", 0), Err(Error::NotAZip)));
        assert!(matches!(
            check_directory(EOCD_SIGNATURE, 4),
            Err(Error::NotAZip)
        ));
    }

    #[test]
    fn rejects_more_entries_than_the_directory_can_hold() {
        assert!(matches!(
            check_directory(&with_preceding_bytes(eocd(u16::MAX, 100, 0, b"")), 1000),
            Err(Error::NotAZip)
        ));
    }

    #[test]
    fn rejects_a_directory_starting_past_the_end_of_the_file() {
        assert!(matches!(
            check_directory(&with_preceding_bytes(eocd(1, 46, 5000, b"")), 1000),
            Err(Error::NotAZip)
        ));
    }

    #[test]
    fn accepts_a_realistic_zip64_archive() {
        let file_len = 10_000_000u64;
        let tail_len = 56 + 20 + 22;
        let record_offset = file_len - tail_len;
        let tail = zip64_end(300_000, 22_000_000, 0, record_offset);
        assert_eq!(tail.len() as u64, tail_len);
        assert!(check_directory(&tail, file_len).is_ok());
    }

    #[test]
    fn rejects_a_huge_zip64_entry_count_without_allocating() {
        let file_len = 1000u64;
        let record_offset = file_len - (56 + 20 + 22);
        let tail = zip64_end(u64::MAX, 1000, 0, record_offset);
        assert!(matches!(
            check_directory(&tail, file_len),
            Err(Error::LimitExceeded(_))
        ));
    }

    #[test]
    fn rejects_a_huge_zip64_directory() {
        let file_len = u64::MAX / 2;
        let record_offset = file_len - (56 + 20 + 22);
        let tail = zip64_end(10, MAX_DIRECTORY_BYTES + 1, 0, record_offset);
        assert!(matches!(
            check_directory(&tail, file_len),
            Err(Error::LimitExceeded(_))
        ));
    }

    #[test]
    fn rejects_a_zip64_record_the_check_cannot_see() {
        // The server could otherwise send the parser a different record later.
        let tail = zip64_end(10, 1000, 0, 5);
        assert!(matches!(
            check_directory(&tail, 1_000_000),
            Err(Error::LimitExceeded(_))
        ));
        let tail = zip64_end(10, 1000, 0, u64::MAX);
        assert!(matches!(
            check_directory(&tail, 1_000_000),
            Err(Error::LimitExceeded(_))
        ));
    }

    #[test]
    fn rejects_an_end_record_too_far_from_the_end_of_the_file() {
        let mut tail = vec![0u8; 0];
        tail.extend_from_slice(&eocd(1, 46, 0, b""));
        assert!(matches!(
            check_directory(&tail, tail.len() as u64 + 100),
            Err(Error::NotAZip)
        ));
    }

    #[test]
    fn tail_is_long_enough_for_every_record() {
        assert_eq!(TAIL_BYTES, 22 + 65535 + 20 + 56 + 2048);
    }
}
