//! Unique physical artifact-object allocation and layout.
//!
//! Object IDs are deliberately independent of cache keys and content hashes.
//! A key can be republished many times while an older hit still owns an
//! `Arc`; every publication therefore receives a never-overwritten physical
//! object name.  The counter is stored as little-endian bytes so the low byte
//! naturally distributes sequential objects across the fixed bucket set.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(test)]
use std::fs::{File, OpenOptions};
#[cfg(test)]
use std::io::{Read, Write};

const OBJECTS_ROOT: &str = "objects";
const DELETING_ROOT: &str = "deleting";
const POINTERS_ROOT: &str = ".object-pointers";
// The allocation/pointer half of this module (counter, `allocate_object_id`,
// pointer read/write/remove) is exercised only by the in-file tests until the
// #1164 publication path lands its real consumers; it is `#[cfg(test)]`-gated
// so `-D warnings` non-test builds see no dead code.
#[cfg(test)]
const COUNTER_FILE: &str = ".object-counter";
#[cfg(test)]
const COUNTER_LOCK: &str = ".object-counter.lock";
const BUCKET_COUNT: u16 = 256;
const HEX: &[u8; 16] = b"0123456789abcdef";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::daemon::server) struct ArtifactObjectId(pub(crate) u64);

impl ArtifactObjectId {
    pub(crate) fn name(self) -> String {
        let mut name = String::with_capacity(16);
        for byte in self.0.to_le_bytes() {
            name.push(HEX[(byte >> 4) as usize] as char);
            name.push(HEX[(byte & 0x0f) as usize] as char);
        }
        name
    }

    pub(crate) fn parse(name: &str) -> Option<Self> {
        if name.len() != 16 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let mut bytes = [0_u8; 8];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let high = name.as_bytes()[index * 2].to_ascii_lowercase();
            let low = name.as_bytes()[index * 2 + 1].to_ascii_lowercase();
            *byte = (hex_value(high)? << 4) | hex_value(low)?;
        }
        Some(Self(u64::from_le_bytes(bytes)))
    }

    fn bucket(self) -> String {
        self.name()[..2].to_string()
    }

    fn leaf(self) -> String {
        self.name()[2..].to_string()
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

pub(crate) fn object_root(artifact_dir: &Path) -> PathBuf {
    artifact_dir.join(OBJECTS_ROOT)
}

pub(crate) fn deleting_root(artifact_dir: &Path) -> PathBuf {
    artifact_dir.join(DELETING_ROOT)
}

fn pointer_root(artifact_dir: &Path) -> PathBuf {
    artifact_dir.join(POINTERS_ROOT)
}

#[cfg(test)]
pub(crate) fn object_path(artifact_dir: &Path, id: ArtifactObjectId) -> PathBuf {
    object_root(artifact_dir).join(id.bucket()).join(id.leaf())
}

pub(crate) fn deleting_object_path(artifact_dir: &Path, id: ArtifactObjectId) -> PathBuf {
    deleting_root(artifact_dir)
        .join(id.bucket())
        .join(id.leaf())
}

#[cfg(test)]
pub(crate) fn object_pointer_path(artifact_dir: &Path, key: &str) -> PathBuf {
    pointer_root(artifact_dir).join(format!("{key}.object"))
}

/// Create the stable object-store roots and all 256 buckets. This is cheap and
/// idempotent, so startup can establish the complete layout before any
/// publication or orphan sweep begins.
pub(crate) fn ensure_object_layout(artifact_dir: &Path) -> io::Result<()> {
    let objects = object_root(artifact_dir);
    let deleting = deleting_root(artifact_dir);
    fs::create_dir_all(&objects)?;
    fs::create_dir_all(&deleting)?;
    fs::create_dir_all(pointer_root(artifact_dir))?;
    for bucket in 0..BUCKET_COUNT {
        let name = format!("{bucket:02x}");
        fs::create_dir_all(objects.join(&name))?;
        fs::create_dir_all(deleting.join(&name))?;
    }
    Ok(())
}

#[cfg(test)]
fn sync_file(path: &Path) -> io::Result<()> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?
        .sync_all()
}

