//! Contract tests for the read-only wiki index reader (issue #121, slice 1).
//!
//! Fixtures are written inline — the real index is 143 MB and lives on
//! nodes, not in this repository. What must hold is the *shape*: version
//! fail-closed on both spellings, line-numbered manifest/chunk errors,
//! unknown-key preservation through the bincode cache, 0600 cache file, and
//! a status report that treats a missing cache as a report, not an error.

use danso_wiki::{cache, chunk, manifest, meta, status};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn write_index(dir: &Path, meta_version: u32, manifest_version: u32) {
    std::fs::create_dir_all(dir).expect("index dir");
    std::fs::write(
        dir.join("meta.json"),
        format!(
            r#"{{"version":{meta_version},"backend":"hashed","dims":4096,"chunks":2,"files":2,"builtAt":"2026-09-27T00:00:00Z","builtOn":"seoseo","skippedSecretChunks":7,"futureField":{{"kept":true}}}}"#
        ),
    )
    .expect("meta");
    std::fs::write(
        dir.join("manifest.jsonl"),
        format!(
            concat!(
                r#"{{"path":"pages/a.md","fileHash":"hash-a","mtime":1758900000.0,"bytes":100,"chunks":1,"backend":"hashed","indexVersion":{v},"indexedAt":"2026-09-27T00:00:00Z"}}"#,
                "\n",
                "\n",
                r#"{{"path":"pages/b.md","fileHash":"hash-b","mtime":1758900001.0,"bytes":200,"chunks":1,"backend":"hashed","denseModel":"jina-embeddings-v4","indexVersion":{v},"indexedAt":"2026-09-27T00:00:00Z"}}"#,
                "\n"
            ),
            v = manifest_version
        ),
    )
    .expect("manifest");
    std::fs::write(
        dir.join("chunks.jsonl"),
        concat!(
            r#"{"id":"a-0","path":"pages/a.md","startLine":1,"endLine":10,"heading":"A","headingStack":["A"],"level":1,"split":0,"bytes":100,"mtime":1758900000.0,"fileHash":"hash-a","snippet":"alpha","terms":{"alpha":1.5},"vector":{"11":0.25},"denseVector":[0.5,0.25],"denseBackend":"jina-api","denseModel":"jina-embeddings-v4","denseDims":2,"denseTask":"retrieval.passage","denseInputRole":"document","unknownChunkKey":[1,2]}"#,
            "\n",
            r#"{"id":"b-0","path":"pages/b.md","startLine":5,"endLine":6,"heading":"B","headingStack":["B"],"level":1,"split":1,"bytes":200,"mtime":1758900001.0,"fileHash":"hash-b","snippet":"beta","terms":{"beta":0.5},"vector":{}}"#,
            "\n"
        ),
    )
    .expect("chunks");
}

#[test]
fn meta_is_fail_closed_on_an_unsupported_version() {
    let temp = tempfile::tempdir().expect("temp");
    let index = temp.path().join("index");
    write_index(&index, 2, 3);
    let error = meta::IndexMeta::load(&index.join("meta.json")).expect_err("version 2 refused");
    assert!(
        error
            .to_string()
            .contains("unsupported wiki index version 2"),
        "the refusal must say what it found: {error:#}"
    );
}

#[test]
fn manifest_errors_name_the_offending_line() {
    let temp = tempfile::tempdir().expect("temp");
    let index = temp.path().join("index");
    std::fs::create_dir_all(&index).expect("index dir");
    std::fs::write(
        index.join("manifest.jsonl"),
        concat!(
            r#"{"path":"pages/a.md","fileHash":"h","mtime":1.0,"bytes":1,"chunks":1,"backend":"hashed","indexVersion":3,"indexedAt":"x"}"#,
            "\n",
            "{not json}\n"
        ),
    )
    .expect("manifest");
    let error = manifest::load(&index.join("manifest.jsonl")).expect_err("malformed line");
    assert!(
        error.to_string().contains("line 2"),
        "a dropped line is a stale file; the error must name the line: {error:#}"
    );
}

#[test]
fn manifest_is_fail_closed_on_an_unsupported_index_version() {
    let temp = tempfile::tempdir().expect("temp");
    let index = temp.path().join("index");
    write_index(&index, 3, 4);
    let error = manifest::load(&index.join("manifest.jsonl")).expect_err("indexVersion 4 refused");
    assert!(
        error.to_string().contains("unsupported indexVersion 4"),
        "both version spellings are checked: {error:#}"
    );
}

#[test]
fn chunks_parse_and_keep_unknown_keys() {
    let temp = tempfile::tempdir().expect("temp");
    let index = temp.path().join("index");
    write_index(&index, 3, 3);
    let chunks = chunk::load(&index.join("chunks.jsonl")).expect("chunks parse");
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].dense_model.as_deref(), Some("jina-embeddings-v4"));
    assert_eq!(chunks[1].dense_vector, None);
    assert_eq!(
        chunks[0].extra.get("unknownChunkKey"),
        Some(&"[1,2]".to_string()),
        "a field this crate does not know must survive the parse"
    );
    // The real file is ~143 MB of these; the reader must skip nothing and
    // invent nothing — blank lines included.
    assert_eq!(chunks[0].terms.get("alpha"), Some(&1.5));
    // The writer emits `split` as a small integer (0–5 in the real index);
    // the reader keeps it verbatim.
    assert_eq!(chunks[0].split, Some(0));
    assert_eq!(chunks[1].split, Some(1));
}

