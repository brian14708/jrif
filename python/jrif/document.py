"""Parse a JRIF v0 sidecar into an in-memory document tree.

The wire format uses short keys (``t``, ``r``, ``c``, ``k``, ``f``, ``n``, ``v``)
described in ``docs/spec.md``. The data classes here mirror those, but expose
slightly richer Python names.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from enum import Enum
from typing import TYPE_CHECKING, Any, Optional

from .errors import InvalidDocumentError

if TYPE_CHECKING:
    from .nav import Frame


JRIF_V0_TAG = "v0"


class ValueKind(str, Enum):
    """Discriminator for :class:`Value`. Values match the JRIF wire tags."""

    INLINE = "v"
    STRING = "s"
    ARRAY = "a"
    OBJECT = "o"


class ArrayChunkKind(str, Enum):
    """Discriminator for :class:`ArrayChunk`. Values match the JRIF wire tags."""

    ITEMS = "is"
    ITEM = "i"


class ObjectChunkKind(str, Enum):
    """Discriminator for :class:`ObjectChunk`. Values match the JRIF wire tags."""

    FIELDS = "fs"
    FIELD = "f"


class HitKind(str, Enum):
    """Discriminator for :class:`ArrayHit` / :class:`ObjectHit`."""

    ITEM = "item"
    ITEMS = "items"
    FIELD = "field"
    FIELDS = "fields"


@dataclass(frozen=True, slots=True)
class ByteRange:
    """An absolute ``[start, length]`` byte range into the payload."""

    start: int
    length: int

    @property
    def end(self) -> int:
        return self.start + self.length

    def contains(self, child: "ByteRange") -> bool:
        return self.start <= child.start and child.end <= self.end


def _parse_range(raw: Any, *, where: str) -> ByteRange:
    if not isinstance(raw, list) or len(raw) != 2:
        raise InvalidDocumentError(f"{where}: range must be [start, length]")
    start, length = raw
    _require_nonneg_int(start, where=f"{where}.start")
    _require_pos_int(length, where=f"{where}.length")
    return ByteRange(start, length)


def _require_nonneg_int(value: Any, *, where: str) -> None:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise InvalidDocumentError(f"{where}: must be a non-negative integer")


def _require_pos_int(value: Any, *, where: str) -> None:
    if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
        raise InvalidDocumentError(f"{where}: must be a positive integer")


# ---- Value tree ------------------------------------------------------------


@dataclass
class Value:
    """A JRIF ``Value`` — inline literal or ranged ``s | a | o``."""

    kind: ValueKind
    inline: Any = None  # set when kind == INLINE
    range: Optional[ByteRange] = None  # set when kind != INLINE
    array_chunks: list["ArrayChunk"] = field(default_factory=list)
    object_chunks: list["ObjectChunk"] = field(default_factory=list)

    _arr_cum: Optional[list[int]] = field(default=None, repr=False)
    _arr_hits: Optional[list["ArrayHit"]] = field(default=None, repr=False)
    _obj_lookup: Optional[dict[str, "ObjectHit"]] = field(default=None, repr=False)
    # Pre-computed Frame for descent into this Value. Populated by
    # populate_chunk_caches at parse time so cursor descent is one attribute
    # load — never None for a Value reachable from a parsed Document.
    _frame: Optional["Frame"] = field(default=None, repr=False)

    @property
    def is_inline(self) -> bool:
        return self.kind is ValueKind.INLINE


@dataclass
class ArrayChunk:
    kind: ArrayChunkKind
    count: int = 0
    range: Optional[ByteRange] = None
    value: Optional[Value] = None  # populated for ITEM


@dataclass
class ObjectChunk:
    kind: ObjectChunkKind
    fields: list[int] = field(default_factory=list)  # populated for FIELDS
    range: Optional[ByteRange] = None
    name: int = 0  # populated for FIELD
    value: Optional[Value] = None  # populated for FIELD


# ---- ObjectHit / ArrayHit --------------------------------------------------


@dataclass(frozen=True, slots=True)
class ObjectHit:
    """Pre-computed result of resolving a field name through object chunks."""

    kind: HitKind
    range: Optional[ByteRange]
    value: Optional[Value]


@dataclass(frozen=True, slots=True)
class ArrayHit:
    kind: HitKind
    range: Optional[ByteRange]
    value: Optional[Value]
    start_ordinal: int = 0


# ---- Document --------------------------------------------------------------


@dataclass
class Document:
    jrif: str
    meta: dict[str, Any] = field(default_factory=dict)
    keys: list[str] = field(default_factory=list)
    root: Value = field(
        default_factory=lambda: Value(kind=ValueKind.INLINE, inline=None)
    )


def parse_document(data: bytes | str) -> Document:
    """Parse a JRIF sidecar.

    The top-level shape is ``{"jrif", "meta"?, "keys"?, "root"}``. Unknown
    top-level keys are rejected to mirror the strict-decoding behavior of the
    Rust and Go readers.
    """
    try:
        raw = json.loads(data)
    except json.JSONDecodeError as exc:
        raise InvalidDocumentError(f"parse jrif: {exc}") from exc
    if not isinstance(raw, dict):
        raise InvalidDocumentError("top-level must be a JSON object")

    allowed_top = {"jrif", "meta", "keys", "root"}
    extra = set(raw) - allowed_top
    if extra:
        raise InvalidDocumentError(f"unknown top-level field(s): {sorted(extra)}")

    if "jrif" not in raw:
        raise InvalidDocumentError("missing `jrif`")
    if "root" not in raw:
        raise InvalidDocumentError("missing `root`")
    jrif_tag = raw["jrif"]
    if not isinstance(jrif_tag, str):
        raise InvalidDocumentError("`jrif` must be a string")
    if jrif_tag != JRIF_V0_TAG:
        raise InvalidDocumentError(
            f"unsupported jrif version: {jrif_tag}", jrif_version=jrif_tag
        )

    meta = raw.get("meta") or {}
    if not isinstance(meta, dict):
        raise InvalidDocumentError("`meta` must be an object")

    keys = raw.get("keys") or []
    if not isinstance(keys, list) or not all(isinstance(k, str) for k in keys):
        raise InvalidDocumentError("`keys` must be an array of strings")

    root = _parse_value(raw["root"], where="root")
    doc = Document(jrif=jrif_tag, meta=dict(meta), keys=list(keys), root=root)
    populate_chunk_caches(doc.root, doc.keys)
    return doc


# ---- Value/chunk parsing ---------------------------------------------------


_VALUE_FIELDS = {"t", "v", "r", "c"}
_ITEM_FIELDS = {"k"} | _VALUE_FIELDS
_FIELDS_GROUP_FIELDS = {"k", "f", "r"}
_FIELD_ENTRY_FIELDS = {"k", "n"} | _VALUE_FIELDS
_ITEMS_GROUP_FIELDS = {"k", "n", "r"}


def _check_keys(raw: dict[str, Any], allowed: set[str], *, where: str) -> None:
    extra = set(raw) - allowed
    if extra:
        raise InvalidDocumentError(f"{where}: unknown field(s): {sorted(extra)}")


def _parse_value(raw: Any, *, where: str) -> Value:
    if not isinstance(raw, dict):
        raise InvalidDocumentError(f"{where}: must be an object")
    if "t" not in raw:
        raise InvalidDocumentError(f"{where}: missing `t`")
    _check_keys(raw, _VALUE_FIELDS, where=where)
    try:
        kind = ValueKind(raw["t"])
    except ValueError:
        raise InvalidDocumentError(f"{where}: unknown type {raw['t']!r}") from None
    if kind is ValueKind.INLINE:
        if "v" not in raw:
            raise InvalidDocumentError(f"{where}(v): missing `v`")
        if "r" in raw or "c" in raw:
            raise InvalidDocumentError(
                f"{where}(v): inline must not carry range/chunks"
            )
        return Value(kind=kind, inline=raw["v"])
    if "v" in raw:
        raise InvalidDocumentError(
            f"{where}({kind.value}): ranged form must not carry inline value"
        )
    if "r" not in raw:
        raise InvalidDocumentError(f"{where}({kind.value}): ranged form requires `r`")
    rng = _parse_range(raw["r"], where=f"{where}.r")
    if kind is ValueKind.STRING:
        if "c" in raw:
            raise InvalidDocumentError(f"{where}(s): must not carry chunks")
        return Value(kind=kind, range=rng)
    chunks_raw = raw.get("c") or []
    if kind is ValueKind.ARRAY:
        return Value(
            kind=kind,
            range=rng,
            array_chunks=[
                _parse_array_chunk(c, where=f"{where}.c[{i}]")
                for i, c in enumerate(chunks_raw)
            ],
        )
    return Value(
        kind=kind,
        range=rng,
        object_chunks=[
            _parse_object_chunk(c, where=f"{where}.c[{i}]")
            for i, c in enumerate(chunks_raw)
        ],
    )


def _parse_array_chunk(raw: Any, *, where: str) -> ArrayChunk:
    if not isinstance(raw, dict):
        raise InvalidDocumentError(f"{where}: must be an object")
    k = raw.get("k")
    if k == ArrayChunkKind.ITEMS.value:
        _check_keys(raw, _ITEMS_GROUP_FIELDS, where=where)
        if "n" not in raw or "r" not in raw:
            raise InvalidDocumentError(f"{where}(is): requires `n` and `r`")
        _require_pos_int(raw["n"], where=f"{where}.n")
        return ArrayChunk(
            kind=ArrayChunkKind.ITEMS,
            count=raw["n"],
            range=_parse_range(raw["r"], where=f"{where}.r"),
        )
    if k == ArrayChunkKind.ITEM.value:
        _check_keys(raw, _ITEM_FIELDS, where=where)
        # The wrapped Value is flattened — strip k and treat the rest as a Value.
        v = _parse_value(_without_keys(raw, ("k",)), where=where)
        return ArrayChunk(kind=ArrayChunkKind.ITEM, value=v, range=v.range)
    raise InvalidDocumentError(f"{where}: unknown chunk kind {k!r}")


def _parse_object_chunk(raw: Any, *, where: str) -> ObjectChunk:
    if not isinstance(raw, dict):
        raise InvalidDocumentError(f"{where}: must be an object")
    k = raw.get("k")
    if k == ObjectChunkKind.FIELDS.value:
        _check_keys(raw, _FIELDS_GROUP_FIELDS, where=where)
        if "f" not in raw or "r" not in raw:
            raise InvalidDocumentError(f"{where}(fs): requires `f` and `r`")
        fids = raw["f"]
        if not isinstance(fids, list) or not fids:
            raise InvalidDocumentError(
                f"{where}(fs): `f` must be a non-empty array of non-negative integers"
            )
        for i, idx in enumerate(fids):
            _require_nonneg_int(idx, where=f"{where}.f[{i}]")
        return ObjectChunk(
            kind=ObjectChunkKind.FIELDS,
            fields=list(fids),
            range=_parse_range(raw["r"], where=f"{where}.r"),
        )
    if k == ObjectChunkKind.FIELD.value:
        _check_keys(raw, _FIELD_ENTRY_FIELDS, where=where)
        if "n" not in raw:
            raise InvalidDocumentError(f"{where}(f): missing `n`")
        _require_nonneg_int(raw["n"], where=f"{where}.n")
        v = _parse_value(_without_keys(raw, ("k", "n")), where=where)
        return ObjectChunk(
            kind=ObjectChunkKind.FIELD, name=raw["n"], value=v, range=v.range
        )
    raise InvalidDocumentError(f"{where}: unknown chunk kind {k!r}")


def _without_keys(d: dict[str, Any], drop: tuple[str, ...]) -> dict[str, Any]:
    return {k: v for k, v in d.items() if k not in drop}


# ---- Chunk-cache population ------------------------------------------------


def populate_chunk_caches(v: Optional[Value], keys: list[str]) -> None:
    """Walk the value tree once and fill cumulative-ordinal tables for arrays,
    name→hit lookups for objects, and the navigation Frame on every Value.

    Recurses through Item/Field wrapped values so every descendable Value has
    its caches primed before any cursor descent runs.
    """
    # Local import to dodge the cursor/nav/document import cycle.
    from .nav import frame_of

    _populate(v, keys, frame_of)


def _populate(v: Optional[Value], keys: list[str], frame_of) -> None:
    if v is None:
        return
    if v.kind is ValueKind.ARRAY:
        if not v.is_inline and v.array_chunks:
            cum = [0]
            hits: list[ArrayHit] = []
            running = 0
            for c in v.array_chunks:
                if c.kind is ArrayChunkKind.ITEM:
                    hits.append(
                        ArrayHit(kind=HitKind.ITEM, range=c.range, value=c.value)
                    )
                    running += 1
                    _populate(c.value, keys, frame_of)
                else:
                    hits.append(
                        ArrayHit(
                            kind=HitKind.ITEMS,
                            range=c.range,
                            value=None,
                            start_ordinal=running,
                        )
                    )
                    running += c.count
                cum.append(running)
            v._arr_cum = cum
            v._arr_hits = hits
    elif v.kind is ValueKind.OBJECT:
        if not v.is_inline and v.object_chunks:
            v._obj_lookup = _build_object_lookup(v.object_chunks, keys)
        for c in v.object_chunks:
            if c.kind is ObjectChunkKind.FIELD:
                _populate(c.value, keys, frame_of)
    # Pre-warm the Frame cache so cursor descent skips the lazy branch.
    frame_of(v)


def _build_object_lookup(
    chunks: list[ObjectChunk], keys: list[str]
) -> Optional[dict[str, ObjectHit]]:
    if not chunks or not keys:
        return None
    out: dict[str, ObjectHit] = {}
    for c in chunks:
        if c.kind is ObjectChunkKind.FIELD:
            if 0 <= c.name < len(keys):
                out.setdefault(
                    keys[c.name],
                    ObjectHit(kind=HitKind.FIELD, range=c.range, value=c.value),
                )
        else:
            hit = ObjectHit(kind=HitKind.FIELDS, range=c.range, value=None)
            for idx in c.fields:
                if 0 <= idx < len(keys):
                    out.setdefault(keys[idx], hit)
    return out or None