#[cfg(test)]
fn write_counter(path: &Path, value: u64) -> io::Result<()> {
    let temporary = path.with_file_name(format!(".{COUNTER_FILE}.tmp-{}", std::process::id()));
    {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&value.to_le_bytes())?;
        file.sync_all()?;
    }
    let result = fs::rename(&temporary, path);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
fn read_counter(path: &Path) -> io::Result<Option<u64>> {
    let mut bytes = [0_u8; 8];
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    file.read_exact(&mut bytes)?;
    Ok(Some(u64::from_le_bytes(bytes)))
}

#[cfg(test)]
fn path_occupied(artifact_dir: &Path, id: ArtifactObjectId) -> bool {
    object_path(artifact_dir, id).exists()
        || deleting_object_path(artifact_dir, id).exists()
        || object_root(artifact_dir)
            .join(id.bucket())
            .join(format!(".tmp-{}", id.leaf()))
            .exists()
}

/// Allocate and durably checkpoint one object ID. The lock is intentionally
/// separate from the publication lock: unrelated keys can publish in
/// parallel, while ID allocation remains serialized across processes.
#[cfg(test)]
pub(crate) fn allocate_object_id(artifact_dir: &Path) -> io::Result<ArtifactObjectId> {
    ensure_object_layout(artifact_dir)?;
    let lock_path = artifact_dir.join(COUNTER_LOCK);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    fs2::FileExt::lock_exclusive(&lock)?;

    let counter_path = artifact_dir.join(COUNTER_FILE);
    let last = match read_counter(&counter_path) {
        Ok(Some(value)) => value,
        Ok(None) => u64::MAX,
        Err(error) => {
            tracing::warn!(%error, "object counter is unreadable; recovering by collision checks");
            u64::MAX
        }
    };
    let mut candidate = last.wrapping_add(1);
    if candidate == 0 && last == u64::MAX {
        tracing::warn!("artifact object counter wrapped; collision checks remain enabled");
        crate::core::lifecycle::write_event(
            "artifact_object_counter_wrapped",
            serde_json::json!({ "counter": "u64::MAX" }),
        );
    }
    loop {
        let id = ArtifactObjectId(candidate);
        if !path_occupied(artifact_dir, id) {
            write_counter(&counter_path, candidate)?;
            fs2::FileExt::unlock(&lock)?;
            return Ok(id);
        }
        candidate = candidate.wrapping_add(1);
        if candidate == 0 {
            tracing::warn!("artifact object allocator skipped through counter wrap");
        }
    }
}

#[cfg(test)]
pub(crate) fn read_object_pointer(
    artifact_dir: &Path,
    key: &str,
) -> io::Result<Option<ArtifactObjectId>> {
    let path = object_pointer_path(artifact_dir, key);
    let value = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let value = value.trim();
    let id = ArtifactObjectId::parse(value).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid artifact object pointer",
        )
    })?;
    Ok(Some(id))
}

