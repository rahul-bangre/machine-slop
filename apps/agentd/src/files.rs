use std::{
    fs::Metadata,
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

use axum::{
    body::{Body, Bytes},
    extract::{Path as UrlPath, Query},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::error::ApiError;
use futures_util::{StreamExt, TryStreamExt};
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};
use tokio_util::io::{ReaderStream, StreamReader, SyncIoBridge};

const NEW_FILE_MODE: u32 = 0o644;
const ARCHIVE_CHUNK_BYTES: usize = 64 * 1024;
const ARCHIVE_CHUNKS_IN_FLIGHT: usize = 4;
const NONBLOCKING_OPEN: i32 = rustix::fs::OFlags::NONBLOCK.bits() as i32;

pub async fn read(UrlPath(path): UrlPath<String>) -> Result<Response, ApiError> {
    let path = machine_path(&path)?;
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(NONBLOCKING_OPEN)
        .open(&path)
        .await?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() {
        return Err(not_a_regular_file(&path));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, metadata.len().to_string()),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        Body::from_stream(ReaderStream::new(file.take(metadata.len()))),
    )
        .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteOptions {
    mode: Option<String>,
}

pub async fn write(
    UrlPath(path): UrlPath<String>,
    Query(options): Query<WriteOptions>,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let requested = machine_path(&path)?;
    let explicit_mode = options.mode.as_deref().map(parse_mode).transpose()?;
    tokio::fs::create_dir_all(requested.parent().expect("a machine path has a parent")).await?;
    let path = match tokio::fs::canonicalize(&requested).await {
        Ok(resolved) => resolved,
        Err(error) if error.kind() == io::ErrorKind::NotFound => requested,
        Err(error) => return Err(error.into()),
    };
    let parent = path.parent().expect("a machine path has a parent");
    let mode = match explicit_mode {
        Some(mode) => mode,
        None => match tokio::fs::metadata(&path).await {
            Ok(existing) => existing.permissions().mode() & 0o7777,
            Err(error) if error.kind() == io::ErrorKind::NotFound => NEW_FILE_MODE,
            Err(error) => return Err(error.into()),
        },
    };
    let staged = tempfile::Builder::new()
        .prefix(".agentd-upload-")
        .tempfile_in(parent)?;
    let mut file = tokio::fs::File::from_std(staged.as_file().try_clone()?);
    let mut body = body.into_data_stream();
    while let Some(chunk) = body.next().await {
        file.write_all(&chunk.map_err(io::Error::other)?).await?;
    }
    file.flush().await?;
    drop(file);
    staged
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))?;
    staged.persist(&path).map_err(|error| error.error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn remove(UrlPath(path): UrlPath<String>) -> Result<StatusCode, ApiError> {
    let path = machine_path(&path)?;
    let removed = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.is_dir() => tokio::fs::remove_dir_all(&path).await,
        Ok(_) => tokio::fs::remove_file(&path).await,
        Err(error) => Err(error),
    };
    match removed {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(StatusCode::NO_CONTENT),
    }
}

pub async fn read_archive(UrlPath(path): UrlPath<String>) -> Result<Response, ApiError> {
    let path = machine_path(&path)?;
    if !tokio::fs::metadata(&path).await?.is_dir() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("{} is not a directory", path.display()),
        ));
    }
    let (chunks, mut received) = mpsc::channel(ARCHIVE_CHUNKS_IN_FLIGHT);
    tokio::task::spawn_blocking(move || {
        let mut builder = tar::Builder::new(ChunkSender {
            chunks,
            pending: Vec::with_capacity(ARCHIVE_CHUNK_BYTES),
            failed: false,
        });
        let written = append_tree(&mut builder, &path, Path::new(""))
            .and_then(|()| builder.finish())
            .and_then(|()| builder.get_mut().flush());
        if let Err(error) = written {
            builder.get_mut().fail(error);
        }
    });
    let stream = async_stream::stream! {
        while let Some(chunk) = received.recv().await {
            yield chunk;
        }
    };
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-tar"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

pub async fn write_archive(
    UrlPath(path): UrlPath<String>,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let path = machine_path(&path)?;
    tokio::fs::create_dir_all(&path).await?;
    let reader = SyncIoBridge::new(StreamReader::new(
        body.into_data_stream().map_err(io::Error::other),
    ));
    tokio::task::spawn_blocking(move || extract_archive(reader, &path))
        .await
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        .map_err(|error| match error.kind() {
            io::ErrorKind::Other | io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => {
                ApiError(StatusCode::UNPROCESSABLE_ENTITY, error.to_string())
            }
            _ => error.into(),
        })?;
    Ok(StatusCode::NO_CONTENT)
}

