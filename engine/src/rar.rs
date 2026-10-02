//! Archive media views. Stored members retain their source volumes; compressed
//! members become readable only after bounded, checksum-verified extraction.
//! No archive path is ever used as a destination path.
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
pub use unrar_rs::archive::ReadSeek;
use unrar_rs::{CompressionMethod, Limits, RarArchive};

pub trait RarVolume: Send + Sync {
    fn open(&self) -> Result<Box<dyn ReadSeek>, &'static str>;
    fn size(&self) -> u64;
}

struct LocalVolume {
    path: PathBuf,
    size: u64,
}
impl RarVolume for LocalVolume {
    fn open(&self) -> Result<Box<dyn ReadSeek>, &'static str> {
        let file = File::open(&self.path).map_err(|_| "source_preparation_failed")?;
        if file
            .metadata()
            .map_err(|_| "source_preparation_failed")?
            .len()
            != self.size
        {
            return Err("source_preparation_failed");
        }
        Ok(Box::new(file))
    }
    fn size(&self) -> u64 {
        self.size
    }
}

pub fn local_volume(path: &Path) -> Result<Arc<dyn RarVolume>, &'static str> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| "source_preparation_failed")?;
    if !metadata.is_file() {
        return Err("unsupported_output");
    }
    Ok(Arc::new(LocalVolume {
        path: path.to_owned(),
        size: metadata.len(),
    }))
}

/// Only the safe basename and declared size are suitable playback metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RarFile {
    pub name: String,
    pub size: u64,
    pub stored: bool,
}
struct StoredPart {
    volume: Arc<dyn RarVolume>,
    physical: u64,
    logical: u64,
    size: u64,
}
struct Spool(PathBuf);
impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
enum Content {
    Stored(Vec<StoredPart>),
    Extracted(Spool),
}
struct Media {
    file: RarFile,
    content: Content,
}

#[derive(Clone)]
pub struct RarMedia(Arc<Media>);
impl RarMedia {
    /// Call on a blocking worker. Volume readers must wait for arriving bytes,
    /// never pretend unavailable torrent pieces are EOF. `spool_budget` is the
    /// caller's reserved remainder of its global cache budget.
    pub fn open(
        volumes: Vec<Arc<dyn RarVolume>>,
        cache: &Path,
        spool_budget: u64,
    ) -> Result<Self, &'static str> {
        let (mut archive, index, file) = catalog(&volumes)?;
        archive.set_limits(Limits {
            max_header_size: 1024 * 1024,
            max_data_segment: spool_budget.max(file.size),
            max_unpacked_size: spool_budget.max(file.size),
            max_dict_size: 64 * 1024 * 1024,
        });
        archive.set_decode_mode(unrar_rs::DecodeMode::Serial);
        let RarFile { name, size, stored } = file;
        let content = if stored {
            let segments = archive.member_segments(index).ok_or("unsupported_output")?;
            let mut logical = 0u64;
            let mut parts = Vec::with_capacity(segments.len());
            for segment in segments {
                let volume = volumes
                    .get(segment.volume_index)
                    .ok_or("unsupported_output")?
                    .clone();
                let end = segment
                    .data_offset
                    .checked_add(segment.data_size)
                    .ok_or("unsupported_output")?;
                if end > volume.size() {
                    return Err("source_preparation_failed");
                }
                parts.push(StoredPart {
                    volume,
                    physical: segment.data_offset,
                    logical,
                    size: segment.data_size,
                });
                logical = logical
                    .checked_add(segment.data_size)
                    .ok_or("unsupported_output")?;
            }
            if logical != size {
                return Err("source_preparation_failed");
            }
            Content::Stored(parts)
        } else {
            if size > spool_budget {
                return Err("torrent_cache_capacity");
            }
            private_directory(cache)?;
            let path = cache.join(format!("rar-{}.media", uuid::Uuid::new_v4()));
            let guard = Spool(path.clone());
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let output = options.open(&path).map_err(|_| "torrent_cache_capacity")?;
            let mut bounded = BoundedWriter {
                output,
                written: 0,
                limit: size.min(spool_budget),
            };
            archive
                .by_index(index)
                .map_err(|_| "source_preparation_failed")?
                .copy_to(&mut bounded)
                .map_err(|_| "processing_failed")?;
            if bounded.written != size {
                return Err("processing_failed");
            }
            Content::Extracted(guard)
        };
        Ok(Self(Arc::new(Media {
            file: RarFile { name, size, stored },
            content,
        })))
    }
    pub fn inspect(volumes: &[Arc<dyn RarVolume>]) -> Result<RarFile, &'static str> {
        Ok(catalog(volumes)?.2)
    }
    pub fn file(&self) -> &RarFile {
        &self.0.file
    }
    pub fn read_range(&self, offset: u64, length: u32) -> Result<Vec<u8>, &'static str> {
        if length > 4 * 1024 * 1024 || offset > self.0.file.size {
            return Err("invalid_range");
        }
        let end = offset
            .saturating_add(u64::from(length))
            .min(self.0.file.size);
        let mut bytes = vec![0; (end - offset) as usize];
        match &self.0.content {
            Content::Extracted(spool) => {
                let mut source = File::open(&spool.0).map_err(|_| "processing_failed")?;
                source
                    .seek(SeekFrom::Start(offset))
                    .map_err(|_| "processing_failed")?;
                source
                    .read_exact(&mut bytes)
                    .map_err(|_| "processing_failed")?;
            }
            Content::Stored(parts) => {
                for part in parts {
                    let start = offset.max(part.logical);
                    let stop = end.min(part.logical + part.size);
                    if start >= stop {
                        continue;
                    }
                    let mut source = part.volume.open()?;
                    source
                        .seek(SeekFrom::Start(part.physical + start - part.logical))
                        .map_err(|_| "processing_failed")?;
                    source
                        .read_exact(&mut bytes[(start - offset) as usize..(stop - offset) as usize])
                        .map_err(|_| "processing_failed")?;
                }
            }
        }
        Ok(bytes)
    }
}

