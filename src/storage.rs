use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const LIMIT: u64 = 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn lock_instance(directory: &Path) -> io::Result<Option<File>> {
    let lock = OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(directory.join("app.lock"))?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

pub fn read_text(path: &Path) -> io::Result<String> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > LIMIT {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Expected a config file no larger than 1 MiB"));
    }
    let mut text = String::new();
    file.take(LIMIT + 1).read_to_string(&mut text)?;
    if text.len() as u64 > LIMIT {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Config exceeds 1 MiB"));
    }
    Ok(text)
}

/// Write beside the destination so the final rename stays on the same volume.
pub fn write_text(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    loop {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = directory.join(format!(".simpledarkeq-{}-{sequence}.tmp", std::process::id()));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let written = file.write_all(bytes).and_then(|_| file.sync_all());
        drop(file);
        let result = written.and_then(|_| fs::rename(&temporary, path));
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        return result;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_instance_is_rejected_and_can_reopen_after_exit() {
        let directory = std::env::temp_dir().join(format!("simpledarkeq-instance-test-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let guard = lock_instance(&directory).unwrap().unwrap();
        assert!(lock_instance(&directory).unwrap().is_none());
        drop(guard);
        assert!(lock_instance(&directory).unwrap().is_some());
        fs::remove_file(directory.join("app.lock")).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn replaces_settings_and_cleans_up_temporary_files_on_failed_rename() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let directory = std::env::temp_dir().join(format!("simpledarkeq-test-{}-{nonce}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("config.json");
        write_text(&path, b"{\"value\":1}").unwrap();
        write_text(&path, b"{\"value\":2}").unwrap();
        assert_eq!(read_text(&path).unwrap(), "{\"value\":2}");
        let nested = directory.join("existing-directory");
        fs::create_dir(&nested).unwrap();
        assert!(write_text(&nested, b"may not replace a directory").is_err());
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        fs::remove_dir(nested).unwrap();
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
