use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use futures::stream::{self, StreamExt};
use s3sync::{localwalk, plan, remote, store};

/// Uploads run this many at a time by default when `--concurrency`/`-j`
/// isn't given — enough to get real overlap on the network round trip
/// per file without opening an unbounded number of connections against
/// a small self-hosted endpoint.
const DEFAULT_CONCURRENCY: usize = 8;

#[derive(Parser)]
#[command(name = "s3sync", about = "rclone, scoped to S3-compatible backends")]
struct Cli {
    #[arg(long, env = "S3SYNC_BUCKET")]
    bucket: String,
    #[arg(long, env = "AWS_ENDPOINT")]
    endpoint: Option<String>,
    #[arg(long, env = "AWS_REGION")]
    region: Option<String>,
    #[arg(long, env = "AWS_ACCESS_KEY_ID")]
    access_key_id: Option<String>,
    #[arg(long, env = "AWS_SECRET_ACCESS_KEY")]
    secret_access_key: Option<String>,
    /// Needed for non-TLS local endpoints like a dev MinIO/RustFS instance.
    #[arg(long)]
    allow_http: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// One-way sync: uploads new/changed files from `local_dir` to
    /// `remote_prefix`. Never deletes remote files unless `--delete` is given.
    Sync {
        local_dir: PathBuf,
        remote_prefix: String,
        #[arg(long)]
        delete: bool,
        /// Show what would happen without uploading or deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Upload this many files at once instead of one at a time.
        #[arg(short = 'j', long, default_value_t = DEFAULT_CONCURRENCY)]
        concurrency: usize,
        /// Decide what needs uploading by comparing a content hash (MD5)
        /// against the remote object's ETag instead of size+mtime. Slower
        /// (every local file that also exists remotely gets read and
        /// hashed up front) but catches same-size-and-mtime edits that the
        /// default heuristic would miss. Falls back to size+mtime for any
        /// object whose ETag isn't a plain (non-multipart) MD5.
        #[arg(long)]
        checksum: bool,
    },
    /// List everything under a remote prefix.
    Ls { remote_prefix: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = store::S3Config {
        bucket: cli.bucket,
        endpoint: cli.endpoint,
        region: cli.region,
        access_key_id: cli.access_key_id,
        secret_access_key: cli.secret_access_key,
        allow_http: cli.allow_http,
    };
    let object_store = store::build_store(&cfg)?;

    match cli.command {
        Command::Ls { remote_prefix } => {
            let listed = remote::list_remote(object_store.as_ref(), &remote_prefix).await?;
            for (key, meta) in &listed {
                println!("{:>12}  {key}", meta.size);
            }
            println!("{} object(s)", listed.len());
        }
        Command::Sync {
            local_dir,
            remote_prefix,
            delete,
            dry_run,
            concurrency,
            checksum,
        } => {
            let local = localwalk::walk_local(&local_dir)?;
            let remote_state = remote::list_remote(object_store.as_ref(), &remote_prefix).await?;

            let sync_plan = if checksum {
                // Only hash local files that also exist remotely: a file
                // missing remotely uploads unconditionally, so hashing it
                // first would be wasted I/O on top of the cost --checksum
                // already accepts (reading every *comparison candidate*
                // up front, not literally every local file).
                let candidates = local.keys().filter(|path| remote_state.contains_key(*path));
                let local_hashes = localwalk::hash_files(&local_dir, candidates)?;
                plan::plan_sync_checksum(&local, &local_hashes, &remote_state, delete)
            } else {
                plan::plan_sync(&local, &remote_state, delete)
            };

            println!(
                "{} to upload, {} unchanged, {} to delete{}",
                sync_plan.upload.len(),
                sync_plan.skip.len(),
                sync_plan.delete.len(),
                if dry_run {
                    " (dry run — nothing will actually happen)"
                } else {
                    ""
                }
            );
            for path in &sync_plan.upload {
                println!("  upload  {path}");
            }
            for path in &sync_plan.delete {
                println!("  delete  {path}");
            }

            if dry_run {
                return Ok(());
            }

            let concurrency = concurrency.max(1);
            let mut failures: Vec<String> = Vec::new();

            // Uploads run up to `concurrency` at a time via
            // `buffer_unordered`. Each task carries its own path back out
            // alongside its `Result` so one failure can't swallow or block
            // the rest: every upload is attempted regardless of earlier
            // failures, and every failure is collected here before this
            // command returns a single aggregated error.
            let upload_results: Vec<(String, anyhow::Result<()>)> =
                stream::iter(sync_plan.upload.clone())
                    .map(|path| {
                        let object_store = Arc::clone(&object_store);
                        let remote_prefix = remote_prefix.clone();
                        let local_dir = local_dir.clone();
                        async move {
                            let result = remote::upload_file(
                                object_store.as_ref(),
                                &remote_prefix,
                                &local_dir,
                                &path,
                            )
                            .await;
                            (path, result)
                        }
                    })
                    .buffer_unordered(concurrency)
                    .collect()
                    .await;

            for (path, result) in upload_results {
                match result {
                    Ok(()) => println!("  uploaded  {path}"),
                    Err(err) => {
                        eprintln!("  FAILED upload  {path}: {err:#}");
                        failures.push(format!("upload {path}: {err:#}"));
                    }
                }
            }

            // Deletes are typically far fewer than uploads and are kept
            // sequential, but the same "attempt everything, then report
            // every failure" contract applies here too rather than
            // aborting the whole sync on the first error.
            for path in &sync_plan.delete {
                match remote::delete_key(object_store.as_ref(), &remote_prefix, path).await {
                    Ok(()) => println!("  deleted  {path}"),
                    Err(err) => {
                        eprintln!("  FAILED delete  {path}: {err:#}");
                        failures.push(format!("delete {path}: {err:#}"));
                    }
                }
            }

            if !failures.is_empty() {
                anyhow::bail!(
                    "{} of {} operation(s) failed:\n  {}",
                    failures.len(),
                    sync_plan.upload.len() + sync_plan.delete.len(),
                    failures.join("\n  ")
                );
            }
        }
    }
    Ok(())
}
