use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};

type TokenHash = [u8; 32];

pub(crate) fn default_path() -> io::Result<PathBuf> {
    let root = env::var_os("LOCALAPPDATA")
        .filter(|root| !root.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is not set"))?;
    Ok(PathBuf::from(root).join("TouchCursor").join("trusted-devices.txt"))
}

pub(crate) struct TrustStore {
    path: PathBuf,
    hashes: Mutex<Option<BTreeSet<TokenHash>>>,
    count: AtomicUsize,
}

impl TrustStore {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path, hashes: Mutex::new(None), count: AtomicUsize::new(0) }
    }

    pub(crate) fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    // Call only on the server's blocking pool. The same mutex serializes loads,
    // pairing commits and revocation, including work from a retiring runtime.
    pub(crate) fn load(&self) -> io::Result<usize> {
        let mut current = self.lock()?;
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error),
        };
        let mut loaded = BTreeSet::new();
        for line in text.lines() {
            let hash = decode_hex(line).ok_or_else(invalid_store)?;
            if !loaded.insert(hash) {
                return Err(invalid_store());
            }
        }
        let count = loaded.len();
        *current = Some(loaded);
        self.count.store(count, Ordering::Release);
        Ok(count)
    }

    pub(crate) fn contains(&self, token: &str) -> io::Result<bool> {
        let Some(bytes) = decode_hex(token) else { return Ok(false) };
        let hash: TokenHash = Sha256::digest(bytes).into();
        let current = self.lock()?;
        Ok(current.as_ref().ok_or_else(not_loaded)?.contains(&hash))
    }

    pub(crate) fn issue_token(&self, may_issue: impl FnOnce() -> bool) -> io::Result<Option<String>> {
        let mut current = self.lock()?;
        // A queued grant from an old generation must not run after a reset
        // that acquired this mutex first.
        if !may_issue() {
            return Ok(None);
        }
        let hashes = current.as_mut().ok_or_else(not_loaded)?;
        let (token, hash) = loop {
            let mut token = [0; 32];
            OsRng.try_fill_bytes(&mut token).map_err(|_| {
                io::Error::other("Could not generate a secure device credential")
            })?;
            let hash: TokenHash = Sha256::digest(token).into();
            if !hashes.contains(&hash) {
                break (token, hash);
            }
        };
        hashes.insert(hash);
        if let Err(error) = self.persist(hashes) {
            hashes.remove(&hash);
            return Err(error);
        }
        self.count.store(hashes.len(), Ordering::Release);
        Ok(Some(String::from_utf8(hex(&token).to_vec()).expect("hex is ASCII")))
    }

    pub(crate) fn forget_all(&self) -> io::Result<()> {
        let mut current = self.lock()?;
        let empty = BTreeSet::new();
        self.persist(&empty)?;
        *current = Some(empty);
        self.count.store(0, Ordering::Release);
        Ok(())
    }

    fn lock(&self) -> io::Result<std::sync::MutexGuard<'_, Option<BTreeSet<TokenHash>>>> {
        self.hashes.lock().map_err(|_| io::Error::other("The trusted-device registry is unavailable"))
    }

    fn persist(&self, hashes: &BTreeSet<TokenHash>) -> io::Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "The trusted-device path has no parent")
        })?;
        fs::create_dir_all(parent)?;
        let (temporary, mut file) = loop {
            let temporary = self.path.with_extension(format!("{:016x}.tmp", OsRng.next_u64()));
            match fs::OpenOptions::new().write(true).create_new(true).open(&temporary) {
                Ok(file) => break (temporary, file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        let result = (|| {
            for hash in hashes {
                file.write_all(&hex(hash))?;
                file.write_all(b"\n")?;
            }
            file.sync_all()?;
            drop(file);
            replace_file(&temporary, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn invalid_store() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "The trusted-device file is corrupt")
}

fn not_loaded() -> io::Error {
    io::Error::other("The trusted-device registry has not been loaded")
}

fn decode_hex(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut decoded = [0; 32];
    for (output, pair) in decoded.iter_mut().zip(bytes.chunks_exact(2)) {
        let digit = |byte| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        *output = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(decoded)
}

fn hex(bytes: &[u8; 32]) -> [u8; 64] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = [0; 64];
    for (index, byte) in bytes.iter().enumerate() {
        text[index * 2] = DIGITS[usize::from(byte >> 4)];
        text[index * 2 + 1] = DIGITS[usize::from(byte & 15)];
    }
    text
}

#[cfg(windows)]
fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    let temporary: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(temporary.as_ptr(), destination.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)?;
    fs::File::open(destination.parent().expect("validated parent"))?.sync_all()
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::atomic::AtomicU64;

    pub(crate) struct TestDirectory(PathBuf);

    impl TestDirectory {
        pub(crate) fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            loop {
                let path = env::temp_dir().join(format!(
                    "touchcursor-trust-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create isolated test directory: {error}"),
                }
            }
        }

        pub(crate) fn store_path(&self) -> PathBuf {
            self.0.join("trusted-devices.txt")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::TestDirectory, *};
    use std::{sync::Arc, thread};

    #[test]
    fn committed_credentials_survive_reload_without_storing_bearer_tokens() {
        let directory = TestDirectory::new();
        let store = TrustStore::new(directory.store_path());
        assert_eq!(store.load().unwrap(), 0);
        let token = store.issue_token(|| true).unwrap().unwrap();
        assert_eq!(token.len(), 64);
        assert!(store.contains(&token).unwrap());
        let disk = fs::read_to_string(directory.store_path()).unwrap();
        assert!(!disk.contains(&token));
        assert_eq!(disk.lines().count(), 1);
        let restarted = TrustStore::new(directory.store_path());
        assert_eq!(restarted.load().unwrap(), 1);
        assert!(restarted.contains(&token).unwrap());
        for bad in ["".to_string(), "0".repeat(64), "A".repeat(64), format!("{token}0")] {
            assert!(!restarted.contains(&bad).unwrap());
        }
        restarted.forget_all().unwrap();
        assert!(!restarted.contains(&token).unwrap());
        let after_revoke = TrustStore::new(directory.store_path());
        assert_eq!(after_revoke.load().unwrap(), 0);
        assert!(!after_revoke.contains(&token).unwrap());
    }

    #[test]
    fn concurrent_pairings_commit_every_hash() {
        let directory = TestDirectory::new();
        let store = Arc::new(TrustStore::new(directory.store_path()));
        store.load().unwrap();
        let workers: Vec<_> = (0..8).map(|_| {
            let store = store.clone();
            thread::spawn(move || store.issue_token(|| true).unwrap().unwrap())
        }).collect();
        let tokens: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
        let restarted = TrustStore::new(directory.store_path());
        assert_eq!(restarted.load().unwrap(), tokens.len());
        for token in tokens {
            assert!(restarted.contains(&token).unwrap());
        }
        let disk = fs::read_to_string(directory.store_path()).unwrap();
        let lines: Vec<_> = disk.lines().collect();
        assert!(lines.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn queued_pairing_cannot_restore_credentials_after_revocation() {
        use std::sync::{atomic::AtomicU64, mpsc};
        let directory = TestDirectory::new();
        let store = Arc::new(TrustStore::new(directory.store_path()));
        store.load().unwrap();
        let token = store.issue_token(|| true).unwrap().unwrap();
        let epoch = Arc::new(AtomicU64::new(0));
        let locked = store.lock().unwrap();
        let (started, waiting) = mpsc::channel();
        let pending = {
            let store = store.clone();
            let epoch = epoch.clone();
            thread::spawn(move || {
                started.send(()).unwrap();
                store.issue_token(|| epoch.load(Ordering::Acquire) == 0).unwrap()
            })
        };
        waiting.recv().unwrap();
        epoch.store(1, Ordering::Release);
        drop(locked);
        store.forget_all().unwrap();
        assert!(pending.join().unwrap().is_none());
        let restarted = TrustStore::new(directory.store_path());
        assert_eq!(restarted.load().unwrap(), 0);
        assert!(!restarted.contains(&token).unwrap());
    }

    #[test]
    fn corrupt_and_unreadable_stores_fail_closed_but_explicit_reset_can_recover_corruption() {
        let directory = TestDirectory::new();
        fs::write(directory.store_path(), b"not a credential hash\n").unwrap();
        let store = TrustStore::new(directory.store_path());
        assert_eq!(store.load().unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(store.issue_token(|| true).is_err());
        assert_eq!(fs::read_to_string(directory.store_path()).unwrap(), "not a credential hash\n");
        store.forget_all().unwrap();
        assert_eq!(store.load().unwrap(), 0);
        fs::remove_file(directory.store_path()).unwrap();
        fs::create_dir(directory.store_path()).unwrap();
        assert!(store.load().is_err());
        assert!(store.forget_all().is_err());
    }

    #[test]
    fn failed_replace_does_not_register_a_credential() {
        let directory = TestDirectory::new();
        let store = TrustStore::new(directory.store_path());
        store.load().unwrap();
        fs::create_dir(directory.store_path()).unwrap();
        assert!(store.issue_token(|| true).is_err());
        assert_eq!(store.count(), 0);
        fs::remove_dir(directory.store_path()).unwrap();
        assert_eq!(store.load().unwrap(), 0);
    }
}
