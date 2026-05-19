"""Tests for the Python JRIF reader."""

from __future__ import annotations

import io
import json
from pathlib import Path

import pytest

import jrif


HERE = Path(__file__).resolve().parent
SAMPLE_JSON = HERE / "sample.json"
SAMPLE_JRIF = HERE / "sample.json.jrif"


@pytest.fixture
def payload() -> bytes:
    return SAMPLE_JSON.read_bytes()


@pytest.fixture
def sidecar() -> bytes:
    return SAMPLE_JRIF.read_bytes()


@pytest.fixture
def idx(sidecar: bytes, payload: bytes) -> jrif.Index:
    return jrif.open(sidecar, payload)


class RecordingSource:
    """Wraps a BytesSource and records every (offset, length) it serves."""

    def __init__(self, buf: bytes) -> None:
        self._inner = jrif.BytesSource(buf)
        self.fetches: list[tuple[int, int]] = []

    def read_exact_at(self, offset: int, length: int) -> bytes:
        self.fetches.append((offset, length))
        return self._inner.read_exact_at(offset, length)


# ---------------------------------------------------------------------------
# Navigation: chunk-index descent vs parse fallback
# ---------------------------------------------------------------------------


def test_chunk_index_descent_does_not_fetch(sidecar: bytes, payload: bytes) -> None:
    src = RecordingSource(payload)
    idx = jrif.open(sidecar, src)
    _ = idx.root["metadata"]
    assert src.fetches == []


def test_value_fetches_exactly_the_chunk_range(sidecar: bytes, payload: bytes) -> None:
    src = RecordingSource(payload)
    idx = jrif.open(sidecar, src)
    metadata = idx.root["metadata"]
    rng = metadata.range
    assert rng is not None
    assert isinstance(metadata.value(), dict)
    assert len(src.fetches) == 1
    assert src.fetches[0] == (rng.start, rng.length)


def test_nested_navigation_through_chunks(sidecar: bytes, payload: bytes) -> None:
    src = RecordingSource(payload)
    idx = jrif.open(sidecar, src)
    assert idx.root["records"][1]["name"].as_str() == "bob"
    total = sum(length for _, length in src.fetches)
    assert total < len(payload)


def test_root_materialization(idx: jrif.Index) -> None:
    v = idx.root.value()
    assert isinstance(v, dict)
    assert "records" in v


def test_fallback_to_parse_when_index_cant_drill(
    sidecar: bytes, payload: bytes
) -> None:
    src = RecordingSource(payload)
    idx = jrif.open(sidecar, src)
    version = idx.root["metadata"]["version"]
    # Type hint is unknown for a cursor with pending segments.
    assert version.json_type_hint() is None
    assert int(version) == 1
    assert len(src.fetches) == 1


def test_field_not_found_after_fallback(idx: jrif.Index) -> None:
    with pytest.raises(jrif.NotFoundError) as exc:
        idx.root["metadata"]["nope"].value()
    assert str(exc.value.path) == "$.metadata.nope"


def test_index_out_of_bounds(idx: jrif.Index) -> None:
    with pytest.raises(jrif.NotFoundError):
        idx.root["records"].index(99).value()


def test_negative_index(idx: jrif.Index) -> None:
    assert idx.root["records"][-1]["name"].as_str() == "carol"


def test_bytes_returns_raw_when_resolved(idx: jrif.Index, payload: bytes) -> None:
    metadata = idx.root["metadata"]
    rng = metadata.range
    assert rng is not None
    assert metadata.bytes() == payload[rng.start : rng.end]


def test_len_from_chunk_index_without_fetch(sidecar: bytes, payload: bytes) -> None:
    src = RecordingSource(payload)
    idx = jrif.open(sidecar, src)
    assert len(idx.root["records"]) >= 2
    assert src.fetches == []


