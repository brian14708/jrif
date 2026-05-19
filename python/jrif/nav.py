"""Internal navigation helpers: chunk frames and chunk-hit resolution.

The cursor descends through the chunk index by asking a navigation *frame*
where a given field name or array ordinal lives. A miss is not an error — it
just defers the work to a parse step at the next leaf accessor.
"""

from __future__ import annotations

import bisect
from dataclasses import dataclass
from typing import Optional

from .document import ArrayChunkKind, ArrayHit, ObjectHit, Value, ValueKind


WRAP_NONE = 0
WRAP_ARRAY = 1
WRAP_OBJECT = 2


FRAME_DONE = 0
FRAME_ARRAY = 1
FRAME_OBJECT = 2


@dataclass(slots=True)
class Frame:
    """Live navigation context for a cursor position."""

    kind: int  # FRAME_DONE | FRAME_ARRAY | FRAME_OBJECT
    value: Optional[Value] = None  # the source Value, when chunked


DONE_FRAME = Frame(kind=FRAME_DONE)


def frame_of(v: Value) -> Frame:
    """Build (or return the cached) navigation frame for ``v``.

    Ranged compounds with non-empty chunks yield a chunk frame; everything else
    (inline values, primitives, unchunked compounds) yields the shared
    done-frame. The result is memoized on the ``Value`` so repeated descents
    skip the dispatch.
    """
    cached = v._frame
    if cached is not None:
        return cached
    if v.kind is ValueKind.ARRAY and not v.is_inline and v.array_chunks:
        out = Frame(kind=FRAME_ARRAY, value=v)
    elif v.kind is ValueKind.OBJECT and not v.is_inline and v.object_chunks:
        out = Frame(kind=FRAME_OBJECT, value=v)
    else:
        out = DONE_FRAME
    v._frame = out
    return out


def find_array_match(frame: Frame, ordinal: int) -> Optional[ArrayHit]:
    """Locate the chunk covering ``ordinal``. ``None`` when out of range."""
    if frame.kind != FRAME_ARRAY or frame.value is None:
        return None
    v = frame.value
    cum = v._arr_cum
    if cum is None or ordinal < 0 or ordinal >= cum[-1]:
        return None
    # Largest i with cum[i] <= ordinal; bisect_right gives first > ordinal.
    hi = bisect.bisect_right(cum, ordinal)
    if hi == 0 or hi > len(v.array_chunks):
        return None
    hits = v._arr_hits
    return hits[hi - 1] if hits is not None else None


def find_object_match(frame: Frame, name: str) -> Optional[ObjectHit]:
    """Locate the chunk covering field ``name``. ``None`` when not chunked."""
    if frame.kind != FRAME_OBJECT or frame.value is None:
        return None
    lookup = frame.value._obj_lookup
    if lookup is None:
        return None
    return lookup.get(name)


def array_len(frame: Frame) -> Optional[int]:
    """O(1) array length from the cumulative table, if available."""
    if frame.kind != FRAME_ARRAY or frame.value is None:
        return None
    cum = frame.value._arr_cum
    if not cum:
        return None
    return cum[-1]


def iter_array_chunks(frame: Frame):
    """Yield ``(ordinal, ArrayHit)`` per logical item in source order.

    Walks the chunks linearly (O(M) for M chunks, then O(1) per item) — used
    by :meth:`Cursor.__iter__` to avoid an O(log M) bisect per ordinal.
    """
    if frame.kind != FRAME_ARRAY or frame.value is None:
        return
    v = frame.value
    hits = v._arr_hits
    if hits is None:
        return
    ordinal = 0
    for c, hit in zip(v.array_chunks, hits):
        if c.kind is ArrayChunkKind.ITEM:
            yield ordinal, hit
            ordinal += 1
        else:
            start = ordinal
            for i in range(c.count):
                yield start + i, hit
            ordinal += c.count
