"""Micro-benchmark for hot cursor paths.

Run with: ``uv run --extra test python tests/bench.py``
"""

from __future__ import annotations

import time
from pathlib import Path

import jrif


HERE = Path(__file__).resolve().parent
SAMPLE_JSON = HERE / "sample.json"
SAMPLE_JRIF = HERE / "sample.json.jrif"


def bench(name: str, fn, n: int = 100_000) -> float:
    # Warmup
    for _ in range(min(1000, n // 10)):
        fn()
    t0 = time.perf_counter()
    for _ in range(n):
        fn()
    dt = time.perf_counter() - t0
    per_op_us = dt * 1e6 / n
    print(f"{name:48s} {dt * 1e3:7.1f} ms  ({per_op_us:7.2f} µs/op, {n // 1000}k ops)")
    return dt


def main() -> None:
    sidecar = SAMPLE_JRIF.read_bytes()
    payload = SAMPLE_JSON.read_bytes()
    idx = jrif.open(sidecar, payload)

    # Chunk-only descent (no I/O)
    bench("descend 1 level (chunk index)", lambda: idx.root["metadata"])
    bench("descend 2 levels (chunk index)", lambda: idx.root["records"][1])
    bench("descend 3 levels (mixed)", lambda: idx.root["records"][1]["name"])

    # Leaf accessors
    bench(
        "as_str at depth 3 (slow path)", lambda: idx.root["records"][1]["name"].as_str()
    )
    bench("int at depth 2 (slow path)", lambda: int(idx.root["metadata"]["version"]))

    # Pre-built cursors (skip descent overhead)
    name_cursor = idx.root["records"][1]["name"]
    bench("as_str on pre-built cursor", lambda: name_cursor.as_str())

    metadata = idx.root["metadata"]
    bench("metadata value() (chunk-resolved range)", lambda: metadata.value(), n=10_000)

    # Iteration
    records = idx.root["records"]
    bench(
        "iterate 3 records, read name",
        lambda: [c["name"].as_str() for c in records],
        n=10_000,
    )

    # Len fast path
    bench("len(records) (chunk index)", lambda: len(records))


if __name__ == "__main__":
    main()
