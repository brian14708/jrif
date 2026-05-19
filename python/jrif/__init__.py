"""Python reader for the JSON Range Index Format (JRIF) v0.

Build an :class:`Index` from a JRIF sidecar and a byte source (in-memory
bytes, a file path, or a custom :class:`Source`), then navigate it by path
through a :class:`Cursor`. The cursor's operator overloads make access feel
like a native ``dict``/``list``:

>>> import jrif
>>> idx = jrif.open(open("data.json.jrif", "rb").read(), "data.json")
>>> idx.root["records"][1]["name"].as_str()
'bob'
"""

from __future__ import annotations

from .cursor import Cursor
from .document import ByteRange, Document, Value, parse_document
from .errors import (
    FetchError,
    InvalidDocumentError,
    JrifError,
    JsonType,
    NotFoundError,
    ParseError,
    Path,
    Segment,
    TypeMismatchError,
)
from .index import Index, PayloadLike, SidecarLike
from .source import BytesSource, FileObjectSource, FileSource, Source


def open(sidecar: SidecarLike, payload: PayloadLike) -> Index:  # noqa: A001
    """Parse a JRIF sidecar and pair it with a payload source.

    ``sidecar`` may be:

    * ``bytes`` / ``str`` — JSON contents directly
    * a readable binary file-like — ``.read()`` is called once

    ``payload`` may be:

    * ``bytes`` / ``bytearray`` / ``memoryview`` — wrapped as :class:`BytesSource`
      with no copy
    * a ``str`` / ``os.PathLike`` path — opened as :class:`FileSource`
    * a seekable binary file-like — wrapped as :class:`FileObjectSource`
      (lazy positional reads, no eager load)
    * any object implementing the :class:`Source` protocol
    """
    return Index.open(sidecar, payload)


__all__ = [
    "open",
    "Index",
    "Cursor",
    "Source",
    "BytesSource",
    "FileSource",
    "FileObjectSource",
    "Document",
    "Value",
    "ByteRange",
    "Path",
    "Segment",
    "JsonType",
    "parse_document",
    "JrifError",
    "InvalidDocumentError",
    "NotFoundError",
    "TypeMismatchError",
    "FetchError",
    "ParseError",
]
