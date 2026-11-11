//! Bridge app tokens (bridge roadmap step 1.8) on Linux: one owner-only file,
//! `$XDG_STATE_HOME/vectorcable/tokens` (or `~/.local/state/vectorcable/tokens`),
//! a JSON object from each bridge's `device_id` to its token. The PIN itself is
//! never stored.
//!
//! When the file cannot be read or written, the token lives only in memory and
//! the PIN is asked for on the next run, as before there was a store.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use vbcore::pin::TokenStore;

/// Far more than any number of bridges' tokens; a bigger file is not ours.
const MAX_BYTES: u64 = 64 * 1024;

/// Where the tokens are kept: under `$XDG_STATE_HOME` (ignored unless absolute,
/// as the XDG spec says), else `~/.local/state`. A state folder, not
/// `~/.config`, so it is less likely to be copied by dotfile sync.
pub fn path() -> Option<PathBuf> {
    path_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn path_from(state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute = |v: OsString| Some(PathBuf::from(v)).filter(|p| p.is_absolute());
    let base = state_home.and_then(absolute).or_else(|| {
        home.and_then(absolute)
            .map(|h| h.join(".local").join("state"))
    })?;
    Some(base.join("vectorcable").join("tokens"))
}

#[derive(Debug)]
pub struct TokenFile {
    path: PathBuf,
    /// One read-modify-write at a time within this process.
    lock: Mutex<()>,
}

impl TokenFile {
    pub fn new(path: PathBuf) -> TokenFile {
        TokenFile {
            path,
            lock: Mutex::new(()),
        }
    }

    /// The saved tokens. No file is an empty map, and so is one that does not
    /// parse or is too big (a save then replaces it); any other read error, or
    /// something other than a plain file at the path, is an error.
    fn read(&self) -> io::Result<BTreeMap<String, String>> {
        // Non-blocking, so a FIFO at the path cannot hang the start; a plain
        // file ignores the flag.
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(e),
        };
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("not a plain file"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Ok(BTreeMap::new());
        }
        Ok(serde_json::from_slice(&bytes).unwrap_or_default())
    }

    /// Replaces the file with `map`: written to an owner-only temporary file in
    /// the same folder, then renamed over it, so a crash leaves the old file or
    /// the new one, never half of one. An empty map removes the file.
    fn write(&self, map: &BTreeMap<String, String>) -> io::Result<()> {
        if map.is_empty() {
            return match fs::remove_file(&self.path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let dir = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("no folder for the token file"))?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // Only a folder of this user's, kept owner-only even if something
        // loosened it; anywhere else (a shared `$XDG_STATE_HOME`) the token is
        // not written, and the PIN is asked for next run.
        // SAFETY: geteuid has no preconditions and cannot fail.
        if fs::metadata(dir)?.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::other("the folder belongs to another user"));
        }
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let tmp = dir.join(format!(".tokens.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let result = write_new(&tmp, map).and_then(|_| fs::rename(&tmp, &self.path));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    fn describe(&self, e: io::Error) -> String {
        format!("{}: {e}", shown(&self.path))
    }
}

fn write_new(tmp: &Path, map: &BTreeMap<String, String>) -> io::Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)?;
    let text = serde_json::to_string(map).map_err(io::Error::other)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()
}

