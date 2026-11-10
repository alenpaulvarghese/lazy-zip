"""Read files from remote ZIP archives over HTTP range requests, like `zipfile.ZipFile`.

Opening a `RemoteZip` downloads only the archive's directory, usually in one or two requests.
After that, each member costs at most two requests and about its compressed size, so you can
list a huge archive and pull out a few files without downloading the rest.

Example:
    >>> import lazy_zip
    >>> with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:  # doctest: +SKIP
    ...     print(archive.namelist())
    ...     data = archive.read("docs/readme.txt")
    ...     archive.extractall("out", workers=8)

Errors:
    Exceptions raised for a failure have a `kind` attribute (a `str`) that says what went
    wrong, and the exception type follows from it:

    - `ConnectionError`: `"network"` (connection failed, timed out or the response was
      unusable), `"http_status"` (the server answered with an error status),
      `"range_not_supported"`, `"remote_changed"` (the file changed on the server).
    - `BadZipFile`, a subclass of `zipfile.BadZipFile`: `"not_a_zip"`, `"limit_exceeded"`,
      `"corrupt"`, `"suspicious"` (crafted to cause harm, such as overlapping data),
      `"unsafe"` (a name that can't be written safely).
    - `NotImplementedError`: `"unsupported"` (encrypted, or an unsupported compression method).
    - `KeyError`: `"not_found"` (no member has that name).
    - `ValueError`: `"invalid_header"` (a header in `headers` is invalid).
    - `OSError`: `"io"` (a local file couldn't be written) and `"cancelled"`.

    `ExtractError` collects the failures of `RemoteZip.extractall()`. Using a closed archive,
    or giving an invalid timeout, raises `ValueError` without a `kind`.

Threads, the GIL and Ctrl-C:
    Every call that waits for the network releases the GIL, so other threads keep running.
    One `RemoteZip` can be used from several threads at once. Give each thread its own
    `ZipExtFile` from `open()`. On the main thread, Ctrl-C raises `KeyboardInterrupt` within
    about 0.1 seconds and stops the work, removing partly written files first.
"""

import io
from typing import Optional, TextIO, Union

from . import _lazy_zip
from ._lazy_zip import BadZipFile, EntryStream, ExtractError, FileEntry, ZipInfo

__all__ = ["BadZipFile", "ExtractError", "FileEntry", "RemoteZip", "ZipExtFile", "ZipInfo"]

_BUFFER_SIZE = 64 * 1024


class _RawEntry(io.RawIOBase):
    """Adapts an `EntryStream` to `io.RawIOBase`, so `io.BufferedReader` can wrap it."""

    def __init__(self, stream: EntryStream) -> None:
        super().__init__()
        self._stream = stream

    def readable(self) -> bool:
        return True

    def seekable(self) -> bool:
        return True

    def readinto(self, buffer) -> int:  # type: ignore[override]
        view = memoryview(buffer).cast("B")
        data = self._stream.read(len(view))
        view[: len(data)] = data
        return len(data)

    def seek(self, offset: int, whence: int = io.SEEK_SET) -> int:
        return self._stream.seek(offset, whence)

    def tell(self) -> int:
        return self._stream.tell()

    def close(self) -> None:
        if not self.closed:
            self._stream.close()
        super().close()


class ZipExtFile(io.BufferedReader):
    """A member opened for reading with `RemoteZip.open()`. A read-only, binary file object.

    It supports everything `io.BufferedReader` does: `read()`, `read1()`, `readinto()`,
    `readline()`, iteration over lines, `peek()`, `seek()` and `tell()`, and `with` blocks.
    Data is downloaded as it's read, 64 KiB at a time, so a large member isn't held in memory.
    Wrap it in `io.TextIOWrapper` to read text.

    The size and CRC are checked as the end is reached: the read that gets there raises
    `BadZipFile` if they don't match. Until then, data already returned isn't verified yet.

    Seeking forward downloads and discards the data it skips. Seeking backward, beyond what's
    still buffered, opens the member again (one or two requests) and reads from its start.
    Every byte still passes through the CRC check, unlike `zipfile` after a seek. `tell()` is
    free. For many random seeks, read the member into `io.BytesIO` instead.

    It stays readable after its `RemoteZip` is closed. After a read fails, later reads raise
    `OSError`; open the member again. Don't share one between threads: reads are serialized
    but share one position.
    """

    def __init__(self, stream: EntryStream, name: str) -> None:
        super().__init__(_RawEntry(stream), _BUFFER_SIZE)
        self._name = name

    @property
    def name(self) -> str:  # type: ignore[override]
        """The member's name in the archive."""
        return self._name

    def __repr__(self) -> str:
        state = " [closed]" if self.closed else ""
        return f"<lazy_zip.ZipExtFile name={self._name!r}{state}>"


