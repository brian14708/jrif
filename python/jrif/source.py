"""Byte source protocol and built-in implementations.

Sources serve absolute byte ranges. The built-ins are deliberately lazy:

* :class:`BytesSource` wraps a buffer (``bytes`` / ``bytearray`` / ``memoryview``)
  and slices without copying except at the final return.
* :class:`FileSource` opens a file by path and uses ``os.pread`` for
  thread-safe positional reads.
* :class:`FileObjectSource` wraps an already-open binary file-like object
  (``BufferedReader``, ``BytesIO``, an ``mmap``, etc.) and serves ranged reads
  via ``seek`` + ``read`` under a lock — no eager load of the whole payload.
"""

from __future__ import annotations

import os
import threading
from typing import IO, Callable, Protocol, runtime_checkable


@runtime_checkable
class Source(Protocol):
    """Supplies payload bytes for absolute ``[offset, length)`` reads.

    Implementations MUST return exactly ``length`` bytes on success.
    """

    def read_exact_at(self, offset: int, length: int) -> bytes: ...


def _check_args(offset: int, length: int) -> None:
    if offset < 0 or length < 0:
        raise ValueError("offset and length must be non-negative")


def _read_exact(
    read_chunk: Callable[[int, int], bytes], offset: int, length: int
) -> bytes:
    """Drive an arbitrary chunked reader until ``length`` bytes are gathered.

    ``read_chunk(remaining, cursor)`` returns up to ``remaining`` bytes starting
    at ``cursor``. An empty return short-circuits with an ``IOError``.
    """
    out = bytearray()
    remaining = length
    cur = offset
    while remaining > 0:
        chunk = read_chunk(remaining, cur)
        if not chunk:
            raise IOError(
                f"short read at offset {offset}: got {len(out)} bytes, expected {length}"
            )
        out.extend(chunk)
        remaining -= len(chunk)
        cur += len(chunk)
    return bytes(out)


class BytesSource:
    """In-memory ``Source`` backed by a buffer.

    Wraps ``bytes``, ``bytearray``, ``memoryview``, or any object exposing the
    buffer protocol. The buffer is held by reference — no copy at
    construction. Each ``read_exact_at`` returns a fresh ``bytes``.
    """

    __slots__ = ("_buf", "_len")

    def __init__(self, buf: bytes | bytearray | memoryview) -> None:
        # Fast path for bytes: slicing already produces bytes — no memoryview
        # wrap needed. For other buffer-protocol types, view-cast once and
        # eat the bytes() copy at read time.
        if isinstance(buf, bytes):
            self._buf: bytes | memoryview = buf
        elif isinstance(buf, memoryview):
            self._buf = buf
        else:
            self._buf = memoryview(buf).cast("B")
        self._len = len(self._buf)

    def read_exact_at(self, offset: int, length: int) -> bytes:
        _check_args(offset, length)
        end = offset + length
        if end > self._len:
            raise IOError(f"read [{offset},{end}) past payload length {self._len}")
        sliced = self._buf[offset:end]
        return sliced if isinstance(sliced, bytes) else bytes(sliced)

    def __len__(self) -> int:
        return self._len


class FileSource:
    """``Source`` backed by an OS file descriptor served via ``os.pread``.

    Opens the file in binary mode at construction. Use the context-manager
    protocol or call :meth:`close` to release the descriptor. ``os.pread`` is
    thread-safe on Linux and macOS, so this source is safe to share across
    threads with no internal locking.
    """

    __slots__ = ("_fd", "_path")

    def __init__(self, path: str | os.PathLike[str]) -> None:
        self._path = os.fspath(path)
        self._fd = os.open(self._path, os.O_RDONLY)

    def read_exact_at(self, offset: int, length: int) -> bytes:
        _check_args(offset, length)
        return _read_exact(
            lambda remaining, cur: os.pread(self._fd, remaining, cur), offset, length
        )

    def close(self) -> None:
        if self._fd is not None and self._fd >= 0:
            try:
                os.close(self._fd)
            finally:
                self._fd = -1

    def __enter__(self) -> "FileSource":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    def __del__(self) -> None:
        try:
            self.close()
        except Exception:
            pass


class FileObjectSource:
    """``Source`` wrapping any seekable binary file-like object.

    Useful for in-memory ``BytesIO``, ``open(path, 'rb')`` handles you already
    own, ``mmap.mmap`` objects, or remote read-only handles that implement
    ``seek``/``read``. The wrapped object is **not** owned by this source —
    callers are responsible for closing it.

    Reads are serialized through an internal lock since most Python file
    objects are not safe for concurrent ``seek``/``read``. For genuinely
    thread-safe positional reads, prefer :class:`FileSource` over a path.
    """

    __slots__ = ("_f", "_lock")

    def __init__(self, f: IO[bytes]) -> None:
        if not (hasattr(f, "seek") and hasattr(f, "read")):
            raise TypeError("file-like object must support seek() and read()")
        self._f = f
        self._lock = threading.Lock()

    def read_exact_at(self, offset: int, length: int) -> bytes:
        _check_args(offset, length)
        with self._lock:
            self._f.seek(offset)
            return _read_exact(
                lambda remaining, _cur: self._f.read(remaining), offset, length
            )
