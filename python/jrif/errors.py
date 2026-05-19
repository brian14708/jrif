"""Error types, JSON-type discriminator, and path/segment helpers."""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Sequence


class JsonType(str, Enum):
    NULL = "null"
    BOOLEAN = "boolean"
    NUMBER = "number"
    STRING = "string"
    ARRAY = "array"
    OBJECT = "object"

    def __str__(self) -> str:  # noqa: D401
        return self.value

    @classmethod
    def of(cls, value: Any) -> "JsonType":
        t = type(value)
        out = _TYPE_TO_JSON.get(t)
        if out is not None:
            return out
        # Fallback for subclasses (rare in JSON-shaped values).
        if isinstance(value, bool):
            return cls.BOOLEAN
        if isinstance(value, (int, float)):
            return cls.NUMBER
        if isinstance(value, str):
            return cls.STRING
        if isinstance(value, dict):
            return cls.OBJECT
        if isinstance(value, list):
            return cls.ARRAY
        if value is None:
            return cls.NULL
        raise TypeError(f"unknown json type for {t.__name__}")


_TYPE_TO_JSON: dict[type, JsonType] = {
    type(None): JsonType.NULL,
    bool: JsonType.BOOLEAN,
    int: JsonType.NUMBER,
    float: JsonType.NUMBER,
    str: JsonType.STRING,
    list: JsonType.ARRAY,
    dict: JsonType.OBJECT,
}


class Segment:
    """One step in a Path from the document root to a cursor.

    Exactly one of ``field`` or ``index`` is meaningful; ``is_index``
    disambiguates.

    Implementation note: hand-written rather than ``@dataclass(frozen=True,
    slots=True)`` because ``Segment`` is allocated on every cursor descent.
    The ``cls.__new__`` factories cut construction by ~2.8× over the
    dataclass-generated ``__init__``. Slots are never mutated after the
    factory returns, so the ``__eq__`` / ``__hash__`` pair is consistent in
    practice — but the class is not enforced-frozen. Do not mutate.
    """

    __slots__ = ("field", "index", "is_index")

    def __init__(self, field: str = "", index: int = 0, is_index: bool = False) -> None:
        self.field = field
        self.index = index
        self.is_index = is_index

    @classmethod
    def of_field(cls, name: str) -> "Segment":
        s = cls.__new__(cls)
        s.field = name
        s.index = 0
        s.is_index = False
        return s

    @classmethod
    def of_index(cls, ordinal: int) -> "Segment":
        s = cls.__new__(cls)
        s.field = ""
        s.index = ordinal
        s.is_index = True
        return s

    def __str__(self) -> str:
        return f"[{self.index}]" if self.is_index else f".{self.field}"

    def __repr__(self) -> str:
        if self.is_index:
            return f"Segment.of_index({self.index!r})"
        return f"Segment.of_field({self.field!r})"

    def __eq__(self, other: object) -> bool:
        return (
            isinstance(other, Segment)
            and self.is_index == other.is_index
            and self.index == other.index
            and self.field == other.field
        )

    def __hash__(self) -> int:
        return hash((self.field, self.index, self.is_index))


@dataclass(frozen=True, slots=True)
class Path:
    segments: tuple[Segment, ...] = field(default_factory=tuple)

    @classmethod
    def of(cls, segments: Sequence[Segment]) -> "Path":
        return cls(tuple(segments))

    def __str__(self) -> str:
        return "$" + "".join(str(s) for s in self.segments)

    def __bool__(self) -> bool:
        return bool(self.segments)


class JrifError(Exception):
    """Base class for all jrif errors."""


class InvalidDocumentError(JrifError):
    """Malformed or unsupported JRIF sidecar."""

    def __init__(self, reason: str, jrif_version: str | None = None) -> None:
        super().__init__(f"invalid JRIF document: {reason}")
        self.reason = reason
        self.jrif_version = jrif_version


class NotFoundError(JrifError, KeyError, IndexError):
    """An object field is missing, or an array index is out of bounds.

    Subclasses both ``KeyError`` and ``IndexError`` so that user code written
    against native dict/list semantics catches the right exception regardless
    of access mode.
    """

    def __init__(self, path: Path) -> None:
        JrifError.__init__(self, f"not found at {path}")
        self.path = path

    # KeyError prints its arg with repr() and would render "'not found at $.x'".
    # Override so the message stays clean.
    def __str__(self) -> str:  # noqa: D401
        return f"not found at {self.path}"


class TypeMismatchError(JrifError, TypeError):
    """A leaf accessor or descent step saw an unexpected JSON type."""

    def __init__(self, path: Path, expected: JsonType, got: JsonType) -> None:
        msg = f"type mismatch at {path}: expected {expected}, got {got}"
        JrifError.__init__(self, msg)
        self.path = path
        self.expected = expected
        self.got = got


class FetchError(JrifError):
    """The underlying byte source raised while serving a range."""

    def __init__(self, path: Path, cause: BaseException) -> None:
        prefix = f"fetch failed at {path}" if path else "fetch failed"
        super().__init__(f"{prefix}: {cause}")
        self.path = path
        self.__cause__ = cause


class ParseError(JrifError):
    """Fetched bytes did not parse as a complete JSON value."""

    def __init__(self, path: Path, cause: BaseException) -> None:
        prefix = f"parse error at {path}" if path else "parse error"
        super().__init__(f"{prefix}: {cause}")
        self.path = path
        self.__cause__ = cause