#[test]
fn the_cache_round_trips_at_0600() {
    let temp = tempfile::tempdir().expect("temp");
    let index = temp.path().join("index");
    let cache_dir = temp.path().join("cache");
    write_index(&index, 3, 3);

    cache::build(&index, &cache_dir).expect("cache build");
    let mode = std::fs::metadata(cache::cache_path(&cache_dir))
        .expect("cache file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the cache holds wiki content: owner-only");

    let cached = cache::load(&cache_dir).expect("cache decode");
    assert_eq!(cached.chunks.len(), 2);
    assert!(cached.meta.extra.contains_key("futureField"));
    assert_eq!(
        cached.chunks[0].extra.get("unknownChunkKey"),
        Some(&"[1,2]".to_string()),
        "bincode must round-trip the preserved unknown keys"
    );
    assert!(matches!(
        cache::inspect(&index, &cache_dir).expect("inspect"),
        cache::CacheState::Fresh
    ));
}

#[test]
fn a_cache_built_from_other_sources_is_stale_not_fresh() {
    let temp = tempfile::tempdir().expect("temp");
    let index = temp.path().join("index");
    let cache_dir = temp.path().join("cache");
    write_index(&index, 3, 3);
    cache::build(&index, &cache_dir).expect("cache build");

    // An rsync can replace a source file without a rebuild; the byte size in
    // the key is what catches it.
    std::fs::write(index.join("chunks.jsonl"), "{}\n").expect("rewrite chunks");
    let state = cache::inspect(&index, &cache_dir).expect("inspect");
    match state {
        cache::CacheState::Stale { cached, current } => {
            assert_ne!(cached.chunks_bytes, current.chunks_bytes);
        }
        other => panic!("expected stale, got {other:?}"),
    }
}

#[test]
fn a_bash_managed_cache_dir_mode_is_accepted_and_the_file_stays_0600() {
    let temp = tempfile::tempdir().expect("temp");
    let index = temp.path().join("index");
    let cache_dir = temp.path().join("cache");
    write_index(&index, 3, 3);
    // The real wiki-cache tree is bash-managed and sits at the sync's umask
    // default; the reader must still be able to add its cache there.
    std::fs::create_dir_all(&cache_dir).expect("cache dir");
    std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o755))
        .expect("loosen dir");

    cache::build(&index, &cache_dir).expect("build into an operator-managed dir");
    let mode = std::fs::metadata(cache::cache_path(&cache_dir))
        .expect("cache file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the file is the privacy boundary: owner-only");
}

#[test]
fn an_undecodable_cache_is_invalid_not_fatal() {
    let temp = tempfile::tempdir().expect("temp");
    let index = temp.path().join("index");
    let cache_dir = temp.path().join("cache");
    write_index(&index, 3, 3);
    std::fs::create_dir_all(&cache_dir).expect("cache dir");
    std::fs::write(cache::cache_path(&cache_dir), b"not a cache").expect("garbage cache");
    assert!(matches!(
        cache::inspect(&index, &cache_dir).expect("inspect"),
        cache::CacheState::Invalid { .. }
    ));
}

#[test]
fn an_absent_cache_is_a_report_not_an_error() {
    let temp = tempfile::tempdir().expect("temp");
    let index = temp.path().join("index");
    let cache_dir = temp.path().join("cache");
    write_index(&index, 3, 3);
    assert!(matches!(
        cache::inspect(&index, &cache_dir).expect("inspect"),
        cache::CacheState::Absent
    ));
}

#[test]
fn status_reports_presence_and_names_missing_files() {
    let temp = tempfile::tempdir().expect("temp");
    let index = temp.path().join("index");
    let cache_dir = temp.path().join("cache");
    write_index(&index, 3, 3);
    std::fs::create_dir_all(cache_dir.join("pages")).expect("cache tree");
    std::fs::write(cache_dir.join("pages/a.md"), "content").expect("cached file");

    let report = status::status(&index, &cache_dir).expect("status");
    assert_eq!(report.manifest_entries, 2);
    assert_eq!(report.cache_files.present, 1);
    assert_eq!(report.cache_files.missing, 1);
    assert_eq!(
        report.cache_files.missing_samples,
        vec!["pages/b.md".to_string()]
    );
    let text = report.to_text();
    assert!(text.contains("dims 4096"), "{text}");
    assert!(text.contains("cache-dir files: 1/2 present"), "{text}");
    assert!(text.contains("missing: pages/b.md"), "{text}");
    assert!(text.contains("cache: absent"), "{text}");
}

#[test]
fn status_fails_closed_on_an_index_it_cannot_read() {
    let temp = tempfile::tempdir().expect("temp");
    let index = temp.path().join("index");
    let cache_dir = temp.path().join("cache");
    write_index(&index, 9, 3);
    let error = status::status(&index, &cache_dir).expect_err("version 9 refused");
    assert!(
        error
            .to_string()
            .contains("unsupported wiki index version 9"),
        "{error:#}"
    );
}
