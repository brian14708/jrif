"""``Cursor`` — Pythonic navigation handle over a JRIF-indexed payload.

Navigation methods (``cursor[k]``, ``cursor.get(name)``, ``cursor.index(i)``)
are synchronous and infallible: they either advance through the chunk index in
memory or record a pending segment. All I/O and parsing happens in the leaf
accessors (``.value()``, ``.bytes()``, ``.as_str()``, iteration, ``len()``,
``in``, …), which run any deferred work in one go and surface the first error
they hit.

The class deliberately overloads the operators a Python user would reach for
on a ``dict`` or ``list``:

================  =========================  ==================================
operator          on a cursor at…           behavior
================  =========================  ==================================
``c[key]``        object                      child cursor for that field
``c[i]``          array                       child cursor for that ordinal
``len(c)``        array | object | string     element / key / char count
``for x in c``    array                       child cursors
``for k in c``    object                      field names (matches ``dict``)
``key in c``      object                      membership test
``int(c)``,       number                      typed coercion
``float(c)``,     number                      typed coercion
``str(c)``        any                         like ``str()`` on the value
``bool(c)``       any                         Python truthiness of the value
``c == x``        any                         compare against a Python value
================  =========================  ==================================
"""

from __future__ import annotations

import copy
import json
from typing import TYPE_CHECKING, Any, Iterator, Optional

from .document import ByteRange, HitKind, Value, ValueKind
from .errors import (
    FetchError,
    JrifError,
    JsonType,
    NotFoundError,
    ParseError,
    Path,
    Segment,
    TypeMismatchError,
)
from .nav import (
    DONE_FRAME,
    FRAME_ARRAY,
    FRAME_OBJECT,
    WRAP_ARRAY,
    WRAP_NONE,
    WRAP_OBJECT,
    Frame,
    array_len,
    find_array_match,
    find_object_match,
    iter_array_chunks,
)

if TYPE_CHECKING:
    from .index import Index


# Module-level decoder reused per call: skips json.loads' BOM / encoding
# detection. ~3× faster than ``json.loads(bytes)`` on small fragments.
_DECODER = json.JSONDecoder()
_MISSING: Any = object()


def _decode_json(b: bytes | str) -> Any:
    s = b.decode() if isinstance(b, (bytes, bytearray, memoryview)) else b
    obj, end = _DECODER.raw_decode(s)
    # raw_decode stops at the first complete JSON value; reject any
    # significant trailing data to match json.loads semantics.
    if end < len(s) and s[end:].strip():
        raise json.JSONDecodeError("Extra data", s, end)
    return obj


def _wrap_bytes(b: bytes, wrap: int) -> bytes:
    if wrap == WRAP_ARRAY:
        return b"[" + b + b"]"
    if wrap == WRAP_OBJECT:
        return b"{" + b + b"}"
    return b


def _walk_pending(value: Any, pending: list[Segment], full_path: list[Segment]) -> Any:
    """Walk ``pending`` segments against an already-parsed ``value``.

    Errors carry the path prefix up to and including the failing segment.
    """
    prefix_off = len(full_path) - len(pending)
    for i, seg in enumerate(pending):
        if seg.is_index:
            if not isinstance(value, list):
                raise TypeMismatchError(
                    Path.of(full_path[: prefix_off + i + 1]),
                    JsonType.ARRAY,
                    JsonType.of(value),
                )
            idx = seg.index
            if idx < 0 or idx >= len(value):
                raise NotFoundError(Path.of(full_path[: prefix_off + i + 1]))
            value = value[idx]
            continue
        if not isinstance(value, dict):
            raise TypeMismatchError(
                Path.of(full_path[: prefix_off + i + 1]),
                JsonType.OBJECT,
                JsonType.of(value),
            )
        sub = value.get(seg.field, _MISSING)
        if sub is _MISSING:
            raise NotFoundError(Path.of(full_path[: prefix_off + i + 1]))
        value = sub
    return value


