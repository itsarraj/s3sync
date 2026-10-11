# s3sync

`rclone`, scoped to S3-compatible backends. Full `rclone` (Go) supports
40+ cloud providers — too much surface for a first tool. This covers
exactly one, well: anything speaking the S3 API (AWS S3 itself, MinIO,
self-hosted RustFS, Cloudflare R2, Backblaze B2, ...) via the `object_store`
crate — the same crate `mini-me`, `handshake-system`, `job-search-app`, and
`SeekersConnect` already use for their own RustFS-backed photo/resume
storage, so this is a natural fit for replacing a Go `rclone` binary in
those apps' own deploy/backup scripts.

## Installation

```sh
cargo install --path .
```

## Usage

```bash
export AWS_ACCESS_KEY_ID=...
export AWS_SECRET_ACCESS_KEY=...
export AWS_ENDPOINT=http://your-rustfs-host:9000   # omit for real AWS S3
export AWS_REGION=us-east-1

s3sync --bucket my-backups --allow-http ls photos/
s3sync --bucket my-backups --allow-http sync ./local-photos photos/ --dry-run
s3sync --bucket my-backups --allow-http sync ./local-photos photos/ --delete
s3sync --bucket my-backups --allow-http sync ./local-photos photos/ -j 16      # 16 uploads at once
s3sync --bucket my-backups --allow-http sync ./local-photos photos/ --checksum # content-hash comparison
```

`--allow-http` is only needed for a non-TLS local/self-hosted endpoint;
real AWS S3 is always HTTPS. Every flag also has an env var
(`--endpoint`/`AWS_ENDPOINT`, etc.) via `clap`'s `env` feature — same
convention as every other AWS-adjacent tool, zero flags needed if your
shell already has them exported.

`sync` is one-way, local → remote, and **never deletes anything remote
unless `--delete` is explicitly passed** — matching this workspace's own
"no destructive default" convention.

## How it decides what to upload

By default, the same heuristic `rsync`'s default mode uses: a file uploads
if it's new, its size differs from the remote copy, or the local file is
newer than the remote one. Fast, no extra reads, and what every mainstream
sync tool ships as the default — but it can't catch an edit that keeps the
exact same size and doesn't bump mtime.

Pass `--checksum` for content-hash comparison instead: every local file
that also exists remotely gets read and MD5-hashed up front, then compared
against the remote object's `ETag`. This is strictly more accurate but
costs the I/O of reading those files — the same tradeoff every mainstream
sync tool makes for its own opt-in hash mode. It only applies to plain
(non-multipart) uploads: a multipart `ETag` is `<hex>-<part-count>`, the
MD5 of the concatenated part MD5s rather than the object's own content
hash, so `--checksum` transparently falls back to size-or-mtime for any
object shaped like that (`plan::is_comparable_md5_etag` is the guard).

## Concurrency

`sync` uploads `-j`/`--concurrency` files at once (default 8, via
`futures::stream::buffer_unordered`) instead of one at a time. Every
upload is attempted regardless of earlier failures — one failed file
doesn't block or skip the rest — and every failure is collected into a
single aggregated error at the end rather than aborting on the first one.
Deletes stay sequential (typically far fewer of them, and less benefit
from overlap).

## Status: built and verified end-to-end against a real S3-compatible endpoint, not just an in-memory store

- **21 unit/integration tests** (`cargo test --lib`):
  - `plan`: missing-remote-file uploads; identical size + older local mtime
    is skipped; **differing size uploads even when the remote copy is
    newer** (size wins); **same size but newer local mtime still uploads**
    (a locally-edited file with coincidentally the same byte count must
    not be silently skipped); remote-only files are deleted only when
    `--delete` is explicitly passed, never by default.
  - `localwalk`: real recursive directory walk (real temp directories, not
    fixtures) producing forward-slash-normalized relative paths regardless
    of nesting depth; an empty directory produces an empty map rather than
    erroring.
  - `remote`: **run against `object_store::memory::InMemory` — the
    `object_store` crate's own real implementation of the `ObjectStore`
    trait**, not a hand-rolled mock. This means `list_remote`,
    `upload_file`, and `delete_key` are exercised through the exact same
    trait calls (`put`/`list`/`delete`) that hit real S3/MinIO/RustFS in
    production, with only the network transport swapped out: prefix
    stripping confirmed correct (and confirmed *not* to leak keys from a
    sibling prefix), a real upload-then-list round trip against actual
    file contents on disk, and delete actually removing the key from a
    subsequent listing.
  - `store`: `AmazonS3Builder::build()` succeeds against explicit
    config — this only validates configuration locally (no request is
    issued until first use), so it's a legitimate offline test of the
    config-wiring path.
  - `plan::plan_sync_checksum`/`is_comparable_md5_etag`: a file with a
    stale mtime and a deliberately wrong recorded size is still correctly
    **skipped** when its real content hash matches the remote `ETag`
    (proving checksum mode overrides the default heuristic rather than
    just adding to it); a multipart-shaped `ETag` (`<hex>-<part-count>`)
    is rejected as non-comparable and falls back to size-or-mtime instead
    of comparing against a hash that isn't actually the object's content.
- **CLI wiring verified against a real (deliberately unreachable) address**:
  ran the actual compiled binary with `--endpoint http://127.0.0.1:1`
  (nothing listens there) and confirmed it fails with a clear, fully
  contextualized error chain (`listing remote objects` → the underlying
  `object_store`/`reqwest`/OS-level `Connection refused`) — not a panic,
  not a hang, an honest one-line-per-cause error report.
- **End-to-end live against a real S3-compatible endpoint** (a self-hosted
  RustFS instance on `localhost:9000`, the same backend this workspace's
  own `mini-me` app runs against): built the release binary and ran it for
  real — first sync uploaded 2 new files and a follow-up `aws s3 ls`
  against the real bucket confirmed both objects present; an immediate
  re-sync with no changes reported **0 to upload, 2 unchanged** (the
  size/mtime skip path exercised against a live remote, not a mock);
  editing one file's content and re-running with `--checksum` correctly
  re-uploaded exactly the changed file and left the other alone; `-j 4`
  concurrency was exercised throughout with no dropped or duplicated
  uploads. The probe bucket and its objects were deleted afterward,
  leaving the shared instance clean.

**Not done / deliberately deferred**: any provider beyond S3-compatible
ones (Google Cloud Storage, Azure Blob — `object_store` supports both,
wiring in a `--provider` flag is a natural v2, not attempted here to keep
v1's config surface to what's actually been tested).
