//! End-to-end tests: build a JRIF sidecar, open it via Index, and navigate
//! with a Cursor over an in-memory payload.

use bytes::Bytes;
use jrif::{BufferReader, Error, Index, Indexer};
use std::fmt::Write as _;

const SAMPLE: &str = r#"{
  "id": "doc-1",
  "metadata": {"version": 1, "tags": ["alpha", "beta"]},
  "records": [
    {"id": 1, "name": "alice", "score": 0.91, "notes": "first  record  with  padding"},
    {"id": 2, "name": "bob",   "score": 0.42, "notes": "second record  with  padding"},
    {"id": 3, "name": "carol", "score": 0.73, "notes": "third  record  with  padding"}
  ]
}"#;

fn build_pair() -> (Bytes, Bytes) {
    let payload = Bytes::from_static(SAMPLE.as_bytes());
    let jrif = Indexer::new().min_chunk_bytes(16).build(&payload).unwrap();
    (payload, jrif)
}

#[tokio::test]
async fn deserialize_through_chunked_record() {
    let (payload, jrif) = build_pair();
    let index = Index::open(&jrif, payload).await.unwrap();
    let name: String = index
        .root()
        .get("records")
        .index(1)
        .get("name")
        .deserialize()
        .await
        .unwrap();
    assert_eq!(name, "bob");
}

#[tokio::test]
async fn iter_visits_every_record() {
    let (payload, jrif) = build_pair();
    let index = Index::open(&jrif, payload).await.unwrap();
    let records = index.root().get("records");
    let mut names = Vec::new();
    for item in records.iter().await.unwrap() {
        let n: String = item.get("name").deserialize().await.unwrap();
        names.push(n);
    }
    assert_eq!(names, vec!["alice", "bob", "carol"]);
}

#[tokio::test]
async fn iter_size_hint_is_exact() {
    let (payload, jrif) = build_pair();
    let index = Index::open(&jrif, payload).await.unwrap();
    let it = index.root().get("records").iter().await.unwrap();
    assert_eq!(it.size_hint(), (3, Some(3)));
}

#[tokio::test]
async fn cursor_range_only_when_fully_resolved() {
    let (payload, jrif) = build_pair();
    let index = Index::open(&jrif, payload).await.unwrap();
    // root.get("records").index(0) lands on a chunk-indexed Item — range is exact.
    let c = index.root().get("records").index(0);
    assert!(c.range().is_some());
    // root.get("metadata").get("version") falls back to parse — range is None.
    let c = index.root().get("metadata").get("version");
    assert!(c.range().is_none());
}

#[tokio::test]
async fn field_not_found_includes_path_context() {
    let (payload, jrif) = build_pair();
    let index = Index::open(&jrif, payload).await.unwrap();
    let err = index
        .root()
        .get("metadata")
        .get("missing_field")
        .value()
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("not found"));
    assert!(msg.contains(".metadata"));
    assert!(msg.contains(".missing_field"));
}

#[tokio::test]
async fn index_out_of_bounds_includes_path_context() {
    let (payload, jrif) = build_pair();
    let index = Index::open(&jrif, payload).await.unwrap();
    let err = index
        .root()
        .get("records")
        .index(99)
        .value()
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains(".records") && msg.contains("[99]"));
}

#[tokio::test]
async fn entries_visits_every_top_level_field() {
    let (payload, jrif) = build_pair();
    let index = Index::open(&jrif, payload).await.unwrap();
    let mut seen: Vec<String> = Vec::new();
    for (name, _cur) in index.root().entries().await.unwrap() {
        seen.push(String::from(name));
    }
    assert_eq!(seen, vec!["id", "metadata", "records"]);
}

#[tokio::test]
async fn entries_works_through_chunk_fragment() {
    // metadata is below the chunking threshold — entries() falls back to
    // fetch+parse to enumerate its fields.
    let (payload, jrif) = build_pair();
    let index = Index::open(&jrif, payload).await.unwrap();
    let mut seen: Vec<String> = Vec::new();
    for (name, _cur) in index.root().get("metadata").entries().await.unwrap() {
        seen.push(String::from(name));
    }
    assert_eq!(seen, vec!["version", "tags"]);
}

