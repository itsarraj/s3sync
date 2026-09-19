use std::collections::BTreeMap;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq)]
pub struct LocalMeta {
    pub size: u64,
    pub modified: SystemTime,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteMeta {
    pub size: u64,
    pub last_modified: SystemTime,
    /// The object's `ETag` as reported by the store (quotes included, as
    /// S3-compatible APIs send it over the wire). For a plain (non-multipart)
    /// `PUT` this is the hex MD5 of the object's bytes, which is exactly
    /// what `--checksum` mode compares against — see
    /// [`is_comparable_md5_etag`]. `None` for backends/tests that don't
    /// populate it.
    pub e_tag: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyncPlan {
    pub upload: Vec<String>,
    pub delete: Vec<String>,
    pub skip: Vec<String>,
}

/// Decides what a one-way local→remote sync needs to do. A file uploads if
/// it's missing remotely, its size differs, or the local copy is strictly
/// newer than the remote one — the same "size-or-mtime" heuristic
/// `rsync`'s default mode uses.
///
/// Deliberately not content-hash comparison: computing a real hash means
/// reading every local file's full contents up front, and S3's `ETag`
/// isn't a plain MD5 once multipart upload is involved, so comparing
/// against it isn't as simple as it looks. size-or-mtime is the practical
/// default every mainstream sync tool ships with the hash-based comparison
/// as an opt-in extra, not the default — same tradeoff made here.
pub fn plan_sync(
    local: &BTreeMap<String, LocalMeta>,
    remote: &BTreeMap<String, RemoteMeta>,
    delete_extraneous: bool,
) -> SyncPlan {
    let mut plan = SyncPlan::default();

    for (path, local_meta) in local {
        match remote.get(path) {
            None => plan.upload.push(path.clone()),
            Some(remote_meta) => {
                if needs_upload_size_or_mtime(local_meta, remote_meta) {
                    plan.upload.push(path.clone());
                } else {
                    plan.skip.push(path.clone());
                }
            }
        }
    }

    plan.delete = plan_deletes(local, remote, delete_extraneous);
    plan
}

fn needs_upload_size_or_mtime(local_meta: &LocalMeta, remote_meta: &RemoteMeta) -> bool {
    local_meta.size != remote_meta.size || local_meta.modified > remote_meta.last_modified
}

fn plan_deletes(
    local: &BTreeMap<String, LocalMeta>,
    remote: &BTreeMap<String, RemoteMeta>,
    delete_extraneous: bool,
) -> Vec<String> {
    if !delete_extraneous {
        return Vec::new();
    }
    remote
        .keys()
        .filter(|path| !local.contains_key(*path))
        .cloned()
        .collect()
}

/// True if `etag` (quotes included or not — both are handled) is a plain,
/// non-multipart S3 `ETag`: exactly a 32-character hex MD5 digest.
/// Multipart uploads produce an `ETag` shaped like `<hex>-<part-count>`,
/// which is *not* the object's content MD5 (it's the MD5 of the
/// concatenated part MD5s) — that form can't be compared against a
/// locally computed whole-file hash, so callers must fall back to the
/// size-or-mtime heuristic for those objects instead.
pub fn is_comparable_md5_etag(etag: &str) -> bool {
    let etag = etag.trim_matches('"');
    etag.len() == 32 && etag.chars().all(|c| c.is_ascii_hexdigit())
}

/// Same decision `plan_sync` makes, but opt-in content-hash comparison
/// instead of size-or-mtime: a file whose remote copy has a comparable
/// (non-multipart) MD5 `ETag` is only re-uploaded if its locally computed
/// hash actually differs from that `ETag`. This is a strictly more
/// accurate (if more expensive — every local file that also exists
/// remotely must be read and hashed up front by the caller) test than
/// size-or-mtime: an edited-then-reverted file, or one that was touched
/// without its content changing, is correctly skipped instead of
/// re-uploaded.
///
/// `local_hashes` must map a subset (or all) of `local`'s keys to their
/// lowercase-hex MD5 digest; entries plan_sync_checksum doesn't need
/// (because the file is missing remotely, or the remote `ETag` isn't a
/// comparable plain MD5) don't need to be present. When no usable hash
/// or `ETag` is available for a path, this falls back to the same
/// size-or-mtime heuristic `plan_sync` uses, rather than guessing.
pub fn plan_sync_checksum(
    local: &BTreeMap<String, LocalMeta>,
    local_hashes: &BTreeMap<String, String>,
    remote: &BTreeMap<String, RemoteMeta>,
    delete_extraneous: bool,
) -> SyncPlan {
    let mut plan = SyncPlan::default();

    for (path, local_meta) in local {
        match remote.get(path) {
            None => plan.upload.push(path.clone()),
            Some(remote_meta) => {
                let needs_upload = match (&remote_meta.e_tag, local_hashes.get(path)) {
                    (Some(etag), Some(local_hash)) if is_comparable_md5_etag(etag) => {
                        !etag.trim_matches('"').eq_ignore_ascii_case(local_hash)
                    }
                    _ => needs_upload_size_or_mtime(local_meta, remote_meta),
                };
                if needs_upload {
                    plan.upload.push(path.clone());
                } else {
                    plan.skip.push(path.clone());
                }
            }
        }
    }

    plan.delete = plan_deletes(local, remote, delete_extraneous);
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t(secs_from_epoch: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs_from_epoch)
    }

    /// Builds a `RemoteMeta` with no `ETag` — the shape every pre-existing
    /// (pre-`--checksum`) test in this file uses.
    fn rm(size: u64, secs_from_epoch: u64) -> RemoteMeta {
        RemoteMeta {
            size,
            last_modified: t(secs_from_epoch),
            e_tag: None,
        }
    }

    #[test]
    fn missing_remote_file_gets_uploaded() {
        let mut local = BTreeMap::new();
        local.insert(
            "a.txt".to_string(),
            LocalMeta {
                size: 10,
                modified: t(100),
            },
        );
        let remote = BTreeMap::new();

        let plan = plan_sync(&local, &remote, false);
        assert_eq!(plan.upload, vec!["a.txt"]);
        assert!(plan.delete.is_empty());
    }

    #[test]
    fn identical_size_and_older_local_mtime_is_skipped() {
        let mut local = BTreeMap::new();
        local.insert(
            "a.txt".to_string(),
            LocalMeta {
                size: 10,
                modified: t(100),
            },
        );
        let mut remote = BTreeMap::new();
        remote.insert("a.txt".to_string(), rm(10, 200));

        let plan = plan_sync(&local, &remote, false);
        assert_eq!(plan.skip, vec!["a.txt"]);
        assert!(plan.upload.is_empty());
    }

    #[test]
    fn differing_size_triggers_upload_even_if_remote_is_newer() {
        let mut local = BTreeMap::new();
        local.insert(
            "a.txt".to_string(),
            LocalMeta {
                size: 999,
                modified: t(100),
            },
        );
        let mut remote = BTreeMap::new();
        remote.insert("a.txt".to_string(), rm(10, 500));

        let plan = plan_sync(&local, &remote, false);
        assert_eq!(plan.upload, vec!["a.txt"]);
    }

    #[test]
    fn newer_local_mtime_with_same_size_still_uploads() {
        let mut local = BTreeMap::new();
        local.insert(
            "a.txt".to_string(),
            LocalMeta {
                size: 10,
                modified: t(999),
            },
        );
        let mut remote = BTreeMap::new();
        remote.insert("a.txt".to_string(), rm(10, 100));

        let plan = plan_sync(&local, &remote, false);
        assert_eq!(
            plan.upload,
            vec!["a.txt"],
            "a locally-edited file (newer mtime) must re-upload even if size happens to match"
        );
    }

    #[test]
    fn remote_only_file_deleted_only_when_flag_is_set() {
        let local = BTreeMap::new();
        let mut remote = BTreeMap::new();
        remote.insert("gone.txt".to_string(), rm(5, 1));

        let without_delete = plan_sync(&local, &remote, false);
        assert!(
            without_delete.delete.is_empty(),
            "must never delete unless explicitly asked"
        );

        let with_delete = plan_sync(&local, &remote, true);
        assert_eq!(with_delete.delete, vec!["gone.txt"]);
    }

    // --- `--checksum` mode (`plan_sync_checksum`) -------------------------

    const SAME_HASH: &str = "d41d8cd98f00b204e9800998ecf8427e";
    const OTHER_HASH: &str = "0cc175b9c0f1b6a831c399e269772661";

    fn rm_etag(size: u64, secs_from_epoch: u64, etag: &str) -> RemoteMeta {
        RemoteMeta {
            size,
            last_modified: t(secs_from_epoch),
            e_tag: Some(etag.to_string()),
        }
    }

    #[test]
    fn is_comparable_md5_etag_accepts_plain_hex_and_rejects_multipart_and_quoted_forms() {
        assert!(is_comparable_md5_etag(SAME_HASH));
        assert!(
            is_comparable_md5_etag(&format!("\"{SAME_HASH}\"")),
            "surrounding quotes (as sent over the wire by S3-compatible APIs) must be tolerated"
        );
        assert!(
            !is_comparable_md5_etag("d41d8cd98f00b204e9800998ecf8427e-3"),
            "a multipart ETag's `-<part-count>` suffix means it is not a plain content MD5"
        );
        assert!(!is_comparable_md5_etag("not-hex-at-all-but-32-characters"));
        assert!(!is_comparable_md5_etag(""));
    }

    #[test]
    fn checksum_mode_skips_a_file_whose_hash_matches_the_remote_etag_even_with_a_stale_mtime() {
        let mut local = BTreeMap::new();
        local.insert(
            "a.txt".to_string(),
            LocalMeta {
                size: 999, // deliberately wrong/misleading size too
                modified: t(9_999),
            },
        );
        let mut remote = BTreeMap::new();
        remote.insert("a.txt".to_string(), rm_etag(10, 1, SAME_HASH));
        let mut hashes = BTreeMap::new();
        hashes.insert("a.txt".to_string(), SAME_HASH.to_string());

        let plan = plan_sync_checksum(&local, &hashes, &remote, false);
        assert_eq!(
            plan.skip,
            vec!["a.txt"],
            "identical content hash must win over mismatched size/mtime once --checksum is on"
        );
        assert!(plan.upload.is_empty());
    }

    #[test]
    fn checksum_mode_uploads_a_file_whose_hash_differs_from_the_remote_etag() {
        let mut local = BTreeMap::new();
        local.insert(
            "a.txt".to_string(),
            LocalMeta {
                size: 10, // same size, same-or-older mtime: size/mtime alone would skip this
                modified: t(1),
            },
        );
        let mut remote = BTreeMap::new();
        remote.insert("a.txt".to_string(), rm_etag(10, 100, SAME_HASH));
        let mut hashes = BTreeMap::new();
        hashes.insert("a.txt".to_string(), OTHER_HASH.to_string());

        let plan = plan_sync_checksum(&local, &hashes, &remote, false);
        assert_eq!(
            plan.upload,
            vec!["a.txt"],
            "changed content must re-upload even when size and mtime look unchanged"
        );
    }

    #[test]
    fn checksum_mode_falls_back_to_size_or_mtime_for_a_multipart_etag() {
        let mut local = BTreeMap::new();
        local.insert(
            "a.txt".to_string(),
            LocalMeta {
                size: 10,
                modified: t(1),
            },
        );
        let mut remote = BTreeMap::new();
        // Multipart ETag: not a plain content MD5, so even though the local
        // hash is provided it cannot be compared against this ETag.
        remote.insert(
            "a.txt".to_string(),
            rm_etag(10, 100, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-2"),
        );
        let mut hashes = BTreeMap::new();
        hashes.insert("a.txt".to_string(), OTHER_HASH.to_string());

        let plan = plan_sync_checksum(&local, &hashes, &remote, false);
        assert_eq!(
            plan.skip,
            vec!["a.txt"],
            "same size + not-newer local mtime must skip when the ETag can't be compared, \
             regardless of what the (incomparable) hash says"
        );
    }

    #[test]
    fn checksum_mode_falls_back_to_size_or_mtime_when_no_local_hash_was_computed() {
        let mut local = BTreeMap::new();
        local.insert(
            "a.txt".to_string(),
            LocalMeta {
                size: 999,
                modified: t(1),
            },
        );
        let mut remote = BTreeMap::new();
        remote.insert("a.txt".to_string(), rm_etag(10, 100, SAME_HASH));
        let hashes = BTreeMap::new(); // caller didn't hash this file

        let plan = plan_sync_checksum(&local, &hashes, &remote, false);
        assert_eq!(
            plan.upload,
            vec!["a.txt"],
            "differing size must still trigger upload when no local hash is available"
        );
    }

    #[test]
    fn checksum_mode_still_uploads_missing_remote_files_and_respects_delete_flag() {
        let mut local = BTreeMap::new();
        local.insert(
            "new.txt".to_string(),
            LocalMeta {
                size: 3,
                modified: t(1),
            },
        );
        let mut remote = BTreeMap::new();
        remote.insert("gone.txt".to_string(), rm_etag(5, 1, SAME_HASH));
        let hashes = BTreeMap::new();

        let without_delete = plan_sync_checksum(&local, &hashes, &remote, false);
        assert_eq!(without_delete.upload, vec!["new.txt"]);
        assert!(without_delete.delete.is_empty());

        let with_delete = plan_sync_checksum(&local, &hashes, &remote, true);
        assert_eq!(with_delete.delete, vec!["gone.txt"]);
    }
}
