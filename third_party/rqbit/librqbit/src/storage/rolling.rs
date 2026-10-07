// SPDX-License-Identifier: Apache-2.0
// Maintained playback-gateway extension.
//! Fixed disk slots for verified torrent pieces. This storage is ephemeral:
//! it never resumes a bitfield from disk, and never allocates full payload files.
use super::{
    BoxStorageFactory, PieceReservation, StorageFactory, StorageFactoryExt, TorrentStorage,
};
use crate::torrent_state::{ManagedTorrentShared, TorrentMetadata};
use anyhow::Context;
use librqbit_core::lengths::ValidPieceIndex;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy)]
pub struct RollingStorageFactory {
    pub capacity: u64,
}
impl StorageFactory for RollingStorageFactory {
    type Storage = RollingStorage;
    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<RollingStorage> {
        let piece_len = u64::from(metadata.lengths.default_piece_length());
        let count = self.capacity / piece_len;
        anyhow::ensure!(count >= 2, "rolling cache needs at least two pieces");
        let count = usize::try_from(count.min(u64::from(metadata.lengths.total_pieces())))?;
        let path = shared.options.output_folder.join("pieces.cache");
        let mut opts = OpenOptions::new();
        opts.create_new(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(path)?;
        // Slot reuse never grows this file. Physical block rounding is bounded separately.
        file.set_len((count as u64) * piece_len)?;
        Ok(RollingStorage {
            state: Arc::new(Mutex::new(State {
                file: Some(file),
                slots: vec![None; count],
                clock: 0,
            })),
            piece_len,
            capacity: self.capacity,
            files: metadata
                .file_infos
                .iter()
                .map(|f| (f.offset_in_torrent, f.len))
                .collect(),
        })
    }
    fn clone_box(&self) -> BoxStorageFactory {
        (*self).boxed()
    }
}
#[derive(Clone, Copy)]
struct Slot {
    piece: ValidPieceIndex,
    touched: u64,
}
struct State {
    file: Option<File>,
    slots: Vec<Option<Slot>>,
    clock: u64,
}
pub struct RollingStorage {
    state: Arc<Mutex<State>>,
    piece_len: u64,
    capacity: u64,
    files: Vec<(u64, u64)>,
}
impl RollingStorage {
    fn range(&self, file: usize, offset: u64, len: usize) -> anyhow::Result<u64> {
        let (base, size) = *self.files.get(file).context("invalid cache file")?;
        anyhow::ensure!(
            offset
                .checked_add(len as u64)
                .is_some_and(|end| end <= size),
            "invalid cache range"
        );
        Ok(base + offset)
    }
    fn io(
        &self,
        file: usize,
        offset: u64,
        len: usize,
        mut action: impl FnMut(&mut File, usize, usize) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut absolute = self.range(file, offset, len)?;
        let mut done = 0;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("cache lock poisoned"))?;
        while done < len {
            let pid = u32::try_from(absolute / self.piece_len)?;
            let slot = state
                .slots
                .iter()
                .position(|s| s.is_some_and(|s| s.piece.get() == pid))
                .context("piece not resident")?;
            let within = absolute % self.piece_len;
            let count = (len - done).min((self.piece_len - within) as usize);
            state.clock += 1;
            let clock = state.clock;
            state.slots[slot].as_mut().unwrap().touched = clock;
            let file = state.file.as_mut().context("cache retired")?;
            file.seek(SeekFrom::Start(slot as u64 * self.piece_len + within))?;
            action(file, done, count)?;
            absolute += count as u64;
            done += count;
        }
        Ok(())
    }
}
impl TorrentStorage for RollingStorage {
    fn rolling_capacity(&self) -> Option<u64> {
        Some(self.capacity)
    }
    fn resident_pieces(&self) -> Vec<ValidPieceIndex> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slots
            .iter()
            .flatten()
            .map(|s| s.piece)
            .collect()
    }
    fn reserve_piece(
        &self,
        piece: ValidPieceIndex,
        protected: &[ValidPieceIndex],
    ) -> anyhow::Result<PieceReservation> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("cache lock poisoned"))?;
        anyhow::ensure!(s.file.is_some(), "cache retired");
        if s.slots.iter().any(|v| v.is_some_and(|v| v.piece == piece)) {
            return Ok(PieceReservation::Reserved { evicted: None });
        }
        let index = s.slots.iter().position(Option::is_none).or_else(|| {
            s.slots
                .iter()
                .enumerate()
                .filter_map(|(i, v)| {
                    let v = v.as_ref()?;
                    (!protected.contains(&v.piece)).then_some((i, v.touched))
                })
                .min_by_key(|(_, t)| *t)
                .map(|(i, _)| i)
        });
        let Some(index) = index else {
            return Ok(PieceReservation::Full);
        };
        let evicted = s.slots[index].map(|v| v.piece);
        s.clock += 1;
        let touched = s.clock;
        s.slots[index] = Some(Slot { piece, touched });
        Ok(PieceReservation::Reserved { evicted })
    }
    fn init(&mut self, _: &ManagedTorrentShared, _: &TorrentMetadata) -> anyhow::Result<()> {
        Ok(())
    }
    fn pread_exact(&self, file: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.io(file, offset, buf.len(), |f, start, n| {
            f.read_exact(&mut buf[start..start + n])?;
            Ok(())
        })
    }
    fn pwrite_all(&self, file: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.io(file, offset, buf.len(), |f, start, n| {
            f.write_all(&buf[start..start + n])?;
            Ok(())
        })
    }
    fn on_piece_completed(&self, piece: ValidPieceIndex) -> anyhow::Result<()> {
        let s = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("cache lock poisoned"))?;
        anyhow::ensure!(
            s.slots.iter().flatten().any(|v| v.piece == piece),
            "completed piece not resident"
        );
        Ok(())
    }
    fn ensure_file_length(&self, _: usize, _: u64) -> anyhow::Result<()> {
        Ok(())
    }
    fn remove_file(&self, _: usize, _: &Path) -> anyhow::Result<()> {
        // Session deletion joins readers/workers first. Closing the shared FD
        // releases blocks even if a stopped handle still retains this storage.
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("cache lock poisoned"))?;
        state.file.take();
        state.slots.fill(None);
        Ok(())
    }
    fn remove_directory_if_empty(&self, _: &Path) -> anyhow::Result<()> {
        Ok(())
    }
    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(Self {
            state: self.state.clone(),
            piece_len: self.piece_len,
            capacity: self.capacity,
            files: self.files.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librqbit_core::lengths::Lengths;
    fn storage() -> (tempfile::NamedTempFile, RollingStorage, Lengths) {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(32768).unwrap();
        let storage = RollingStorage {
            state: Arc::new(Mutex::new(State {
                file: Some(file.reopen().unwrap()),
                slots: vec![None; 2],
                clock: 0,
            })),
            piece_len: 16384,
            capacity: 32768,
            // First file ends inside a piece; the second shares its boundary.
            files: vec![(0, 20000), (20000, 45536)],
        };
        (file, storage, Lengths::new(65536, 16384).unwrap())
    }
    #[test]
    fn pinned_slots_wait_and_abandoned_partial_slots_can_be_reused() {
        let (file, storage, lengths) = storage();
        let p = |id| lengths.validate_piece_index(id).unwrap();
        assert!(matches!(
            storage.reserve_piece(p(0), &[]).unwrap(),
            PieceReservation::Reserved { evicted: None }
        ));
        storage.pwrite_all(0, 0, &[7; 16384]).unwrap();
        storage.on_piece_completed(p(0)).unwrap();
        storage.reserve_piece(p(1), &[]).unwrap();
        storage.pwrite_all(0, 16384, &[8; 100]).unwrap(); // abandoned partial write
        assert!(matches!(
            storage.reserve_piece(p(2), &[p(0), p(1)]).unwrap(),
            PieceReservation::Full
        ));
        assert!(
            matches!(storage.reserve_piece(p(2), &[p(0)]).unwrap(), PieceReservation::Reserved{evicted:Some(id)} if id == p(1))
        );
        let mut bytes = vec![0; 16384];
        storage.pread_exact(0, 0, &mut bytes).unwrap();
        assert_eq!(bytes, vec![7; 16384]);
        assert!(storage.pread_exact(0, 16384, &mut [0; 1]).is_err());
        storage.pwrite_all(1, 32768 - 20000, &[9; 16384]).unwrap();
        assert_eq!(file.as_file().metadata().unwrap().len(), 32768);
        assert!(storage.pread_exact(1, 45536, &mut [0; 1]).is_err());
    }
    #[test]
    fn shared_file_boundary_maps_to_one_slot_and_cloned_storage_preserves_it() {
        let (_, storage, lengths) = storage();
        let piece = lengths.validate_piece_index(1).unwrap();
        storage.reserve_piece(piece, &[]).unwrap();
        storage.pwrite_all(0, 16384, &vec![3; 3616]).unwrap();
        storage.pwrite_all(1, 0, &vec![4; 12768]).unwrap();
        let taken = storage.take().unwrap();
        let mut first = vec![0; 3616];
        let mut second = vec![0; 12768];
        taken.pread_exact(0, 16384, &mut first).unwrap();
        taken.pread_exact(1, 0, &mut second).unwrap();
        assert_eq!(first, vec![3; 3616]);
        assert_eq!(second, vec![4; 12768]);
        taken.ensure_file_length(1, u64::MAX).unwrap();
        assert_eq!(
            storage
                .state
                .lock()
                .unwrap()
                .file
                .as_ref()
                .unwrap()
                .metadata()
                .unwrap()
                .len(),
            32768
        );
        taken.remove_file(0, Path::new("movie.mkv")).unwrap();
        assert!(storage.state.lock().unwrap().file.is_none());
        assert!(storage.pread_exact(0, 16384, &mut [0; 1]).is_err());
        assert!(storage.reserve_piece(piece, &[]).is_err());
    }
}