/// The path with the home folder as `~`, for messages.
fn shown(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

impl TokenStore for TokenFile {
    fn load(&self, device_id: &str) -> Option<String> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        self.read().ok()?.remove(device_id)
    }

    fn save(&self, device_id: &str, token: &str) -> Result<(), String> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut map = self.read().map_err(|e| self.describe(e))?;
        map.insert(device_id.to_string(), token.to_string());
        self.write(&map).map_err(|e| self.describe(e))
    }

    fn remove(&self, device_id: &str) {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        if let Ok(mut map) = self.read() {
            if map.remove(device_id).is_some() {
                let _ = self.write(&map);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder under the system temp folder, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "vectorcable-tokenfile-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn store(&self) -> TokenFile {
            TokenFile::new(self.0.join("state").join("vectorcable").join("tokens"))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_token_survives_into_a_new_store_on_the_same_file() {
        let s = Scratch::new("roundtrip");
        s.store().save("aaaa1111bbbb2222", "tok-1").unwrap();
        assert_eq!(s.store().load("aaaa1111bbbb2222").as_deref(), Some("tok-1"));
        assert_eq!(s.store().load("cccc3333dddd4444"), None);
    }

    #[test]
    fn each_bridge_keeps_its_own_token_and_a_new_one_replaces_the_old() {
        let s = Scratch::new("bridges");
        let t = s.store();
        t.save("bridge-a", "a1").unwrap();
        t.save("bridge-b", "b1").unwrap();
        t.save("bridge-a", "a2").unwrap();
        assert_eq!(t.load("bridge-a").as_deref(), Some("a2"));
        assert_eq!(t.load("bridge-b").as_deref(), Some("b1"));
    }

    #[test]
    fn removing_one_bridge_keeps_the_other_and_the_last_removes_the_file() {
        let s = Scratch::new("remove");
        let t = s.store();
        t.save("bridge-a", "a1").unwrap();
        t.save("bridge-b", "b1").unwrap();
        t.remove("bridge-a");
        assert_eq!(t.load("bridge-a"), None);
        assert_eq!(t.load("bridge-b").as_deref(), Some("b1"));
        t.remove("bridge-b");
        assert!(!t.path.exists());
        t.remove("bridge-b");
    }

    #[test]
    fn the_file_and_its_folder_are_owner_only() {
        let s = Scratch::new("modes");
        let t = s.store();
        t.save("bridge-a", "a1").unwrap();
        assert_eq!(mode(&t.path), 0o600);
        assert_eq!(mode(t.path.parent().unwrap()), 0o700);
        // No temporary file is left beside it.
        let names: Vec<_> = fs::read_dir(t.path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("tokens")]);
    }

    #[test]
    fn a_damaged_file_reads_as_no_token_and_the_next_save_replaces_it() {
        let s = Scratch::new("damaged");
        let t = s.store();
        fs::create_dir_all(t.path.parent().unwrap()).unwrap();
        for junk in [&b"{not json"[..], &[0xff, 0xfe, 0x00][..]] {
            fs::write(&t.path, junk).unwrap();
            assert_eq!(t.load("bridge-a"), None);
            t.save("bridge-a", "a1").unwrap();
            assert_eq!(t.load("bridge-a").as_deref(), Some("a1"));
        }
    }

    #[test]
    fn a_fifo_at_the_path_is_no_token_and_does_not_hang() {
        let s = Scratch::new("fifo");
        let t = s.store();
        fs::create_dir_all(t.path.parent().unwrap()).unwrap();
        let c = std::ffi::CString::new(t.path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert_eq!(t.load("bridge-a"), None);
        assert!(t.save("bridge-a", "a1").is_err());
    }

    /// The fallback Phil asked for: a store that cannot write says why, and
    /// the caller keeps the token in memory and asks for the PIN next run.
    #[test]
    fn a_folder_that_cannot_be_made_is_an_error_naming_the_file() {
        let s = Scratch::new("blocked");
        // A plain file where the folder should be.
        fs::write(s.0.join("state"), "").unwrap();
        let t = s.store();
        let err = t.save("bridge-a", "a1").unwrap_err();
        assert!(err.contains("tokens"), "{err}");
        assert_eq!(t.load("bridge-a"), None);
    }

    #[test]
    fn the_path_follows_xdg_state_home_only_when_it_is_absolute() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            path_from(os("/x/state"), os("/home/u")),
            Some(PathBuf::from("/x/state/vectorcable/tokens"))
        );
        assert_eq!(
            path_from(os("relative/state"), os("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/vectorcable/tokens"))
        );
        assert_eq!(
            path_from(None, os("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/vectorcable/tokens"))
        );
        // No usable home: no store, so the PIN is asked for each run.
        assert_eq!(path_from(None, None), None);
        assert_eq!(path_from(None, os("relative")), None);
    }
}