#[tokio::test]
async fn entries_yields_navigable_cursors() {
    let (payload, jrif) = build_pair();
    let index = Index::open(&jrif, payload).await.unwrap();
    // Iterate records[0] entries and verify each cursor can deserialize.
    let item = index.root().get("records").index(0);
    let mut id_seen = None;
    let mut name_seen = None;
    for (key, cur) in item.entries().await.unwrap() {
        match &*key {
            "id" => id_seen = Some(cur.deserialize::<u32>().await.unwrap()),
            "name" => name_seen = Some(cur.deserialize::<String>().await.unwrap()),
            _ => {}
        }
    }
    assert_eq!(id_seen, Some(1));
    assert_eq!(name_seen.as_deref(), Some("alice"));
}

#[tokio::test]
async fn entries_on_array_reports_type_mismatch() {
    let (payload, jrif) = build_pair();
    let index = Index::open(&jrif, payload).await.unwrap();
    let Err(err) = index.root().get("records").entries().await else {
        panic!("expected TypeMismatch");
    };
    assert!(matches!(err, Error::TypeMismatch { .. }));
}

// Spec §Object Field Chunks: chunks MAY cover only a subset of an object's
// fields. `entries()` must surface uncovered fields by falling through to a
// parse of the payload bytes rather than trusting the chunk index alone.
#[tokio::test]
async fn entries_iterates_partially_chunked_object() {
    let payload_text = r#"{"covered":[1,2,3,4,5,6,7,8,9,10],"uncovered":[1,2,3,4,5,6,7,8,9,10]}"#;
    let payload = Bytes::from_static(payload_text.as_bytes());
    // min_chunk_bytes(8) so both array values get individual `field` chunks.
    let jrif = Indexer::new().min_chunk_bytes(8).build(&payload).unwrap();

    // Prune the "uncovered" field chunk from the sidecar, leaving the spec-valid
    // case where the root object's chunks list covers only "covered".
    let mut doc: serde_json::Value = serde_json::from_slice(&jrif).unwrap();
    let uncovered_idx = doc
        .pointer("/keys")
        .and_then(|v| v.as_array())
        .expect("keys array")
        .iter()
        .position(|k| k.as_str() == Some("uncovered"))
        .expect("uncovered in keys") as u64;
    let chunks = doc
        .pointer_mut("/root/c")
        .and_then(|v| v.as_array_mut())
        .expect("root.c array");
    let before = chunks.len();
    chunks.retain(|c| c.get("n").and_then(serde_json::Value::as_u64) != Some(uncovered_idx));
    assert_eq!(chunks.len(), before - 1, "expected one chunk pruned");
    let modified = Bytes::from(serde_json::to_vec(&doc).unwrap());

    let index = Index::open(&modified, payload).await.unwrap();
    let mut seen: Vec<String> = Vec::new();
    for (name, _cur) in index.root().entries().await.unwrap() {
        seen.push(String::from(name));
    }
    assert_eq!(seen, vec!["covered", "uncovered"]);

    // Both values must also resolve through the resulting cursors.
    for (name, cur) in index.root().entries().await.unwrap() {
        let arr: Vec<u32> = cur.deserialize().await.unwrap();
        assert_eq!(arr, (1..=10).collect::<Vec<_>>(), "values for {name}");
    }
}

#[tokio::test]
async fn deserialize_into_user_struct() {
    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct Record {
        id: u32,
        name: String,
    }

    let (payload, jrif) = build_pair();
    let index = Index::open(&jrif, payload).await.unwrap();
    let bob: Record = index
        .root()
        .get("records")
        .index(1)
        .deserialize()
        .await
        .unwrap();
    assert_eq!(
        bob,
        Record {
            id: 2,
            name: "bob".to_owned()
        }
    );
}

#[tokio::test]
async fn cache_layer_serves_navigation_through_one_block() {
    let (payload, jrif) = build_pair();
    let cached = BufferReader::new(payload)
        .block_size(256)
        .max_bytes(64 * 1024);
    let index = Index::open(&jrif, cached).await.unwrap();
    // Two deferred reads inside metadata should reuse the cached block.
    let _ = index
        .root()
        .get("metadata")
        .get("version")
        .value()
        .await
        .unwrap();
    let _ = index
        .root()
        .get("metadata")
        .get("tags")
        .value()
        .await
        .unwrap();
    let used = index.source().cached_entries();
    assert!(used >= 1);
}

