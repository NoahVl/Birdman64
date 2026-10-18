//! Moving the save between devices (Settings: "Export save…" / "Import
//! save…"). The save is one raw `pw64.eep` file, byte-identical on every
//! platform and in emulators, so a transfer is a file copy; what differs is
//! how the player reaches a file: a native dialog on desktop (rfd, on its own
//! thread so the overlay keeps drawing), the system file picker on Android
//! (android/save_picker.rs), where the app's files are private.
//!
//! [`start`] returns at once; the overlay asks [`poll`] each frame. An import
//! replaces the save on disk and freezes the running game's writes
//! (`pw64_platform::headless::import_eep`): the caller restarts afterwards.

use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Export,
    Import,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(target_os = "ios", allow(dead_code))] // iOS only ever fails
pub enum Outcome {
    /// The picker was dismissed: nothing changed.
    Cancelled,
    Exported,
    /// The save on disk was replaced; the game must restart to load it.
    Imported,
    Failed(String),
}

/// A transfer is under way (a dialog is up): no second one starts.
static BUSY: Mutex<Option<Kind>> = Mutex::new(None);

/// A finished desktop dialog's result, until [`poll`] takes it.
#[cfg(not(target_os = "android"))]
static DONE: Mutex<Option<Outcome>> = Mutex::new(None);

/// The most a save file read accepts: real ones are 2 KB, so anything big is
/// the wrong file and is not read into memory.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const MAX_IMPORT_BYTES: u64 = 64 * 1024;

pub fn busy() -> bool {
    BUSY.lock().unwrap().is_some()
}

/// Opens the file dialog / picker for `kind` and returns at once.
pub fn start(kind: Kind) {
    {
        let mut busy = BUSY.lock().unwrap();
        if busy.is_some() {
            return;
        }
        *busy = Some(kind);
    }
    #[cfg(target_os = "android")]
    match kind {
        Kind::Export => crate::android::save_picker::start_export(),
        Kind::Import => crate::android::save_picker::start_import(),
    }
    #[cfg(not(target_os = "android"))]
    std::thread::spawn(move || {
        let outcome = desktop_dialog(kind);
        *DONE.lock().unwrap() = Some(outcome);
    });
}

/// The finished transfer's outcome, once (`None`: still open or idle).
pub fn poll() -> Option<Outcome> {
    #[cfg(target_os = "android")]
    let outcome = {
        let kind = (*BUSY.lock().unwrap())?;
        let (_, uri) = crate::android::save_picker::take_picked()?;
        Some(match (kind, uri) {
            (_, None) => Outcome::Cancelled,
            (Kind::Export, Some(uri)) => export_bytes()
                .and_then(|b| crate::android::save_picker::write_document(&uri, &b))
                .map_or_else(Outcome::Failed, |()| Outcome::Exported),
            (Kind::Import, Some(uri)) => crate::android::rom_picker::read_picked_bytes(&uri)
                .and_then(|b| import_bytes(&b))
                .map_or_else(Outcome::Failed, |()| Outcome::Imported),
        })
    };
    #[cfg(not(target_os = "android"))]
    let outcome = DONE.lock().unwrap().take();
    if outcome.is_some() {
        *BUSY.lock().unwrap() = None;
    }
    outcome
}

#[cfg_attr(target_os = "ios", allow(dead_code))]
fn export_bytes() -> Result<Vec<u8>, String> {
    pw64_platform::headless::export_eep()
}

#[cfg_attr(target_os = "ios", allow(dead_code))]
fn import_bytes(bytes: &[u8]) -> Result<(), String> {
    pw64_platform::headless::import_eep(bytes)
}

/// iOS has no file dialog (settings hides both rows): the save is
/// `pw64.eep` in the Files app's Birdman64 folder.
#[cfg(target_os = "ios")]
fn desktop_dialog(_kind: Kind) -> Outcome {
    Outcome::Failed("Copy pw64.eep in the Files app (Birdman64 folder) instead.".into())
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn desktop_dialog(kind: Kind) -> Outcome {
    let dialog = rfd::FileDialog::new().add_filter("Pilotwings 64 save", &["eep"]);
    match kind {
        Kind::Export => {
            let Some(path) = dialog
                .set_title("Export save")
                .set_file_name("pw64.eep")
                .save_file()
            else {
                return Outcome::Cancelled;
            };
            export_to(&path).map_or_else(Outcome::Failed, |()| Outcome::Exported)
        }
        Kind::Import => {
            let Some(path) = dialog
                .add_filter("All files", &["*"])
                .set_title("Import save")
                .pick_file()
            else {
                return Outcome::Cancelled;
            };
            import_from(&path).map_or_else(Outcome::Failed, |()| Outcome::Imported)
        }
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn export_to(path: &std::path::Path) -> Result<(), String> {
    let bytes = export_bytes()?;
    std::fs::write(path, bytes).map_err(|e| format!("Couldn't write {}: {e}", path.display()))
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn import_from(path: &std::path::Path) -> Result<(), String> {
    let read = |e: std::io::Error| format!("Couldn't read {}: {e}", path.display());
    let len = std::fs::metadata(path).map_err(read)?.len();
    if len > MAX_IMPORT_BYTES {
        return Err(format!("That file is {len} bytes; a save is 2048 bytes."));
    }
    import_bytes(&std::fs::read(path).map_err(read)?)
}

#[cfg(test)]
mod tests {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn import_rejects_a_big_file_without_reading_it() {
        let p = std::env::temp_dir().join("pw64-save-transfer-big.bin");
        std::fs::write(&p, vec![1u8; (super::MAX_IMPORT_BYTES + 1) as usize]).unwrap();
        let e = super::import_from(&p).unwrap_err();
        assert!(e.contains("2048"), "{e}");
        std::fs::remove_file(&p).unwrap();
    }
}
