"""Command-line interface: list or extract files of a remote ZIP archive.

Usage:

    python -m lazy_zip URL [FILE ...] [-l] [-d DIR] [-H 'NAME: VALUE'] [-j WORKERS] [-t SECONDS]

The `lazy-zip` command installed with the package runs the same thing. Run it with `--help`
for the options. The exit status is 0 on success, 1 on an error, 2 for invalid arguments and
130 after Ctrl-C.
"""

import argparse
import sys
from typing import List, Optional

from . import BadZipFile, ExtractError, RemoteZip


def _header(value: str) -> "tuple[str, str]":
    name, sep, content = value.partition(":")
    if not sep:
        raise argparse.ArgumentTypeError(f"expected 'Name: value', got {value!r}")
    return name.strip(), content.strip()


def _size(size: int) -> str:
    value = float(size)
    for unit in ("B", "KB", "MB"):
        if value < 1024:
            return f"{value:.0f} {unit}" if unit == "B" else f"{value:.1f} {unit}"
        value /= 1024
    return f"{value:.1f} GB"


def main(argv: Optional[List[str]] = None) -> int:
    """Runs the command line and returns its exit status.

    Args:
        argv: The arguments, without the program name. Defaults to `sys.argv[1:]`.

    Returns:
        0 on success, 1 if the archive couldn't be read or a file couldn't be extracted, and
        130 after Ctrl-C. Invalid arguments make `argparse` exit with status 2.
    """
    parser = argparse.ArgumentParser(
        prog="python -m lazy_zip",
        description="List or extract files of a remote ZIP archive without downloading all of it.",
    )
    parser.add_argument("url", help="URL of the ZIP archive")
    parser.add_argument("files", nargs="*", help="files to extract (default: all)")
    parser.add_argument("-l", "--list", action="store_true", help="list the files instead")
    parser.add_argument("-d", "--dir", default=None, help="extract into this folder (default: current)")
    parser.add_argument(
        "-H",
        "--header",
        action="append",
        type=_header,
        default=[],
        metavar="'NAME: VALUE'",
        help="send an HTTP header with every request; can be repeated",
    )
    parser.add_argument("-j", "--workers", type=int, default=8, help="files downloaded at once")
    parser.add_argument(
        "-t", "--timeout", type=float, default=None, metavar="SECONDS", help="connect and read timeout"
    )
    args = parser.parse_args(argv)

    try:
        with RemoteZip(args.url, headers=dict(args.header), timeout=args.timeout) as archive:
            if args.list:
                infos = archive.infolist()
                if args.files:
                    wanted = set(args.files)
                    infos = [info for info in infos if info.filename in wanted]
                for info in infos:
                    print(f"{_size(info.file_size):>10}  {info.filename}")
                return 0
            archive.extractall(args.dir, args.files or None, workers=args.workers)
            print(
                f"Extracted to {args.dir or '.'} "
                f"({_size(archive.network_bytes)} downloaded of {_size(archive.content_length)})",
                file=sys.stderr,
            )
            return 0
    except KeyboardInterrupt:
        return 130
    except (BadZipFile, ExtractError, KeyError, OSError, ValueError, NotImplementedError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