#[tokio::test]
async fn shared_index_across_tasks() {
    let (payload, jrif) = build_pair();
    let index = std::sync::Arc::new(Index::open(&jrif, payload).await.unwrap());
    let mut handles = Vec::new();
    for _ in 0..4 {
        let idx = std::sync::Arc::clone(&index);
        handles.push(tokio::spawn(async move {
            let n: String = idx
                .root()
                .get("records")
                .index(0)
                .get("name")
                .deserialize()
                .await
                .unwrap();
            assert_eq!(n, "alice");
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

// Inline literal values are deserialized without any I/O against the payload
// source. Root-level primitives are inlined directly in the JRIF doc; reading
// them must not fetch any payload bytes.
#[tokio::test]
async fn inline_root_skips_payload_fetch() {
    for (payload_bytes, expected) in [
        (&b"42"[..], serde_json::json!(42)),
        (b"true", serde_json::json!(true)),
        (b"null", serde_json::json!(null)),
        (b"\"ok\"", serde_json::json!("ok")),
        (b"[]", serde_json::json!([])),
        (b"{}", serde_json::json!({})),
    ] {
        let payload = Bytes::copy_from_slice(payload_bytes);
        let jrif = Indexer::new().build(&payload).unwrap();
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let recorder = RecordingPayload {
            inner: payload.clone(),
            count: counter.clone(),
        };
        let index = Index::open(&jrif, recorder).await.unwrap();
        let got: serde_json::Value = index.root().deserialize().await.unwrap();
        assert_eq!(got, expected, "for payload {payload_bytes:?}");
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "inline root should not fetch any bytes (payload {payload_bytes:?})"
        );
    }
}

// Compound children with inline empty values are also resolved without I/O:
// e.g. an object with `"tags": []` lets us descend to `tags` and ask its
// length / contents without fetching.
#[tokio::test]
async fn inline_compound_child_skips_payload_fetch() {
    // min_chunk_bytes(8) so `tags` (empty array, ranged-or-inline?) and
    // `meta` (empty object) become individual `field` chunks wrapping inline
    // empty Values.
    let payload_text = r#"{"tags":[],"meta":{},"extra":"x"}"#;
    let payload = Bytes::from_static(payload_text.as_bytes());
    let jrif = Indexer::new().min_chunk_bytes(8).build(&payload).unwrap();

    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let recorder = RecordingPayload {
        inner: payload.clone(),
        count: counter.clone(),
    };
    let index = Index::open(&jrif, recorder).await.unwrap();
    let before = counter.load(std::sync::atomic::Ordering::Relaxed);

    // Empty array .len() should not fetch.
    let n = index.root().get("tags").len().await.unwrap();
    assert_eq!(n, 0);
    // Empty object .entries() should not fetch.
    let entries = index.root().get("meta").entries().await.unwrap();
    let names: Vec<_> = entries.map(|(name, _)| String::from(name)).collect();
    assert!(names.is_empty());

    let after = counter.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        after, before,
        "inline empty compounds should not trigger fetches"
    );
}

// Arrays of records → `keys` built; navigation through every record still
// resolves field names correctly via `Fields` chunks.
#[tokio::test]
async fn keys_round_trip() {
    let mut payload = String::from("[");
    for i in 0..6 {
        if i > 0 {
            payload.push(',');
        }
        let _ = write!(payload, "{{\"id\":{i},\"name\":\"row{i}\",\"score\":{i}}}");
    }
    payload.push(']');
    let payload = Bytes::from(payload.into_bytes());
    let jrif = Indexer::new().min_chunk_bytes(8).build(&payload).unwrap();

    // Sanity: parse the JRIF and confirm `keys` is present.
    let doc: serde_json::Value = serde_json::from_slice(&jrif).unwrap();
    let keys = doc.get("keys").and_then(|v| v.as_array()).expect("keys");
    let keys_str: Vec<&str> = keys.iter().filter_map(|v| v.as_str()).collect();
    assert!(keys_str.contains(&"id") && keys_str.contains(&"name") && keys_str.contains(&"score"));

    let index = Index::open(&jrif, payload).await.unwrap();
    for i in 0..6u64 {
        let name: String = index
            .root()
            .index(i)
            .get("name")
            .deserialize()
            .await
            .unwrap();
        assert_eq!(name, format!("row{i}"));
    }
}

// Helper: a payload Source that records how many fetches it received.
struct RecordingPayload {
    inner: Bytes,
    count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl jrif::Source for RecordingPayload {
    fn read_exact_at(
        &self,
        offset: u64,
        len: usize,
    ) -> impl std::future::Future<Output = std::io::Result<Bytes>> + Send {
        let inner = self.inner.clone();
        let count = self.count.clone();
        async move {
            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let start = usize::try_from(offset).unwrap();
            let end = start + len;
            Ok(inner.slice(start..end))
        }
    }
}