fn catalog(volumes: &[Arc<dyn RarVolume>]) -> Result<(RarArchive, usize, RarFile), &'static str> {
    if volumes.is_empty() || volumes.len() > 1024 {
        return Err("unsupported_output");
    }
    let mut archive =
        RarArchive::open(volumes[0].open()?).map_err(|_| "source_preparation_failed")?;
    for (index, volume) in volumes.iter().enumerate().skip(1) {
        archive
            .add_volume(index, volume.open()?)
            .map_err(|_| "source_preparation_failed")?;
    }
    let (index, entry) = archive
        .entries()
        .enumerate()
        .filter(|(_, entry)| media_name(&entry.name))
        .filter(|(_, entry)| entry.unpacked_size.is_some())
        .max_by_key(|(_, entry)| entry.unpacked_size.unwrap_or_default())
        .ok_or("unsupported_output")?;
    if archive.member_is_encrypted(index) || !archive.is_extractable(&entry.name) {
        return Err("unsupported_output");
    }
    let size = entry.unpacked_size.ok_or("unsupported_output")?;
    if size == 0 {
        return Err("unsupported_output");
    }
    let name = entry
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("media")
        .to_owned();
    let stored = archive.member_compression(index) == Some(CompressionMethod::Store);
    Ok((archive, index, RarFile { name, size, stored }))
}

pub fn media_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let leaf = lower.rsplit(['/', '\\']).next().unwrap_or("");
    leaf.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension,
            "mkv"
                | "mp4"
                | "avi"
                | "mov"
                | "webm"
                | "m4v"
                | "ts"
                | "mpg"
                | "mpeg"
                | "mp3"
                | "flac"
                | "m4a"
                | "aac"
                | "ogg"
                | "wav"
        )
    })
}
fn private_directory(path: &Path) -> Result<(), &'static str> {
    let existed = path.exists();
    std::fs::create_dir_all(path).map_err(|_| "torrent_cache_capacity")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !existed {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| "torrent_cache_capacity")?;
        }
        let metadata = std::fs::symlink_metadata(path).map_err(|_| "torrent_cache_capacity")?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err("torrent_cache_capacity");
        }
    }
    Ok(())
}
struct BoundedWriter {
    output: File,
    written: u64,
    limit: u64,
}
impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.limit.saturating_sub(self.written) {
            return Err(std::io::Error::other("archive output budget exceeded"));
        }
        let count = self.output.write(bytes)?;
        self.written += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stored_member_reads_seek_without_extracting_a_spool() {
        let root = tempfile::tempdir().unwrap();
        let volume = local_volume(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/rar/stored.rar"
        )))
        .unwrap();
        let media = RarMedia::open(vec![volume], root.path(), 1024).unwrap();
        assert_eq!(media.file().name, "clip.mp4");
        assert!(media.file().stored);
        assert_eq!(media.read_range(10, 8).unwrap(), b"01234567");
        assert_eq!(media.read_range(97, 8).unwrap(), b"789");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
    #[test]
    fn invalid_archives_fail_without_exposing_source_paths() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private.rar");
        std::fs::write(&path, b"not an archive").unwrap();
        assert_eq!(
            RarMedia::open(vec![local_volume(&path).unwrap()], root.path(), 1024).err(),
            Some("source_preparation_failed")
        );
    }
    #[test]
    fn stored_split_member_seeks_across_volume_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let volumes = (1..=3)
            .map(|index| {
                local_volume(
                    &Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join(format!("assets/rar/split.part{index}.rar")),
                )
                .unwrap()
            })
            .collect();
        let media = RarMedia::open(volumes, root.path(), 20000).unwrap();
        assert_eq!(media.file().size, 10000);
        assert_eq!(media.read_range(3800, 16).unwrap(), b"0123456789012345");
        assert_eq!(media.read_range(9997, 32).unwrap(), b"789");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
    #[test]
    fn compressed_member_is_bounded_verified_and_removed_after_last_view() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let volume = local_volume(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/rar/compressed.rar"
        )))
        .unwrap();
        assert_eq!(
            RarMedia::open(vec![volume.clone()], &cache, 9999).err(),
            Some("torrent_cache_capacity")
        );
        assert!(!cache.exists());
        let media = RarMedia::open(vec![volume], &cache, 10000).unwrap();
        assert!(!media.file().stored);
        assert_eq!(media.read_range(10, 12).unwrap(), b"012345678901");
        let second = media.clone();
        drop(media);
        assert_eq!(second.read_range(9987, 20).unwrap(), b"7890123456789");
        drop(second);
        assert_eq!(std::fs::read_dir(cache).unwrap().count(), 0);
    }
}
