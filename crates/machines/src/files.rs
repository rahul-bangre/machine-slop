use std::{io, path::Path, process::Stdio};

use serde::{Serialize, de::DeserializeOwned};
use tokio::process::Command;

pub async fn sync(path: &Path) -> io::Result<()> {
    tokio::fs::File::open(path).await?.sync_all().await
}

pub async fn sync_contents(dir: &Path) -> io::Result<()> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_file() {
            sync(&entry.path()).await?;
        }
    }
    sync(dir).await
}

pub async fn commit(dir: &Path, record_name: &str, record: &impl Serialize) -> io::Result<()> {
    sync_contents(dir).await?;
    replace_record(dir, record_name, record).await?;
    match dir.parent() {
        Some(parent) => sync(parent).await,
        None => Ok(()),
    }
}

pub async fn replace_record(
    dir: &Path,
    record_name: &str,
    record: &impl Serialize,
) -> io::Result<()> {
    let contents = serde_json::to_vec(record).map_err(io::Error::other)?;
    write_private(&dir.join(record_name), &contents).await
}

pub async fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut staged = path.as_os_str().to_owned();
    staged.push(".tmp");
    let staged = std::path::PathBuf::from(staged);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&staged)
        .await?;
    tokio::io::AsyncWriteExt::write_all(&mut file, contents).await?;
    file.sync_all().await?;
    tokio::fs::set_permissions(&staged, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .await?;
    tokio::fs::rename(&staged, path).await?;
    match path.parent() {
        Some(dir) => sync(dir).await,
        None => Ok(()),
    }
}

pub async fn read_record<T: DeserializeOwned>(dir: &Path, record_name: &str) -> io::Result<T> {
    let bytes = tokio::fs::read(dir.join(record_name)).await?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

pub async fn discard(dir: &Path, record_name: &str) -> io::Result<()> {
    remove_if_present(tokio::fs::remove_file(dir.join(record_name)).await)?;
    sync(dir).await?;
    remove_dir_logged(dir).await;
    Ok(())
}

pub async fn remove_dir_logged(dir: &Path) {
    if let Err(error) = remove_dir_if_present(dir).await {
        tracing::error!(path = %dir.display(), %error, "directory cleanup failed");
    }
}

pub async fn publish_dir(staged: &Path, published: &Path) -> io::Result<()> {
    sync_contents(staged).await?;
    tokio::fs::rename(staged, published).await?;
    match published.parent() {
        Some(parent) => sync(parent).await,
        None => Ok(()),
    }
}

pub fn remove_if_present(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

pub async fn remove_dir_if_present(dir: &Path) -> io::Result<()> {
    remove_if_present(tokio::fs::remove_dir_all(dir).await)
}

pub async fn link_immutable(from: &Path, to: &Path) -> io::Result<()> {
    match tokio::fs::hard_link(from, to).await {
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            tokio::fs::copy(from, to).await.map(|_| ())
        }
        result => result,
    }
}

pub async fn copy_disk(from: &Path, to: &Path) -> io::Result<()> {
    let output = Command::new("cp")
        .args(["--reflink=auto", "--sparse=always", "--"])
        .arg(from)
        .arg(to)
        .stdin(Stdio::null())
        .output()
        .await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "copying {} failed: {}",
            from.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
