use std::collections::BTreeMap;
use std::path::Path as StdPath;
use std::time::SystemTime;

use anyhow::{Context, Result};
use futures::StreamExt;
use object_store::path::Path as ObjPath;
use object_store::ObjectStore;

use crate::plan::RemoteMeta;

/// Lists everything under `prefix`, keyed by the part of the key *after*
/// the prefix — so it lines up directly with `walk_local`'s
/// root-relative keys for `plan_sync` to compare.
pub async fn list_remote(
    store: &dyn ObjectStore,
    prefix: &str,
) -> Result<BTreeMap<String, RemoteMeta>> {
    let prefix_path = ObjPath::from(prefix);
    let mut stream = store.list(Some(&prefix_path));
    let mut result = BTreeMap::new();
    let strip = format!("{}/", prefix.trim_matches('/'));

    while let Some(meta) = stream.next().await {
        let meta = meta.context("listing remote objects")?;
        let full_key = meta.location.to_string();
        let rel = full_key
            .strip_prefix(&strip)
            .unwrap_or(&full_key)
            .to_string();
        let last_modified: SystemTime = meta.last_modified.into();
        result.insert(
            rel,
            RemoteMeta {
                size: meta.size as u64,
                last_modified,
                e_tag: meta.e_tag,
            },
        );
    }
    Ok(result)
}

pub async fn upload_file(
    store: &dyn ObjectStore,
    prefix: &str,
    local_root: &StdPath,
    rel_path: &str,
) -> Result<()> {
    let full_path = local_root.join(rel_path);
    let bytes = tokio::fs::read(&full_path)
        .await
        .with_context(|| format!("reading {}", full_path.display()))?;
    let key = ObjPath::from(format!("{}/{rel_path}", prefix.trim_matches('/')));
    store
        .put(&key, bytes.into())
        .await
        .with_context(|| format!("uploading {rel_path}"))?;
    Ok(())
}

pub async fn delete_key(store: &dyn ObjectStore, prefix: &str, rel_path: &str) -> Result<()> {
    let key = ObjPath::from(format!("{}/{rel_path}", prefix.trim_matches('/')));
    store
        .delete(&key)
        .await
        .with_context(|| format!("deleting {rel_path}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;
    use object_store::PutPayload;

    /// `InMemory` is `object_store`'s own real implementation of the
    /// `ObjectStore` trait — used here (not mocked) so these tests
    /// exercise the actual `put`/`list`/`delete` calls this crate makes
    /// against a real S3-compatible backend, just swapping the transport.
    /// Only the network configuration (`AmazonS3Builder`) itself goes
    /// untested in this sandbox — see README.
    #[tokio::test]
    async fn list_remote_strips_the_prefix_and_reports_correct_sizes() {
        let store = InMemory::new();
        store
            .put(
                &ObjPath::from("backups/a.txt"),
                PutPayload::from(b"hello".to_vec()),
            )
            .await
            .unwrap();
        store
            .put(
                &ObjPath::from("backups/sub/b.txt"),
                PutPayload::from(b"world!".to_vec()),
            )
            .await
            .unwrap();
        store
            .put(
                &ObjPath::from("other-prefix/c.txt"),
                PutPayload::from(b"ignored".to_vec()),
            )
            .await
            .unwrap();

        let listed = list_remote(&store, "backups").await.unwrap();
        assert_eq!(
            listed.len(),
            2,
            "must not include keys outside the requested prefix"
        );
        assert_eq!(listed["a.txt"].size, 5);
        assert_eq!(listed["sub/b.txt"].size, 6);
    }

    #[tokio::test]
    async fn upload_then_list_round_trips() {
        let store = InMemory::new();
        let dir = std::env::temp_dir().join(format!("s3sync-remote-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file.txt"), b"payload contents").unwrap();

        upload_file(&store, "myprefix", &dir, "file.txt")
            .await
            .unwrap();
        let listed = list_remote(&store, "myprefix").await.unwrap();
        assert_eq!(listed["file.txt"].size, "payload contents".len() as u64);
        assert!(
            listed["file.txt"].e_tag.is_some(),
            "list_remote must carry ObjectMeta::e_tag through into RemoteMeta \
             (--checksum mode in plan::plan_sync_checksum relies on this; \
             InMemory's own e_tag isn't a real content MD5, but the wiring \
             that plumbs whatever the backend reports is what's under test here)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn delete_removes_the_key() {
        let store = InMemory::new();
        store
            .put(
                &ObjPath::from("p/x.txt"),
                PutPayload::from(b"data".to_vec()),
            )
            .await
            .unwrap();
        assert_eq!(list_remote(&store, "p").await.unwrap().len(), 1);

        delete_key(&store, "p", "x.txt").await.unwrap();
        assert_eq!(list_remote(&store, "p").await.unwrap().len(), 0);
    }
}