class RemoteZip(_lazy_zip.RemoteZip):
    """A remote ZIP archive, read with HTTP range requests. A read-only `zipfile.ZipFile`.

    Creating one downloads the archive's directory, usually in one or two requests, and keeps
    it for the object's life: listing makes no further requests, and each member read costs
    at most two requests and about its compressed size. Reuse one object for many reads.

    The server must support range requests. Every response is checked against the first, so if
    the file changes on the server, reads fail with `kind` `"remote_changed"`; open it again.
    Interrupted downloads resume where they stopped, and failed requests are retried a few
    times (honoring `Retry-After`) before an error is raised.

    Args:
        url: The archive's `http://` or `https://` URL. It must stay valid while you use the
            object, so a presigned URL must not expire before you're done.
        headers: HTTP headers sent with every request, as a mapping or an iterable of
            `(name, value)` pairs, such as `{"Authorization": "Bearer ..."}`. Values are
            treated as secrets. `Range` and `Accept-Encoding` are set by `lazy_zip` and can't
            be given. On a redirect to another site, `Authorization`, `Cookie` and
            `Proxy-Authorization` are dropped as usual; any other header would be handed over,
            so such a redirect fails with `ConnectionError` instead.
        timeout: Seconds to wait to connect, and for each piece of data, as one number or a
            `(connect, read)` tuple, like `requests`. Defaults to 15 seconds to connect and 30
            per read. It bounds stalls, not a whole download. Must be positive and finite.

    Raises:
        ConnectionError: The archive couldn't be downloaded, including timeouts and a server
            without range support. See `kind`.
        BadZipFile: The file isn't a ZIP archive, or its directory is corrupt or too large.
        ValueError: A header or the timeout is invalid.
        TypeError: `timeout` isn't a number or a tuple of numbers.

    Example:
        >>> with RemoteZip("https://example.com/files.zip", timeout=(5, 60)) as archive:  # doctest: +SKIP
        ...     for info in archive.infolist():
        ...         print(info.filename, info.file_size)
        ...     with archive.open("data/records.csv") as f:
        ...         header = f.readline()

    Members can be given by name or as a `ZipInfo`. When names repeat, a name means the last
    entry with it, like `zipfile`, while a `ZipInfo` means its own entry.

    Thread safety: methods can be called from several threads at once, and they release the
    GIL while waiting for the network. `close()` doesn't interrupt calls already running.
    """

    def open(self, name: Union[str, ZipInfo], mode: str = "r") -> ZipExtFile:
        """Opens a member for streaming and returns a `ZipExtFile`.

        The member's local header is fetched and checked before this returns, so a missing,
        encrypted or malformed member fails here rather than on the first read.

        Args:
            name: A member name, or a `ZipInfo`.
            mode: Must be `"r"`. Archives are read-only.

        Returns:
            A seekable, read-only binary file object.

        Raises:
            ValueError: `mode` isn't `"r"`, or the archive is closed.
            KeyError: No member has that name.
            BadZipFile: The member is corrupt or looks malicious.
            NotImplementedError: The member is encrypted or uses an unsupported compression
                method.
            ConnectionError: The download failed or timed out.
        """
        if mode != "r":
            raise ValueError('open() requires mode "r"')
        filename = name.filename if isinstance(name, ZipInfo) else name
        return ZipExtFile(self._open_stream(name), filename)

    @property
    def filelist(self) -> "list[ZipInfo]":
        """The same as `infolist()`, like `zipfile.ZipFile.filelist`."""
        return self.infolist()

    def printdir(self, file: Optional[TextIO] = None) -> None:
        """Prints a table of the members' names, dates and sizes, in `zipfile`'s format.

        Args:
            file: Where to print. Defaults to `sys.stdout`.
        """
        print("%-46s %19s %12s" % ("File Name", "Modified    ", "Size"), file=file)
        for info in self.infolist():
            date = "%d-%02d-%02d %02d:%02d:%02d" % info.date_time[:6]
            print("%-46s %s %12d" % (info.filename, date, info.file_size), file=file)