class Cursor:
    """Position inside a JRIF-indexed payload. Immutable; descent returns a new cursor.

    The cursor's resolved state lives in five slots:

    * ``_inline`` / ``_inline_present`` — the inline JSON literal, when the
      cursor sits on one. ``_inline_present`` flags presence so a legitimate
      ``None`` inline value is distinguishable from a ranged base.
    * ``_range`` / ``_wrap`` — the payload byte range that backs the cursor
      and how a fetched fragment should be wrapped (none / ``[...]`` /
      ``{...}``) to form valid JSON.
    * ``_frame`` — the live chunk-navigation context, or :data:`DONE_FRAME`
      when no further chunk descent is possible.
    """

    __slots__ = (
        "_idx",
        "_path",
        "_pending",
        "_frame",
        "_range",
        "_wrap",
        "_inline",
        "_inline_present",
    )

    def __init__(
        self,
        idx: "Index",
        path: list[Segment],
        pending: list[Segment],
        frame: Frame,
        range: Optional[ByteRange],
        wrap: int,
        inline: Any,
        inline_present: bool,
    ) -> None:
        self._idx = idx
        self._path = path
        self._pending = pending
        self._frame = frame
        self._range = range
        self._wrap = wrap
        self._inline = inline
        self._inline_present = inline_present

    @classmethod
    def _make(
        cls,
        idx: "Index",
        path: list[Segment],
        pending: list[Segment],
        frame: Optional[Frame],
        range: Optional[ByteRange],
        wrap: int,
        inline: Any,
        inline_present: bool,
    ) -> "Cursor":
        """Internal fast constructor: skips ``__init__`` dispatch by writing
        slots directly. ``frame`` is typed Optional only because pre-parsed
        ``Value._frame`` is initialized to ``None``; callers must pass a
        populated frame (``populate_chunk_caches`` primes every Value).
        """
        c = cls.__new__(cls)
        c._idx = idx
        c._path = path
        c._pending = pending
        c._frame = frame if frame is not None else DONE_FRAME
        c._range = range
        c._wrap = wrap
        c._inline = inline
        c._inline_present = inline_present
        return c

    # ---- construction ------------------------------------------------------

    @classmethod
    def _root(cls, idx: "Index") -> "Cursor":
        v = idx.document.root
        if v.kind is ValueKind.INLINE:
            return cls._make(idx, [], [], v._frame, None, WRAP_NONE, v.inline, True)
        return cls._make(idx, [], [], v._frame, v.range, WRAP_NONE, None, False)

    # ---- introspection -----------------------------------------------------

    @property
    def path(self) -> Path:
        return Path.of(self._path)

    @property
    def range(self) -> Optional[ByteRange]:
        """Absolute byte range the cursor identifies, when fully resolved
        against a contiguous payload value."""
        if self._pending or self._inline_present or self._wrap != WRAP_NONE:
            return None
        return self._range

    def is_resolved(self) -> bool:
        """True when the cursor exactly identifies one contiguous JSON value
        (inline or a no-wrap range) without further parse work."""
        return not self._pending and self._wrap == WRAP_NONE

    def json_type_hint(self) -> Optional[JsonType]:
        """Best-effort JSON type from the chunk index, without any I/O."""
        if self._pending:
            return None
        if self._inline_present:
            return JsonType.of(self._inline)
        k = self._frame.kind
        if k == FRAME_ARRAY:
            return JsonType.ARRAY
        if k == FRAME_OBJECT:
            return JsonType.OBJECT
        return None

    # ---- descent -----------------------------------------------------------

    def get(self, name: str) -> "Cursor":
        """Descend to an object member. Infallible — no I/O."""
        frame = self._frame
        seg = Segment.of_field(name)
        new_path = self._path + [seg]
        if self._pending or self._inline_present or frame.kind != FRAME_OBJECT:
            return self._defer(seg, new_path)
        hit = find_object_match(frame, name)
        if hit is None:
            return self._defer(seg, new_path)
        if hit.kind is HitKind.FIELD:
            return self._fill_from_value(hit.value, new_path)
        # FIELDS — wrap the byte range, defer field-name resolution to parse.
        return self._fill_wrapped(hit.range, WRAP_OBJECT, seg, new_path)

    def index(self, ordinal: int) -> "Cursor":
        """Descend to an array item by ordinal. Infallible — no I/O."""
        frame = self._frame
        seg = Segment.of_index(ordinal)
        new_path = self._path + [seg]
        if self._pending or self._inline_present or frame.kind != FRAME_ARRAY:
            return self._defer(seg, new_path)
        hit = find_array_match(frame, ordinal)
        if hit is None:
            return self._defer(seg, new_path)
        if hit.kind is HitKind.ITEM:
            return self._fill_from_value(hit.value, new_path)
        # ITEMS — wrap, defer ordinal resolution to parse with a rebased ordinal.
        rebased = Segment.of_index(ordinal - hit.start_ordinal)
        return self._fill_wrapped(hit.range, WRAP_ARRAY, rebased, new_path)

    def _defer(self, seg: Segment, new_path: list[Segment]) -> "Cursor":
        return Cursor._make(
            self._idx,
            new_path,
            self._pending + [seg],
            self._frame,
            self._range,
            self._wrap,
            self._inline,
            self._inline_present,
        )

    def _fill_from_value(self, v: Optional[Value], new_path: list[Segment]) -> "Cursor":
        if v is None:
            raise JrifError(f"missing wrapped value at {Path.of(new_path)}")
        idx = self._idx
        frame = v._frame
        if v.kind is ValueKind.INLINE:
            return Cursor._make(
                idx, new_path, [], frame, None, WRAP_NONE, v.inline, True
            )
        return Cursor._make(idx, new_path, [], frame, v.range, WRAP_NONE, None, False)

    def _fill_wrapped(
        self,
        rng: Optional[ByteRange],
        wrap: int,
        pending_seg: Segment,
        new_path: list[Segment],
    ) -> "Cursor":
        return Cursor._make(
            self._idx, new_path, [pending_seg], DONE_FRAME, rng, wrap, None, False
        )

    # ---- leaf accessors ----------------------------------------------------

    def value(self) -> Any:
        """Fetch + parse + walk pending. Returns the JSON value at this cursor."""
        return self._safe_materialize()

    def bytes(self) -> bytes:
        """Return the cursor's bytes as a complete JSON value.

        Fast path (fully resolved, ranged): exact payload slice. Inline values
        round-trip through ``json.dumps``. The slow path walks pending segments
        in parsed form and re-encodes the target.
        """
        if self.is_resolved():
            if self._inline_present:
                return json.dumps(self._inline).encode()
            return self._fetch_raw()
        return json.dumps(self._safe_materialize()).encode()

    def keys(self) -> list[str]:
        """Field names of the underlying object, in source order.

        Always parses the underlying bytes — JRIF v0 allows object chunks to
        cover only a subset of fields, so the chunk index can't enumerate
        every key.
        """
        val = self._materialize()
        if not isinstance(val, dict):
            raise TypeMismatchError(self.path, JsonType.OBJECT, JsonType.of(val))
        return list(val.keys())

    def items(self) -> Iterator[tuple[str, "Cursor"]]:
        for k in self.keys():
            yield k, self.get(k)

    def values(self) -> Iterator["Cursor"]:
        for k in self.keys():
            yield self.get(k)

    def len(self) -> int:
        """Length of the array/object/string at this cursor."""
        if not self._pending:
            if self._inline_present:
                v = self._inline
                if isinstance(v, (list, dict, str)):
                    return len(v)
                raise TypeMismatchError(self.path, JsonType.ARRAY, JsonType.of(v))
            if self._frame.kind == FRAME_ARRAY:
                n = array_len(self._frame)
                if n is not None:
                    return n
        v = self._materialize()
        if isinstance(v, (list, dict, str)):
            return len(v)
        raise TypeMismatchError(self.path, JsonType.ARRAY, JsonType.of(v))

    def as_str(self) -> str:
        v = self._materialize()
        if not isinstance(v, str):
            raise TypeMismatchError(self.path, JsonType.STRING, JsonType.of(v))
        return v

    def as_int(self) -> int:
        v = self._require_number()
        if isinstance(v, float):
            if not v.is_integer():
                raise ParseError(self.path, ValueError(f"{v!r} is not an integer"))
            return int(v)
        return v

    def as_float(self) -> float:
        return float(self._require_number())

    def as_bool(self) -> bool:
        v = self._materialize()
        if not isinstance(v, bool):
            raise TypeMismatchError(self.path, JsonType.BOOLEAN, JsonType.of(v))
        return v

    def _require_number(self) -> int | float:
        v = self._materialize()
        if isinstance(v, bool) or not isinstance(v, (int, float)):
            raise TypeMismatchError(self.path, JsonType.NUMBER, JsonType.of(v))
        return v

    # ---- operator overloads ------------------------------------------------

    def __getitem__(self, key: Any) -> "Cursor":
        # Order branches by frequency: string keys (dict-style) are the
        # most common; integer ordinals second; everything else errors.
        kt = type(key)
        if kt is str:
            return self.get(key)
        if kt is int:
            return self._resolve_int(key)
        if isinstance(key, bool):  # bool is subclass of int — reject explicitly
            raise TypeError("cursor index must be str or int, not bool")
        if isinstance(key, int):
            return self._resolve_int(int(key))
        raise TypeError(f"cursor index must be str or int, not {kt.__name__}")

    def _resolve_int(self, n: int) -> "Cursor":
        if n < 0:
            adjusted = self.len() + n
            if adjusted < 0:
                raise NotFoundError(Path.of(self._path + [Segment.of_index(n)]))
            return self.index(adjusted)
        return self.index(n)

    def __contains__(self, key: object) -> bool:
        # Mirror ``key in dict``. For arrays we'd have to materialize and compare
        # values; deliberately unsupported to avoid silent I/O surprises.
        if not isinstance(key, str):
            return False
        try:
            return key in self.keys()
        except (TypeMismatchError, JrifError):
            return False

    def __iter__(self) -> Iterator[Any]:
        hint = self.json_type_hint()
        if hint is JsonType.OBJECT:
            return iter(self.keys())
        if hint is JsonType.ARRAY:
            return self._iter_array()
        if hint is None:
            v = self._materialize()
            if isinstance(v, dict):
                return iter(list(v.keys()))
            if isinstance(v, list):
                return (self.index(i) for i in range(len(v)))
        raise TypeMismatchError(
            self.path, JsonType.ARRAY, hint or JsonType.of(self._materialize())
        )

    def _iter_array(self) -> Iterator["Cursor"]:
        """Walk the array's chunks linearly, yielding one child cursor per item.

        Avoids the O(log M) bisect that ``__iter__`` would otherwise do per
        ordinal — chunked iteration costs O(M + N) instead of O(N log M).
        """
        if self._frame.kind == FRAME_ARRAY:
            base_path = self._path
            for ordinal, hit in iter_array_chunks(self._frame):
                seg = Segment.of_index(ordinal)
                new_path = base_path + [seg]
                if hit.kind is HitKind.ITEM:
                    yield self._fill_from_value(hit.value, new_path)
                else:
                    rebased = Segment.of_index(ordinal - hit.start_ordinal)
                    yield self._fill_wrapped(hit.range, WRAP_ARRAY, rebased, new_path)
            return
        # No chunk frame (unchunked array, or wrapped fragment): per-index fall-back.
        for i in range(self.len()):
            yield self.index(i)

    def __len__(self) -> int:
        return self.len()

    def __bool__(self) -> bool:
        return bool(self._materialize())

    def __str__(self) -> str:
        return str(self._materialize())

    def __int__(self) -> int:
        return self.as_int()

    def __float__(self) -> float:
        return self.as_float()

    def __eq__(self, other: object) -> bool:
        if isinstance(other, Cursor):
            return self._materialize() == other._materialize()
        try:
            return self._materialize() == other
        except (FetchError, ParseError, TypeMismatchError, NotFoundError):
            return False

    __hash__ = None  # type: ignore[assignment]

    def __repr__(self) -> str:
        return f"Cursor({self.path})"

    # ---- internals ---------------------------------------------------------

    def _fetch_raw(self) -> bytes:
        rng = self._range
        if rng is None:
            raise JrifError("internal: fetch_raw on inline base")
        try:
            b = self._idx._source.read_exact_at(rng.start, rng.length)
        except Exception as exc:
            raise FetchError(self.path, exc) from exc
        if len(b) != rng.length:
            raise FetchError(
                self.path,
                IOError(
                    f"source returned {len(b)} bytes for [{rng.start},{rng.length}]"
                ),
            )
        return b

    def _materialize(self) -> Any:
        """Resolve the cursor to a Python JSON value.

        Returns the value verbatim — callers who hand the result back to user
        code should go through :meth:`_safe_materialize` so a mutation by the
        caller can't corrupt cached inline literals.
        """
        if self._inline_present:
            if not self._pending:
                return self._inline
            return _walk_pending(self._inline, self._pending, self._path)
        raw = self._fetch_raw()
        wrapped = _wrap_bytes(raw, self._wrap)
        try:
            parsed = _decode_json(wrapped)
        except json.JSONDecodeError as exc:
            raise ParseError(self.path, exc) from exc
        if not self._pending:
            return parsed
        return _walk_pending(parsed, self._pending, self._path)

    def _safe_materialize(self) -> Any:
        """Like :meth:`_materialize` but defensively copies inline containers
        before handing them out, so callers can mutate the result without
        corrupting cached document state.

        Freshly parsed bytes are returned as-is — they're already owned.
        """
        v = self._materialize()
        if self._inline_present and isinstance(v, (dict, list)):
            return copy.deepcopy(v)
        return v