def test_path_rendering_on_deep_error(idx: jrif.Index) -> None:
    # records[0] is an object, not an array, so .index(7) on it raises.
    with pytest.raises(jrif.TypeMismatchError) as exc:
        idx.root["records"][0].index(7).value()
    assert str(exc.value.path) == "$.records[0][7]"


def test_json_type_hint_for_chunk_backed_cursors(idx: jrif.Index) -> None:
    assert idx.root.json_type_hint() == jrif.JsonType.OBJECT
    assert idx.root["records"].json_type_hint() == jrif.JsonType.ARRAY


# ---------------------------------------------------------------------------
# Operator overloads — the point of the Python SDK
# ---------------------------------------------------------------------------


def test_subscript_by_string_descends_into_object(idx: jrif.Index) -> None:
    assert idx.root["id"].as_str() == "doc-1"


def test_subscript_by_int_descends_into_array(idx: jrif.Index) -> None:
    assert idx.root["records"][0]["name"].as_str() == "alice"


def test_contains_on_object_keys(idx: jrif.Index) -> None:
    assert "records" in idx.root
    assert "metadata" in idx.root
    assert "nope" not in idx.root


def test_iter_over_array_yields_cursors(idx: jrif.Index) -> None:
    names = [c["name"].as_str() for c in idx.root["records"]]
    assert names == ["alice", "bob", "carol"]


def test_iter_over_object_yields_keys(idx: jrif.Index) -> None:
    assert list(iter(idx.root)) == ["id", "metadata", "records"]


def test_len_string(idx: jrif.Index) -> None:
    assert len(idx.root["id"]) == len("doc-1")


def test_int_coerces_number_cursor(idx: jrif.Index) -> None:
    assert int(idx.root["metadata"]["version"]) == 1


def test_float_coerces_number_cursor(idx: jrif.Index) -> None:
    assert float(idx.root["records"][0]["score"]) == pytest.approx(0.91)


def test_str_renders_underlying_value(idx: jrif.Index) -> None:
    assert str(idx.root["records"][0]["name"]) == "alice"


def test_eq_against_python_value(idx: jrif.Index) -> None:
    assert idx.root["records"][0]["name"] == "alice"
    assert idx.root["metadata"]["version"] == 1
    assert idx.root["records"][0]["name"] != "bob"


def test_keyerror_via_dict_access(idx: jrif.Index) -> None:
    # NotFoundError subclasses KeyError so dict-style consumers catch it.
    with pytest.raises(KeyError):
        idx.root["metadata"]["nope"].value()


def test_indexerror_via_list_access(idx: jrif.Index) -> None:
    with pytest.raises(IndexError):
        idx.root["records"][99].value()


def test_attribute_descent_not_supported_for_keys(idx: jrif.Index) -> None:
    # Deliberately don't override __getattr__ for navigation: it would
    # conflict with introspection (.path, .range, .value, …).
    with pytest.raises(AttributeError):
        _ = idx.root.records  # type: ignore[attr-defined]


def test_items_and_values_iterate_like_dict(idx: jrif.Index) -> None:
    rec0 = idx.root["records"][0]
    out = {k: c.value() for k, c in rec0.items()}
    assert out["id"] == 1
    assert out["name"] == "alice"
    assert len(list(rec0.values())) == len(list(rec0.keys()))


# ---------------------------------------------------------------------------
# Entries / iteration through pending chunks
# ---------------------------------------------------------------------------


def test_entries_preserves_source_order_through_pending(idx: jrif.Index) -> None:
    rec = idx.root["records"][0]
    assert rec.keys() == ["id", "name", "score", "notes"]


def test_iter_on_chunked_array_records(idx: jrif.Index) -> None:
    recs = list(idx.root["records"])
    assert len(recs) == 3
    assert recs[2]["name"].as_str() == "carol"


# ---------------------------------------------------------------------------
# Inline root
# ---------------------------------------------------------------------------


def test_inline_primitive_root() -> None:
    sidecar = json.dumps({"jrif": "v0", "root": {"t": "v", "v": 42}}).encode()
    src = RecordingSource(b"")
    idx = jrif.open(sidecar, src)
    assert int(idx.root) == 42
    assert idx.root.bytes() == b"42"
    assert src.fetches == []


