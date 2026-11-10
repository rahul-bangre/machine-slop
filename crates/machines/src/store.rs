use std::{
    collections::HashMap,
    io,
    os::unix::fs::{FileExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use object_store::{
    ObjectStore, buffered::BufWriter, local::LocalFileSystem, path::Path as ObjectPath,
    prefix::PrefixStore,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncSeekExt, AsyncWriteExt},
    sync::{OnceCell, mpsc},
};

use crate::{contract::SnapshotInfo, files, tenants::unix_ms};

const BLOCK: u64 = 4096;
const READ_CHUNK: u64 = 1 << 20;
const RECORD: &str = "record.json";
const SNAPSHOTS: &str = "snapshots";
const IMAGES: &str = "images";
const CONCURRENT_RECORD_READS: usize = 16;
pub(crate) const UPLOAD_LIMIT: Duration = Duration::from_secs(60 * 60);
const ABANDONED_UPLOAD_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub struct Store {
    objects: Arc<dyn ObjectStore>,
    directory: Option<PathBuf>,
    url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoredAs {
    PerSnapshot,
    ContentAddressed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Blob {
    len: u64,
    extents: Vec<[u64; 2]>,
    digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredFile {
    name: String,
    stored_as: StoredAs,
    executable: bool,
    blob: Blob,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredSnapshot {
    pub info: SnapshotInfo,
    pub tenant: String,
    pub published_unix_ms: u64,
    files: Vec<StoredFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum StoredRecord {
    Published(StoredSnapshot),
    Abandoned { at_unix_ms: u64 },
}

pub enum Settled {
    Published(StoredSnapshot),
    Abandoned,
}

impl StoredSnapshot {
    pub(crate) async fn link_images_into(&self, dir: &Path, images: &Path) -> io::Result<()> {
        for file in &self.files {
            if file.stored_as != StoredAs::ContentAddressed {
                continue;
            }
            let cached = images.join(&file.blob.digest);
            match tokio::fs::hard_link(dir.join(&file.name), &cached).await {
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
                _ => {}
            }
        }
        files::sync(images).await
    }
}

impl Store {
    pub fn open(url: &str) -> io::Result<Self> {
        let parsed = url::Url::parse(url)
            .map_err(|error| invalid(format!("store URL {url:?} is invalid: {error}")))?;
        let (objects, directory): (Arc<dyn ObjectStore>, _) = match parsed.scheme() {
            "file" => {
                let directory = parsed
                    .to_file_path()
                    .map_err(|()| invalid(format!("store URL {url:?} is not a local path")))?;
                std::fs::create_dir_all(&directory)?;
                let directory = std::fs::canonicalize(directory)?;
                let local =
                    LocalFileSystem::new_with_prefix(&directory).map_err(io::Error::from)?;
                (Arc::new(local), Some(directory))
            }
            "s3" => {
                let bucket = object_store::aws::AmazonS3Builder::from_env()
                    .with_url(url)
                    .build()
                    .map_err(io::Error::from)?;
                let prefix = ObjectPath::parse(parsed.path()).map_err(object_error)?;
                let objects: Arc<dyn ObjectStore> = if prefix.as_ref().is_empty() {
                    Arc::new(bucket)
                } else {
                    Arc::new(PrefixStore::new(bucket, prefix))
                };
                (objects, None)
            }
            scheme => {
                return Err(invalid(format!(
                    "store URL scheme {scheme:?} is not supported; use file:// or s3://"
                )));
            }
        };
        Ok(Self {
            objects,
            directory,
            url: url.to_owned(),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn snapshot(&self, id: &str) -> io::Result<Option<StoredSnapshot>> {
        if !is_object_name(id) {
            return Ok(None);
        }
        Ok(match self.record(id).await? {
            Some(StoredRecord::Published(stored)) => Some(stored),
            Some(StoredRecord::Abandoned { .. }) | None => None,
        })
    }

    async fn record(&self, id: &str) -> io::Result<Option<StoredRecord>> {
        match self.objects.get(&record_path(id)).await {
            Ok(found) => {
                let bytes = found.bytes().await.map_err(io::Error::from)?;
                serde_json::from_slice(&bytes)
                    .map(Some)
                    .map_err(io::Error::other)
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn settle_snapshot(&self, id: &str) -> io::Result<Settled> {
        if !is_object_name(id) {
            return Ok(Settled::Abandoned);
        }
        let abandoned = StoredRecord::Abandoned {
            at_unix_ms: unix_ms(),
        };
        let bytes = serde_json::to_vec(&abandoned).map_err(io::Error::other)?;
        let put = self
            .objects
            .put_opts(
                &record_path(id),
                bytes.into(),
                object_store::PutMode::Create.into(),
            )
            .await;
        match put {
            Ok(_) => {
                self.written(&record_path(id)).await?;
                Ok(Settled::Abandoned)
            }
            Err(object_store::Error::AlreadyExists { .. }) => match self.record(id).await? {
                Some(StoredRecord::Published(stored)) => Ok(Settled::Published(stored)),
                Some(StoredRecord::Abandoned { .. }) => Ok(Settled::Abandoned),
                None => Err(io::Error::other(format!(
                    "snapshot {id}'s record existed and then vanished while it was settled"
                ))),
            },
            Err(error) => Err(error.into()),
        }
    }

    pub async fn snapshots(&self) -> io::Result<Vec<StoredSnapshot>> {
        let listed = self
            .objects
            .list_with_delimiter(Some(&ObjectPath::from(SNAPSHOTS)))
            .await
            .map_err(io::Error::from)?;
        let mut infos: Vec<StoredSnapshot> = futures_util::stream::iter(listed.common_prefixes)
            .map(|prefix| async move {
                let id = prefix.filename().unwrap_or_default().to_owned();
                self.snapshot(&id).await
            })
            .buffer_unordered(CONCURRENT_RECORD_READS)
            .try_filter_map(|info| async move { Ok(info) })
            .try_collect()
            .await?;
        infos.sort_by(|a, b| a.info.id.cmp(&b.info.id));
        Ok(infos)
    }

    pub async fn delete_snapshot(&self, id: &str) -> io::Result<bool> {
        if !is_object_name(id) {
            return Ok(false);
        }
        if !matches!(self.record(id).await?, Some(StoredRecord::Published(_))) {
            return Ok(false);
        }
        match self.objects.delete(&record_path(id)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
            Err(error) => return Err(error.into()),
        }
        self.removed(&record_path(id)).await?;
        if let Err(error) = self.remove_prefix(&snapshot_prefix(id)).await {
            tracing::warn!(snapshot_id = %id, %error, "deleted snapshot left objects for the sweep");
        }
        Ok(true)
    }

    pub(crate) async fn publish(
        &self,
        tenant: &str,
        info: &SnapshotInfo,
        dir: &Path,
        files: &[(&str, StoredAs)],
        digests: &Digests,
    ) -> io::Result<StoredSnapshot> {
        let started = Instant::now();
        let mut stored = Vec::with_capacity(files.len());
        for &(name, stored_as) in files {
            let path = dir.join(name);
            let executable = tokio::fs::metadata(&path).await?.permissions().mode() & 0o111 != 0;
            let blob = match stored_as {
                StoredAs::PerSnapshot => {
                    let object = snapshot_prefix(&info.id).child(name);
                    let blob = self.upload(&object, &path).await?;
                    self.written(&object).await?;
                    blob
                }
                StoredAs::ContentAddressed => {
                    let blob = digests.blob(&path).await?;
                    let object = image_path(&blob.digest);
                    if !self.exists(&object).await? {
                        let uploaded = self.upload(&object, &path).await?;
                        if uploaded != blob {
                            return Err(io::Error::other(format!(
                                "{} changed while it was uploaded",
                                path.display()
                            )));
                        }
                        self.written(&object).await?;
                    }
                    blob
                }
            };
            stored.push(StoredFile {
                name: name.to_owned(),
                stored_as,
                executable,
                blob,
            });
        }
        if started.elapsed() > UPLOAD_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "uploading the snapshot took longer than {} minutes",
                    UPLOAD_LIMIT.as_secs() / 60
                ),
            ));
        }
        let record = StoredSnapshot {
            info: info.clone(),
            tenant: tenant.to_owned(),
            published_unix_ms: unix_ms(),
            files: stored,
        };
        let record_bytes = serde_json::to_vec(&StoredRecord::Published(record.clone()))
            .map_err(io::Error::other)?;
        let put = self
            .objects
            .put_opts(
                &record_path(&info.id),
                record_bytes.into(),
                object_store::PutMode::Create.into(),
            )
            .await;
        let recorded = match put {
            Ok(_) => Ok(()),
            Err(object_store::Error::AlreadyExists { .. }) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "snapshot {} was already settled as made or abandoned",
                    info.id
                ),
            )),
            Err(error) => Err(error.into()),
        };
        let recorded = match recorded {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
                match self.record(&info.id).await {
                    Ok(Some(StoredRecord::Published(stored)))
                        if stored.info.id == info.id && stored.tenant == tenant =>
                    {
                        Ok(())
                    }
                    Ok(_) => Err(error),
                    Err(unknown) => Err(io::Error::other(format!(
                        "the store may or may not hold snapshot {}: recording it failed ({error}) and checking failed ({unknown})",
                        info.id
                    ))),
                }
            }
            recorded => recorded,
        };
        recorded?;
        self.written(&record_path(&info.id)).await?;
        Ok(record)
    }

    pub(crate) async fn fetch(
        &self,
        stored: &StoredSnapshot,
        into: &Path,
        images: &Path,
    ) -> io::Result<()> {
        for file in &stored.files {
            let destination = into.join(&file.name);
            match file.stored_as {
                StoredAs::PerSnapshot => {
                    let object = snapshot_prefix(&stored.info.id).child(file.name.as_str());
                    self.download(&object, &file.blob, &destination).await?;
                }
                StoredAs::ContentAddressed => {
                    let cached = images.join(&file.blob.digest);
                    if !tokio::fs::try_exists(&cached).await? {
                        let partial = images.join(format!(
                            "{}.partial-{}",
                            file.blob.digest,
                            uuid::Uuid::new_v4().simple()
                        ));
                        let downloaded = self
                            .download(&image_path(&file.blob.digest), &file.blob, &partial)
                            .await;
                        let published = match downloaded {
                            Ok(()) => tokio::fs::rename(&partial, &cached).await,
                            Err(error) => Err(error),
                        };
                        if let Err(error) = published {
                            files::remove_if_present(tokio::fs::remove_file(&partial).await)?;
                            return Err(error);
                        }
                        files::sync(images).await?;
                    }
                    files::link_immutable(&cached, &destination).await?;
                }
            }
            if file.executable {
                tokio::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755))
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn sweep_abandoned_uploads(&self) {
        let listed = match self
            .objects
            .list_with_delimiter(Some(&ObjectPath::from(SNAPSHOTS)))
            .await
        {
            Ok(listed) => listed,
            Err(error) => {
                tracing::warn!(%error, "listing snapshot uploads to sweep failed");
                return;
            }
        };
        let abandoned_before_ms = (SystemTime::now() - ABANDONED_UPLOAD_AGE)
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
            });
        for prefix in listed.common_prefixes {
            if let Err(error) = self.sweep_prefix(&prefix, abandoned_before_ms).await {
                tracing::warn!(prefix = %prefix, %error, "sweeping a snapshot upload failed");
            }
        }
        if let Some(directory) = &self.directory
            && let Err(error) = remove_abandoned_staging_files(directory).await
        {
            tracing::warn!(%error, "sweeping abandoned staging files failed");
        }
    }

    async fn sweep_prefix(&self, prefix: &ObjectPath, abandoned_before_ms: i64) -> io::Result<()> {
        let objects: Vec<_> = self
            .objects
            .list(Some(prefix))
            .try_collect()
            .await
            .map_err(io::Error::from)?;
        let old = objects
            .iter()
            .all(|object| object.last_modified.timestamp_millis() < abandoned_before_ms);
        if !old {
            return Ok(());
        }
        let recorded = objects
            .iter()
            .any(|object| object.location.filename() == Some(RECORD));
        if recorded
            && !matches!(
                self.record(prefix.filename().unwrap_or_default()).await?,
                Some(StoredRecord::Abandoned { .. })
            )
        {
            return Ok(());
        }
        self.remove_prefix(prefix).await.map_err(io::Error::from)?;
        tracing::info!(prefix = %prefix, "removed an abandoned snapshot upload");
        Ok(())
    }

    async fn exists(&self, object: &ObjectPath) -> io::Result<bool> {
        match self.objects.head(object).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    async fn remove_prefix(&self, prefix: &ObjectPath) -> Result<(), object_store::Error> {
        let objects: Vec<_> = self.objects.list(Some(prefix)).try_collect().await?;
        for object in objects {
            match self.objects.delete(&object.location).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    async fn written(&self, object: &ObjectPath) -> io::Result<()> {
        let Some(directory) = &self.directory else {
            return Ok(());
        };
        let file = directory.join(object.as_ref());
        files::sync(&file).await?;
        self.sync_parents(&file, directory).await
    }

    async fn removed(&self, object: &ObjectPath) -> io::Result<()> {
        let Some(directory) = &self.directory else {
            return Ok(());
        };
        self.sync_parents(&directory.join(object.as_ref()), directory)
            .await
    }

    async fn sync_parents(&self, file: &Path, directory: &Path) -> io::Result<()> {
        let mut parent = file.parent();
        while let Some(dir) = parent {
            files::sync(dir).await?;
            if dir == directory {
                break;
            }
            parent = dir.parent();
        }
        Ok(())
    }

    async fn upload(&self, object: &ObjectPath, path: &Path) -> io::Result<Blob> {
        let mut writer = BufWriter::new(self.objects.clone(), object.clone());
        let encoded = encode(path, Some(&mut writer)).await;
        match encoded {
            Ok(blob) => {
                writer.shutdown().await?;
                Ok(blob)
            }
            Err(error) => {
                if let Err(abort) = writer.abort().await {
                    tracing::warn!(object = %object, error = %abort, "aborting an upload failed");
                }
                Err(error)
            }
        }
    }

    async fn download(&self, object: &ObjectPath, blob: &Blob, into: &Path) -> io::Result<()> {
        let mut stream = self
            .objects
            .get(object)
            .await
            .map_err(io::Error::from)?
            .into_stream();
        let mut file = tokio::fs::File::create(into).await?;
        file.set_len(blob.len).await?;
        let mut data = blake3::Hasher::new();
        let mut extents = blob.extents.iter().map(|&[offset, len]| (offset, len));
        let mut current = extents.next();
        while let Some(chunk) = stream.try_next().await.map_err(io::Error::from)? {
            data.update(&chunk);
            let mut chunk = &chunk[..];
            while !chunk.is_empty() {
                let Some((offset, remaining)) = current.as_mut() else {
                    return Err(corrupt(object, "is longer than its extents"));
                };
                let take = chunk
                    .len()
                    .min(usize::try_from(*remaining).unwrap_or(usize::MAX));
                file.seek(io::SeekFrom::Start(*offset)).await?;
                file.write_all(&chunk[..take]).await?;
                *offset += take as u64;
                *remaining -= take as u64;
                chunk = &chunk[take..];
                if *remaining == 0 {
                    current = extents.next();
                }
            }
        }
        if current.is_some() {
            return Err(corrupt(object, "is shorter than its extents"));
        }
        if digest_of(blob.len, &blob.extents, &data.finalize()) != blob.digest {
            return Err(corrupt(object, "does not match its digest"));
        }
        file.sync_all().await
    }
}

async fn remove_abandoned_staging_files(directory: &Path) -> io::Result<()> {
    let abandoned_before = SystemTime::now() - ABANDONED_UPLOAD_AGE;
    let mut pending = vec![directory.to_owned()];
    while let Some(dir) = pending.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let metadata = entry.metadata().await?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if is_staging_file(&entry.file_name().to_string_lossy())
                && metadata.modified()? < abandoned_before
            {
                files::remove_if_present(tokio::fs::remove_file(entry.path()).await)?;
                tracing::info!(path = %entry.path().display(), "removed an abandoned staging file");
            }
        }
    }
    Ok(())
}

fn is_staging_file(name: &str) -> bool {
    name.rsplit_once('#')
        .is_some_and(|(_, suffix)| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
}

pub(crate) async fn seed_image_cache(
    digests: &Digests,
    files: &[&Path],
    images: &Path,
) -> io::Result<()> {
    for file in files {
        let blob = digests.blob(file).await?;
        match tokio::fs::hard_link(file, images.join(&blob.digest)).await {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::CrossesDevices
                ) => {}
            Err(error) => return Err(error),
        }
    }
    files::sync(images).await
}

pub(crate) struct Digests {
    known: std::sync::Mutex<HashMap<FileIdentity, Arc<OnceCell<Blob>>>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FileIdentity {
    device: u64,
    inode: u64,
    len: u64,
    modified_ns: i128,
}

impl Digests {
    pub(crate) fn new() -> Self {
        Self {
            known: std::sync::Mutex::default(),
        }
    }

    async fn blob(&self, path: &Path) -> io::Result<Blob> {
        let metadata = tokio::fs::metadata(path).await?;
        let identity = FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.len(),
            modified_ns: i128::from(metadata.mtime()) * 1_000_000_000
                + i128::from(metadata.mtime_nsec()),
        };
        let cell = self
            .known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(identity)
            .or_default()
            .clone();
        cell.get_or_try_init(|| encode(path, None)).await.cloned()
    }
}

async fn encode(path: &Path, mut upload: Option<&mut BufWriter>) -> io::Result<Blob> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let (chunks, mut received) = mpsc::channel(8);
    let reader = tokio::task::spawn_blocking(move || read_nonzero_runs(&file, len, &chunks));
    let mut data = blake3::Hasher::new();
    let mut extents: Vec<[u64; 2]> = Vec::new();
    let mut written = Ok(());
    while let Some((offset, run)) = received.recv().await {
        let run: Bytes = run;
        data.update(&run);
        match extents.last_mut() {
            Some([start, length]) if *start + *length == offset => *length += run.len() as u64,
            _ => extents.push([offset, run.len() as u64]),
        }
        if let Some(writer) = upload.as_deref_mut() {
            written = writer.put(run).await.map_err(io::Error::from);
            if written.is_err() {
                break;
            }
        }
    }
    drop(received);
    let read = reader.await.map_err(io::Error::other)?;
    written?;
    read?;
    let digest = digest_of(len, &extents, &data.finalize());
    Ok(Blob {
        len,
        extents,
        digest,
    })
}

fn read_nonzero_runs(
    file: &std::fs::File,
    len: u64,
    chunks: &mpsc::Sender<(u64, Bytes)>,
) -> io::Result<()> {
    let mut searched = 0;
    while searched < len {
        let Some(data) = next_data(file, searched)? else {
            break;
        };
        let start = data / BLOCK * BLOCK;
        let hole = next_hole(file, data)?;
        let end = hole.div_ceil(BLOCK).saturating_mul(BLOCK).min(len);
        let mut position = start;
        while position < end {
            let chunk_len = READ_CHUNK.min(end - position);
            let mut chunk = vec![0; usize::try_from(chunk_len).map_err(io::Error::other)?];
            file.read_exact_at(&mut chunk, position)?;
            let chunk = Bytes::from(chunk);
            for (from, to) in nonzero_runs(&chunk) {
                if chunks
                    .blocking_send((position + from as u64, chunk.slice(from..to)))
                    .is_err()
                {
                    return Ok(());
                }
            }
            position += chunk_len;
        }
        searched = end.max(data + 1);
    }
    Ok(())
}

fn nonzero_runs(chunk: &[u8]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (index, block) in chunk.chunks(BLOCK as usize).enumerate() {
        if block.iter().all(|byte| *byte == 0) {
            continue;
        }
        let from = index * BLOCK as usize;
        let to = from + block.len();
        match runs.last_mut() {
            Some((_, end)) if *end == from => *end = to,
            _ => runs.push((from, to)),
        }
    }
    runs
}

fn next_data(file: &std::fs::File, from: u64) -> io::Result<Option<u64>> {
    match rustix::fs::seek(file, rustix::fs::SeekFrom::Data(from)) {
        Ok(offset) => Ok(Some(offset)),
        Err(rustix::io::Errno::NXIO) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn next_hole(file: &std::fs::File, from: u64) -> io::Result<u64> {
    Ok(rustix::fs::seek(file, rustix::fs::SeekFrom::Hole(from))?)
}

fn digest_of(len: u64, extents: &[[u64; 2]], data: &blake3::Hash) -> String {
    let mut digest = blake3::Hasher::new();
    digest.update(&len.to_le_bytes());
    for [offset, length] in extents {
        digest.update(&offset.to_le_bytes());
        digest.update(&length.to_le_bytes());
    }
    digest.update(data.as_bytes());
    digest.finalize().to_hex().to_string()
}

fn is_object_name(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn snapshot_prefix(id: &str) -> ObjectPath {
    ObjectPath::from(SNAPSHOTS).child(id)
}

fn record_path(id: &str) -> ObjectPath {
    snapshot_prefix(id).child(RECORD)
}

fn image_path(digest: &str) -> ObjectPath {
    ObjectPath::from(IMAGES).child(digest)
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn object_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn corrupt(object: &ObjectPath, problem: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("stored object {object} {problem}"),
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileExt;

    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("slop-store-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }

        fn store(&self) -> Store {
            Store::open(&format!("file://{}", self.0.join("store").display())).expect("open store")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sparse_file(path: &Path, writes: &[(u64, &[u8])], len: u64) {
        let file = std::fs::File::create(path).expect("create");
        file.set_len(len).expect("length");
        for (offset, bytes) in writes {
            file.write_all_at(bytes, *offset).expect("write");
        }
    }

    #[tokio::test]
    async fn a_blob_keeps_only_nonzero_blocks_and_restores_every_byte() {
        let scratch = Scratch::new("round-trip");
        let store = scratch.store();
        let original = scratch.0.join("original");
        let len = 64 * BLOCK * 1024 + 123;
        sparse_file(
            &original,
            &[
                (0, b"start"),
                (BLOCK, &[0; 4096]),
                (40 * BLOCK * 1024 + 7, b"middle"),
                (len - 3, b"end"),
            ],
            len,
        );
        let object = ObjectPath::from("blob");
        let blob = store.upload(&object, &original).await.expect("upload");
        assert_eq!(blob.len, len);
        assert_eq!(
            blob.extents,
            [
                [0, BLOCK],
                [40 * BLOCK * 1024, BLOCK],
                [64 * BLOCK * 1024, 123]
            ]
        );
        let stored = store.objects.head(&object).await.expect("head");
        assert_eq!(stored.size, 2 * BLOCK + 123);

        let restored = scratch.0.join("restored");
        store
            .download(&object, &blob, &restored)
            .await
            .expect("download");
        assert_eq!(
            std::fs::read(&restored).expect("restored"),
            std::fs::read(&original).expect("original")
        );
        let allocated = std::fs::metadata(&restored).expect("metadata").blocks() * 512;
        assert!(
            allocated < 1 << 20,
            "restored file allocated {allocated} bytes"
        );
    }

    #[tokio::test]
    async fn equal_contents_give_equal_blobs_whether_zeros_are_holes_or_written() {
        let scratch = Scratch::new("deterministic");
        let holes = scratch.0.join("holes");
        let written = scratch.0.join("written");
        sparse_file(&holes, &[(3 * BLOCK + 1, b"x")], 10 * BLOCK);
        let mut bytes = vec![0; 10 * BLOCK as usize];
        bytes[3 * BLOCK as usize + 1] = b'x';
        std::fs::write(&written, bytes).expect("write");
        let digests = Digests::new();
        assert_eq!(
            digests.blob(&holes).await.expect("holes"),
            digests.blob(&written).await.expect("written")
        );
    }

    #[tokio::test]
    async fn a_download_that_does_not_match_its_digest_is_refused() {
        let scratch = Scratch::new("corrupt");
        let store = scratch.store();
        let original = scratch.0.join("original");
        std::fs::write(&original, b"saved memory").expect("write");
        let object = ObjectPath::from("blob");
        let blob = store.upload(&object, &original).await.expect("upload");
        store
            .objects
            .put(&object, Bytes::from_static(b"saved memorX").into())
            .await
            .expect("tamper");
        let error = store
            .download(&object, &blob, &scratch.0.join("restored"))
            .await
            .expect_err("corrupt blob");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        store
            .objects
            .put(&object, Bytes::from_static(b"saved").into())
            .await
            .expect("truncate");
        let error = store
            .download(&object, &blob, &scratch.0.join("restored"))
            .await
            .expect_err("short blob");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn the_sweep_removes_only_old_uploads_that_were_never_published() {
        let scratch = Scratch::new("sweep");
        let store = scratch.store();
        let root = scratch.0.join("store/snapshots");
        let abandoned = r#"{"outcome":"abandoned","at_unix_ms":0}"#;
        for (id, record, age_hours) in [
            ("old", None, 25),
            ("new", None, 1),
            ("abandoned-long-ago", Some(abandoned), 25),
            ("abandoned-lately", Some(abandoned), 1),
            ("unreadable", Some("{}"), 48),
        ] {
            let dir = root.join(id);
            std::fs::create_dir_all(&dir).expect("snapshot dir");
            std::fs::write(dir.join("memory"), b"memory").expect("memory");
            if let Some(record) = record {
                std::fs::write(dir.join(RECORD), record).expect("record");
            }
            let at = SystemTime::now() - Duration::from_secs(age_hours * 60 * 60);
            for entry in std::fs::read_dir(&dir).expect("read dir") {
                let file = std::fs::File::options()
                    .write(true)
                    .open(entry.expect("entry").path())
                    .expect("open");
                file.set_modified(at).expect("set mtime");
            }
        }
        let staged_long_ago = root.join("new/memory#3");
        std::fs::write(&staged_long_ago, b"half an upload").expect("staged");
        let staged_now = root.join("new/scratch.ext4#4");
        std::fs::write(&staged_now, b"an upload in progress").expect("staged");
        std::fs::File::options()
            .write(true)
            .open(&staged_long_ago)
            .expect("open")
            .set_modified(SystemTime::now() - Duration::from_secs(25 * 60 * 60))
            .expect("set mtime");
        store.sweep_abandoned_uploads().await;
        assert!(!staged_long_ago.exists());
        assert!(staged_now.exists());
        assert!(!root.join("old/memory").exists());
        assert!(root.join("new/memory").exists());
        assert!(!root.join("abandoned-long-ago/memory").exists());
        assert!(root.join("abandoned-lately/memory").exists());
        assert!(root.join("unreadable/memory").exists());
    }
}
