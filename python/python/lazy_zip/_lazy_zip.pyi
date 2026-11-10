"""The native part of `lazy_zip`. Import from `lazy_zip` instead."""

import os
import zipfile
from types import TracebackType
from typing import (
    Dict,
    Iterable,
    List,
    Mapping,
    Optional,
    Sequence,
    Tuple,
    Type,
    TypeVar,
    Union,
)

_Self = TypeVar("_Self", bound="RemoteZip")
_Member = Union[str, "ZipInfo"]
_Path = Union[str, "os.PathLike[str]"]
_Headers = Union[Mapping[str, str], Iterable[Tuple[str, str]]]
_Timeout = Union[float, Tuple[float, float]]

class BadZipFile(zipfile.BadZipFile):
    """The archive or one of its members is corrupt, looks malicious, or can't be written safely.

    A subclass of `zipfile.BadZipFile`.
    """

    kind: str
    """One of `"not_a_zip"`, `"limit_exceeded"`, `"corrupt"`, `"suspicious"` or `"unsafe"`."""

class ExtractError(Exception):
    """Raised by `RemoteZip.extractall()` when some members couldn't be extracted."""

    failures: Dict[str, Exception]
    """Each failed member name and its exception, which has a `kind` attribute."""

class FileEntry:
    """One file or folder of the archive's tree, from `RemoteZip.files`."""

    @property
    def name(self) -> str:
        """The last part of the path, such as `readme.txt`."""
    @property
    def path(self) -> str:
        """A file's member name, or a folder's path without a trailing slash."""
    @property
    def size(self) -> int:
        """The file's uncompressed size in bytes. 0 for folders."""
    @property
    def compressed_size(self) -> int:
        """The file's compressed size in bytes. 0 for folders."""
    @property
    def is_dir(self) -> bool:
        """Whether this is a folder."""
    @property
    def children(self) -> List["FileEntry"]:
        """A folder's contents, sorted by name. Empty for files."""
    @property
    def parent_path(self) -> Optional[str]:
        """The containing folder's `path`, or `None` at the top level."""

class ZipInfo:
    """Information about one member of the archive, like `zipfile.ZipInfo`."""

    @property
    def filename(self) -> str:
        """The member's name: UTF-8 names as stored, other names decoded as cp437."""
    @property
    def orig_filename(self) -> str:
        """The same as `filename`: names aren't normalized."""
    @property
    def date_time(self) -> Tuple[int, int, int, int, int, int]:
        """The modification time as stored (local time): year, month, day, hour, minute, second."""
    @property
    def file_size(self) -> int:
        """The uncompressed size in bytes."""
    @property
    def compress_size(self) -> int:
        """The compressed size in bytes."""
    @property
    def CRC(self) -> int:
        """The CRC-32 of the uncompressed data."""
    @property
    def compress_type(self) -> int:
        """0 stored, 8 deflate, 9 deflate64, 12 bzip2, 14 LZMA (can't be read), 93 zstd, 95 xz."""
    @property
    def external_attr(self) -> int:
        """Host-dependent file attributes. On Unix archives, the high 16 bits are the mode."""
    @property
    def header_offset(self) -> int:
        """Where the member's local header starts in the archive, in bytes."""
    def is_dir(self) -> bool:
        """Returns whether the member is a folder."""

class EntryStream:
    """The raw, unbuffered data of one member. Use `RemoteZip.open()`, which buffers it."""

    def read(self, size: int = -1) -> bytes:
        """Reads up to `size` bytes (64 KiB when negative, at most 1 MiB)."""
    def seek(self, offset: int, whence: int = 0) -> int:
        """Moves to a new position, clamped to the member, and returns it."""
    def tell(self) -> int:
        """Returns the current position in the uncompressed data."""
    @property
    def size(self) -> int:
        """The member's uncompressed size in bytes."""
    @property
    def closed(self) -> bool:
        """Whether `close()` has been called."""
    def close(self) -> None:
        """Closes the stream and its connection."""

class RemoteZip:
    """A remote ZIP archive, read with HTTP range requests. Use `lazy_zip.RemoteZip`."""

    def __new__(
        cls: Type[_Self],
        url: str,
        headers: Optional[_Headers] = None,
        timeout: Optional[_Timeout] = None,
    ) -> _Self: ...
    def __enter__(self: _Self) -> _Self:
        """Returns the archive itself, for use in a `with` block."""
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[TracebackType],
    ) -> None:
        """Closes the archive at the end of a `with` block."""
    @property
    def url(self) -> str:
        """The URL the archive was opened with, exactly as given."""
    @property
    def content_length(self) -> int:
        """The archive's size in bytes, as reported by the server."""
    @property
    def network_bytes(self) -> int:
        """Bytes received from the server so far by this archive, for every request."""
    @property
    def files(self) -> List[FileEntry]:
        """The top level of the archive as a tree, sorted by name. Unsafe names are left out."""
    def namelist(self) -> List[str]:
        """Returns the members' names, in archive order."""
    def infolist(self) -> List[ZipInfo]:
        """Returns a `ZipInfo` for each member, in archive order."""
    def getinfo(self, name: str) -> ZipInfo:
        """Returns the `ZipInfo` of the member called `name` (the last one, if repeated)."""
    def read(self, name: _Member) -> bytes:
        """Reads a member into memory, checking its size and CRC."""
    def _open_stream(self, name: _Member) -> EntryStream:
        """Opens a member as an unbuffered `EntryStream`. Use `open()`."""
    def extract(self, member: _Member, path: Optional[_Path] = None) -> str:
        """Extracts a member under `path` and returns where it was written."""
    def extractall(
        self,
        path: Optional[_Path] = None,
        members: Optional[Sequence[_Member]] = None,
        workers: int = 8,
    ) -> None:
        """Extracts members under `path`, `workers` at a time. Raises `ExtractError` on failures."""
    def testzip(self) -> Optional[str]:
        """Reads every member and returns the name of the first corrupt one, or `None`."""
    def read_to_file(self, path: _Member, filepath: _Path) -> None:
        """Streams a member to exactly `filepath`, replacing it only with verified data."""
    def load(self) -> None:
        """Builds the tree behind `files` now, rather than on first use."""
    def close(self) -> None:
        """Releases the archive. Files already opened with `open()` stay readable."""