#[cfg(test)]
pub(crate) fn write_object_pointer(
    artifact_dir: &Path,
    key: &str,
    id: ArtifactObjectId,
) -> io::Result<()> {
    ensure_object_layout(artifact_dir)?;
    let target = object_pointer_path(artifact_dir, key);
    let temporary = target.with_file_name(format!(
        ".{}.tmp-{}",
        target.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(id.name().as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    let result = fs::rename(&temporary, &target);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    if result.is_ok() {
        let _ = sync_file(&target);
    }
    result
}

#[cfg(test)]
pub(crate) fn remove_object_pointer_if_matches(
    artifact_dir: &Path,
    key: &str,
    expected: ArtifactObjectId,
) -> io::Result<bool> {
    let path = object_pointer_path(artifact_dir, key);
    if read_object_pointer(artifact_dir, key)? != Some(expected) {
        return Ok(false);
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Move object files that are no longer named by any durable key pointer into
/// the deletion queue. This is idempotent and keeps cleanup recoverable.
pub(crate) fn queue_orphaned_objects(artifact_dir: &Path) -> io::Result<usize> {
    ensure_object_layout(artifact_dir)?;
    let mut live = HashSet::new();
    for entry in fs::read_dir(pointer_root(artifact_dir))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        if let Ok(value) = fs::read_to_string(entry.path()) {
            if let Some(id) = ArtifactObjectId::parse(value.trim()) {
                live.insert(id);
            }
        }
    }

    let mut queued = 0;
    for bucket in fs::read_dir(object_root(artifact_dir))? {
        let bucket = bucket?;
        if !bucket.file_type()?.is_dir() {
            continue;
        }
        let bucket_name = bucket.file_name().to_string_lossy().to_string();
        for entry in fs::read_dir(bucket.path())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let leaf = entry.file_name().to_string_lossy().to_string();
            let Some(id) = ArtifactObjectId::parse(&format!("{bucket_name}{leaf}")) else {
                continue;
            };
            if live.contains(&id) {
                continue;
            }
            let target = deleting_object_path(artifact_dir, id);
            match fs::rename(entry.path(), target) {
                Ok(()) => queued += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(queued)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn little_endian_names_spread_low_byte_first() {
        assert_eq!(ArtifactObjectId(0).name(), "0000000000000000");
        assert_eq!(ArtifactObjectId(1).name(), "0100000000000000");
        assert_eq!(
            ArtifactObjectId(0x0123_4567_89ab_cdef).name(),
            "efcdab8967452301"
        );
        assert_eq!(
            ArtifactObjectId::parse("efcdab8967452301"),
            Some(ArtifactObjectId(0x0123_4567_89ab_cdef))
        );
    }

    #[test]
    fn startup_creates_both_bucket_sets() {
        let dir = tempfile::tempdir().unwrap();
        ensure_object_layout(dir.path()).unwrap();
        for bucket in 0..=u8::MAX {
            let name = format!("{bucket:02x}");
            assert!(object_root(dir.path()).join(&name).is_dir());
            assert!(deleting_root(dir.path()).join(&name).is_dir());
        }
    }

    #[test]
    fn allocation_recovers_from_stale_counter_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        ensure_object_layout(dir.path()).unwrap();
        fs::write(dir.path().join(COUNTER_FILE), 7_u64.to_le_bytes()).unwrap();
        let stale = ArtifactObjectId(8);
        fs::create_dir_all(object_path(dir.path(), stale)).unwrap();
        let allocated = allocate_object_id(dir.path()).unwrap();
        assert_eq!(allocated, ArtifactObjectId(9));
        assert_eq!(
            read_counter(&dir.path().join(COUNTER_FILE)).unwrap(),
            Some(9)
        );
    }

    #[test]
    fn pointer_round_trip_and_compare_remove() {
        let dir = tempfile::tempdir().unwrap();
        let id = allocate_object_id(dir.path()).unwrap();
        write_object_pointer(dir.path(), "abc", id).unwrap();
        assert_eq!(read_object_pointer(dir.path(), "abc").unwrap(), Some(id));
        assert!(!remove_object_pointer_if_matches(
            dir.path(),
            "abc",
            ArtifactObjectId(id.0.wrapping_add(1))
        )
        .unwrap());
        assert!(remove_object_pointer_if_matches(dir.path(), "abc", id).unwrap());
        assert_eq!(read_object_pointer(dir.path(), "abc").unwrap(), None);
    }

    #[test]
    fn orphan_objects_are_moved_to_recoverable_queue() {
        let dir = tempfile::tempdir().unwrap();
        let orphan = allocate_object_id(dir.path()).unwrap();
        fs::write(object_path(dir.path(), orphan), b"payload").unwrap();
        let live = allocate_object_id(dir.path()).unwrap();
        fs::write(object_path(dir.path(), live), b"payload").unwrap();
        write_object_pointer(dir.path(), "key", live).unwrap();

        assert_eq!(queue_orphaned_objects(dir.path()).unwrap(), 1);
        assert!(!object_path(dir.path(), orphan).exists());
        assert!(deleting_object_path(dir.path(), orphan).exists());
        assert!(object_path(dir.path(), live).exists());
    }
}
