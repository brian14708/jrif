"""``Index`` — pair a parsed JRIF document with a payload byte source."""

from __future__ import annotations

import os
from typing import IO, Union

from .cursor import Cursor
from .document import Document, parse_document
from .source import BytesSource, FileObjectSource, FileSource, Source


PayloadLike = Union[
    Source,
    bytes,
    bytearray,
    memoryview,
    str,
    "os.PathLike[str]",
    "IO[bytes]",
]
SidecarLike = Union[bytes, str, "IO[bytes]"]


def _to_source(payload: PayloadLike) -> Source:
    if isinstance(payload, Source):
        return payload
    if isinstance(payload, (bytes, bytearray, memoryview)):
        return BytesSource(payload)
    if isinstance(payload, (str, os.PathLike)):
        return FileSource(payload)
    if hasattr(payload, "read") and hasattr(payload, "seek"):
        return FileObjectSource(payload)
    raise TypeError(
        "payload must be bytes-like, a path, a seekable binary file-like, "
        f"or a Source — got {type(payload).__name__}"
    )


def _read_sidecar(sidecar: SidecarLike) -> bytes | str:
    """Coerce ``sidecar`` to bytes/str for the JSON parser.

    The sidecar parser already loads JSON into memory, so a stream is read in
    full here. The point is to let callers pass a file-like without having to
    ``.read()`` themselves.
    """
    if isinstance(sidecar, (bytes, str)):
        return sidecar
    if hasattr(sidecar, "read"):
        data = sidecar.read()
        if not isinstance(data, (bytes, str)):
            raise TypeError(
                f"sidecar file-like returned {type(data).__name__}; "
                "expected bytes or str"
            )
        return data
    raise TypeError(
        f"sidecar must be bytes, str, or a readable file-like — got {type(sidecar).__name__}"
    )


class Index:
    """A parsed JRIF document paired with the byte source for its payload.

    Use :meth:`root` (or the ``root`` property) to obtain a :class:`Cursor` at
    the document root. Navigation does no I/O; the leaf accessors on the
    cursor pull bytes from the source as needed.
    """

    __slots__ = ("_doc", "_source")

    def __init__(self, document: Document, source: Source) -> None:
        self._doc = document
        self._source = source

    @classmethod
    def open(cls, sidecar: SidecarLike, payload: PayloadLike) -> "Index":
        doc = parse_document(_read_sidecar(sidecar))
        return cls(doc, _to_source(payload))

    @property
    def document(self) -> Document:
        return self._doc

    @property
    def source(self) -> Source:
        return self._source

    @property
    def root(self) -> Cursor:
        return Cursor._root(self)