fn append_tree(
    builder: &mut tar::Builder<ChunkSender>,
    directory: &Path,
    name: &Path,
) -> io::Result<()> {
    let mut entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries.collect::<io::Result<Vec<_>>>()?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = name.join(entry.file_name());
        match append_entry(builder, &path, &name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            appended => appended?,
        }
    }
    Ok(())
}

fn append_entry(
    builder: &mut tar::Builder<ChunkSender>,
    path: &Path,
    name: &Path,
) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        builder.append_data(&mut header_for(&metadata), name, io::empty())?;
        append_tree(builder, path, name)
    } else if file_type.is_symlink() {
        builder.append_link(&mut header_for(&metadata), name, std::fs::read_link(path)?)
    } else if file_type.is_file() {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(NONBLOCKING_OPEN)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Ok(());
        }
        let contents = DeclaredLength {
            remaining: metadata.len(),
            file,
            path,
        };
        builder.append_data(&mut header_for(&metadata), name, contents)
    } else {
        Ok(())
    }
}

fn header_for(metadata: &Metadata) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_metadata_in_mode(metadata, tar::HeaderMode::Complete);
    header
}

struct DeclaredLength<'a> {
    file: std::fs::File,
    remaining: u64,
    path: &'a Path,
}

impl Read for DeclaredLength<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let limit = buffer
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        if limit == 0 {
            return Ok(0);
        }
        let count = self.file.read(&mut buffer[..limit])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{} shrank while it was archived", self.path.display()),
            ));
        }
        self.remaining -= count as u64;
        Ok(count)
    }
}

fn extract_archive(reader: impl io::Read, destination: &Path) -> io::Result<()> {
    let destination = destination.canonicalize()?;
    let mut archive = tar::Archive::new(reader);
    let mut directories = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type() == tar::EntryType::Directory {
            directories.push(entry);
        } else {
            unpack_inside(&mut entry, &destination)?;
        }
    }
    directories.sort_by(|a, b| b.path_bytes().cmp(&a.path_bytes()));
    for mut directory in directories {
        unpack_inside(&mut directory, &destination)?;
    }
    Ok(())
}

fn unpack_inside<R: io::Read>(entry: &mut tar::Entry<R>, destination: &Path) -> io::Result<()> {
    if entry.unpack_in(destination)? {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "archive entry {} leaves the destination directory",
                entry.path_bytes().escape_ascii()
            ),
        ))
    }
}

struct ChunkSender {
    chunks: mpsc::Sender<io::Result<Bytes>>,
    pending: Vec<u8>,
    failed: bool,
}

impl ChunkSender {
    fn fail(&mut self, error: io::Error) {
        self.failed = true;
        self.pending.clear();
        let _ = self.chunks.blocking_send(Err(error));
    }
}

impl Write for ChunkSender {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.failed {
            return Err(io::Error::other("archive already failed"));
        }
        self.pending.extend_from_slice(bytes);
        if self.pending.len() >= ARCHIVE_CHUNK_BYTES {
            self.flush()?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::replace(
            &mut self.pending,
            Vec::with_capacity(ARCHIVE_CHUNK_BYTES),
        ));
        self.chunks
            .blocking_send(Ok(chunk))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "archive reader went away"))
    }
}

fn machine_path(path: &str) -> Result<PathBuf, ApiError> {
    let path = Path::new("/").join(path);
    let normal = path
        .components()
        .skip(1)
        .all(|component| matches!(component, Component::Normal(_)));
    if normal && path.parent().is_some() {
        Ok(path)
    } else {
        Err(ApiError(
            StatusCode::BAD_REQUEST,
            "path must be absolute without . or .. components".into(),
        ))
    }
}

fn parse_mode(mode: &str) -> Result<u32, ApiError> {
    u32::from_str_radix(mode, 8)
        .ok()
        .filter(|mode| *mode <= 0o7777)
        .ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "mode must be octal permission bits, like 755".into(),
            )
        })
}

fn not_a_regular_file(path: &Path) -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        format!("{} is not a regular file", path.display()),
    )
}
