//! iOS stand-in for the parts of `rfd` the app uses (rfd has no iOS
//! backend). Message boxes go to the log (stderr → the unified log, seen in
//! Console and Xcode) and answer like a cancelled box. There is no file
//! dialog here either (`rom_setup::pick_rom` says "no dialog"): the ROM is
//! found by scanning the Documents folder (paths.rs).

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