def test_inline_object_root() -> None:
    sidecar = json.dumps(
        {"jrif": "v0", "root": {"t": "v", "v": {"a": 1, "b": 2}}}
    ).encode()
    idx = jrif.open(sidecar, b"")
    assert list(idx.root) == ["a", "b"]
    assert int(idx.root["a"]) == 1


# ---------------------------------------------------------------------------
# Partial coverage — JRIF v0 allows objects whose chunks cover only a subset
# ---------------------------------------------------------------------------


PARTIAL_PAYLOAD = b'{"covered":1,"uncovered":2}'
PARTIAL_SIDECAR = json.dumps(
    {
        "jrif": "v0",
        "keys": ["covered"],
        "root": {
            "t": "o",
            "r": [0, len(PARTIAL_PAYLOAD)],
            "c": [{"k": "f", "n": 0, "t": "v", "v": 1}],
        },
    }
).encode()


def test_uncovered_field_resolves_via_fallback() -> None:
    idx = jrif.open(PARTIAL_SIDECAR, PARTIAL_PAYLOAD)
    assert int(idx.root["covered"]) == 1
    assert int(idx.root["uncovered"]) == 2


def test_keys_lists_uncovered_fields_too() -> None:
    idx = jrif.open(PARTIAL_SIDECAR, PARTIAL_PAYLOAD)
    assert idx.root.keys() == ["covered", "uncovered"]


# ---------------------------------------------------------------------------
# Document parsing
# ---------------------------------------------------------------------------


def test_rejects_unknown_top_level_field() -> None:
    bad = json.dumps({"jrif": "v0", "root": {"t": "v", "v": 1}, "extra": True}).encode()
    with pytest.raises(jrif.InvalidDocumentError):
        jrif.open(bad, b"")


def test_rejects_unknown_jrif_version() -> None:
    bad = json.dumps({"jrif": "v1", "root": {"t": "v", "v": 1}}).encode()
    with pytest.raises(jrif.InvalidDocumentError) as exc:
        jrif.open(bad, b"")
    assert exc.value.jrif_version == "v1"


def test_rejects_zero_length_range() -> None:
    bad = json.dumps({"jrif": "v0", "root": {"t": "s", "r": [0, 0]}}).encode()
    with pytest.raises(jrif.InvalidDocumentError):
        jrif.open(bad, b"")


# ---------------------------------------------------------------------------
# Sources
# ---------------------------------------------------------------------------


def test_file_source_end_to_end(sidecar: bytes) -> None:
    with jrif.FileSource(SAMPLE_JSON) as src:
        idx = jrif.open(sidecar, src)
        assert idx.root["records"][0]["name"].as_str() == "alice"


def test_open_accepts_path_directly(sidecar: bytes) -> None:
    idx = jrif.open(sidecar, str(SAMPLE_JSON))
    assert int(idx.root["metadata"]["version"]) == 1


def test_open_accepts_file_like_payload() -> None:
    with open(SAMPLE_JRIF, "rb") as sf, open(SAMPLE_JSON, "rb") as f:
        idx = jrif.open(sf, f)
        assert int(idx.root["metadata"]["version"]) == 1


def test_open_accepts_bytesio_for_payload(payload: bytes, sidecar: bytes) -> None:
    idx = jrif.open(sidecar, io.BytesIO(payload))
    assert idx.root["records"][2]["name"].as_str() == "carol"


def test_open_accepts_file_like_sidecar(payload: bytes) -> None:
    with open(SAMPLE_JRIF, "rb") as sf:
        idx = jrif.open(sf, payload)
    assert int(idx.root["metadata"]["version"]) == 1


def test_file_object_source_short_read_raises() -> None:
    src = jrif.FileObjectSource(io.BytesIO(b"abc"))
    with pytest.raises(OSError):
        src.read_exact_at(0, 5)
