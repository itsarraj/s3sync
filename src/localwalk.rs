use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::plan::LocalMeta;

/// Computes the lowercase-hex MD5 digest of a single local file's full
/// contents. Used only in `--checksum` mode: for a plain (non-multipart)
/// `PUT`, this is exactly the value S3-compatible stores return as the
/// object's `ETag` (see `plan::is_comparable_md5_etag`), so comparing
/// against it needs no round trip to fetch the remote object's bytes —
/// just the `ETag` `remote::list_remote` already returned.
///
/// This reads the entire file into memory, which is the real cost
/// `--checksum` mode trades away versus the size-or-mtime default; see
/// the README.
pub fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("hashing {}", path.display()))?;
    Ok(format!("{:x}", md5::compute(bytes)))
}

/// Computes [`hash_file`] for every path in `paths`, resolving each one
/// against `root`. Callers should pass only the paths that actually need
/// a hash (e.g. files present both locally and remotely) — hashing a
/// brand-new file that's uploading unconditionally regardless is wasted
/// I/O.
pub fn hash_files<'a>(
    root: &Path,
    paths: impl IntoIterator<Item = &'a String>,
) -> Result<BTreeMap<String, String>> {
    let mut hashes = BTreeMap::new();
    for path in paths {
        let hash = hash_file(&root.join(path))?;
        hashes.insert(path.clone(), hash);
    }
    Ok(hashes)
}

/// Recursively walks `root`, returning every regular file keyed by its
/// path relative to `root` with forward slashes (so it matches S3 key
/// conventions regardless of the host OS's path separator).
pub fn walk_local(root: &Path) -> Result<BTreeMap<String, LocalMeta>> {
    let mut result = BTreeMap::new();
    walk_into(root, root, &mut result)?;
    Ok(result)
}

fn walk_into(root: &Path, dir: &Path, out: &mut BTreeMap<String, LocalMeta>) -> Result<()> {
    let entries =
        fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk_into(root, &path, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(root)
                .expect("walked path is always under root")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let metadata = entry.metadata()?;
            out.insert(
                rel,
                LocalMeta {
                    size: metadata.len(),
                    modified: metadata.modified()?,
                },
            );
        }
        // Symlinks are neither `is_dir()` nor `is_file()` under
        // `DirEntry::file_type` (which doesn't follow them) — skipped
        // rather than followed, so a symlink loop can't hang the walk.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("s3sync-test-{}-{}", std::process::id(), name))
    }

    #[test]
    fn walks_nested_directories_with_forward_slash_relative_paths() {
        let root = unique_temp_dir("nested");
        fs::create_dir_all(root.join("sub/dir")).unwrap();
        fs::write(root.join("top.txt"), b"top").unwrap();
        fs::write(root.join("sub/dir/deep.txt"), b"deep content").unwrap();

        let walked = walk_local(&root).unwrap();
        assert_eq!(walked.len(), 2);
        assert_eq!(walked["top.txt"].size, 3);
        assert_eq!(walked["sub/dir/deep.txt"].size, 12);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn empty_directory_produces_empty_map() {
        let root = unique_temp_dir("empty");
        fs::create_dir_all(&root).unwrap();
        let walked = walk_local(&root).unwrap();
        assert!(walked.is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hash_file_matches_a_known_md5_digest() {
        let root = unique_temp_dir("hash-known");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("empty.txt"), b"").unwrap();
        fs::write(root.join("abc.txt"), b"abc").unwrap();

        // Well-known MD5 test vectors (RFC 1321) — pins hash_file to the
        // exact algorithm S3's plain-PUT ETag uses, not just "some hash".
        assert_eq!(
            hash_file(&root.join("empty.txt")).unwrap(),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
        assert_eq!(
            hash_file(&root.join("abc.txt")).unwrap(),
            "900150983cd24fb0d6963f7d28e17f72"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hash_file_differs_for_different_content_and_is_stable_for_same_content() {
        let root = unique_temp_dir("hash-stable");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"same content").unwrap();
        fs::write(root.join("b.txt"), b"same content").unwrap();
        fs::write(root.join("c.txt"), b"different content").unwrap();

        let hash_a = hash_file(&root.join("a.txt")).unwrap();
        let hash_b = hash_file(&root.join("b.txt")).unwrap();
        let hash_c = hash_file(&root.join("c.txt")).unwrap();

        assert_eq!(hash_a, hash_b, "identical bytes must hash identically");
        assert_ne!(hash_a, hash_c, "different bytes must not collide here");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hash_files_only_hashes_the_requested_subset() {
        let root = unique_temp_dir("hash-subset");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("keep.txt"), b"keep me").unwrap();
        fs::write(root.join("skip.txt"), b"never hashed").unwrap();

        let wanted = vec!["keep.txt".to_string()];
        let hashes = hash_files(&root, &wanted).unwrap();

        assert_eq!(hashes.len(), 1);
        assert!(hashes.contains_key("keep.txt"));
        assert!(
            !hashes.contains_key("skip.txt"),
            "hash_files must not touch paths the caller didn't ask for"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hash_file_reports_a_contextualized_error_for_a_missing_file() {
        let root = unique_temp_dir("hash-missing");
        fs::create_dir_all(&root).unwrap();

        let err = hash_file(&root.join("does-not-exist.txt")).unwrap_err();
        assert!(
            format!("{err:#}").contains("does-not-exist.txt"),
            "error should name the file that failed to hash: {err:#}"
        );

        fs::remove_dir_all(&root).ok();
    }
}
