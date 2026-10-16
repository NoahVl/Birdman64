//! Android stand-in for the parts of `rfd` the app uses (rfd has no Android
//! backend). Message boxes go to the log (stderr → logcat, android.rs) and
//! answer like a cancelled box. No file picker (`rom_setup::pick_rom` says
//! "no dialog"), so the ROM comes from the app's `rom/` folders or the
//! in-window ROM screen.
//! TODO(android): real dialogs and the Storage Access Framework picker
//! (`ACTION_OPEN_DOCUMENT` through JNI), docs/notes/android.md.

#[derive(Clone, Copy, Debug)]
pub enum MessageLevel {
    Info,
    Error,
}

#[derive(Clone, Copy, Debug)]
pub enum MessageButtons {
    OkCancel,
    YesNo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // rfd's full set; `show` never answers Yes
pub enum MessageDialogResult {
    Ok,
    Cancel,
    Yes,
    No,
}

#[derive(Default)]
pub struct MessageDialog {
    title: String,
    description: String,
    buttons: Option<MessageButtons>,
}

impl MessageDialog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_level(self, _level: MessageLevel) -> Self {
        self
    }

    pub fn set_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub fn set_description(mut self, text: impl Into<String>) -> Self {
        self.description = text.into();
        self
    }

    pub fn set_buttons(mut self, buttons: MessageButtons) -> Self {
        self.buttons = Some(buttons);
        self
    }

    /// Logs the box; the answer is the negative one (nobody saw it).
    pub fn show(self) -> MessageDialogResult {
        eprintln!("[dialog] {}: {}", self.title, self.description);
        match self.buttons {
            Some(MessageButtons::YesNo) => MessageDialogResult::No,
            Some(MessageButtons::OkCancel) => MessageDialogResult::Cancel,
            None => MessageDialogResult::Ok,
        }
    }
}
