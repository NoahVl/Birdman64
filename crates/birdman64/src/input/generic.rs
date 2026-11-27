//! Generic controllers on gilrs (docs/notes/input.md "Generic controllers"):
//! the user SDL mapping file, the per-model custom setups (`raw.rs`) applied
//! to live pads, the raw capture behind Settings > Controls > Controller
//! setup, and that page's diagnostics (which pads are recognised, their
//! GUIDs, the live raw buttons / axes / hats).

use super::Pad;
use super::raw::{self, Profile, RawEvent, RawInput, RawState, Want};
use super::sdl_map::{self, UserMappings};
use gilrs::{Axis, Button, Gilrs};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// The user mapping file in the data folder (SDL2 format, one per line).
pub const MAPPINGS_FILE: &str = "gamecontrollerdb.txt";

/// A pad's GUID as gilrs builds it (SDL layout without the name CRC), lower
/// case hex: what `[input.devices]` and `gamecontrollerdb.txt` key on.
pub fn guid(g: &gilrs::Gamepad) -> String {
    g.uuid().iter().map(|b| format!("{b:02x}")).collect()
}

/// Windows.Gaming.Input gamepads (Xbox-class) all report the nil GUID:
/// they're fully supported, and a setup keyed on it would hit every one.
fn settable(guid: &str) -> bool {
    guid.bytes().any(|b| b != b'0')
}

// ---------------------------------------------------------------------------
// User SDL mappings

/// What was loaded at startup, for the setup page and the log.
#[derive(Debug, Clone, Default)]
pub struct MappingsInfo {
    pub path: std::path::PathBuf,
    /// The file exists (else the page says where to put one).
    pub file_found: bool,
    pub loaded: Vec<(String, String)>,
    pub notes: Vec<String>,
}

static MAPPINGS: Mutex<Option<MappingsInfo>> = Mutex::new(None);

/// The user mappings loaded at startup (`None` before the input thread ran).
pub fn mappings_info() -> Option<MappingsInfo> {
    MAPPINGS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// gilrs with the user's mappings: `gamecontrollerdb.txt` from the data
/// folder, then `SDL_GAMECONTROLLERCONFIG` (both normalised by `sdl_map`;
/// gilrs's own reading of the variable is turned off for that). gilrs adds
/// its bundled DB after these, so for a GUID the bundled DB knows, its
/// mapping wins: a custom setup is the way to override one.
// gilrs's own error type (as `Gilrs::new`, which allows the same lint).
#[allow(clippy::result_large_err)]
pub fn build_gilrs() -> Result<Gilrs, gilrs::Error> {
    let path = crate::paths::data_dir().join(MAPPINGS_FILE);
    let mut all = UserMappings::default();
    let file_found = match std::fs::read_to_string(&path) {
        Ok(text) => {
            all = sdl_map::parse(&text, MAPPINGS_FILE, sdl_map::PLATFORM);
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            all.notes
                .push(format!("{MAPPINGS_FILE}: can't read it ({e})"));
            true
        }
    };
    if let Ok(env) = std::env::var("SDL_GAMECONTROLLERCONFIG") {
        let e = sdl_map::parse(&env, "SDL_GAMECONTROLLERCONFIG", sdl_map::PLATFORM);
        all.text.push_str(&e.text);
        all.loaded.extend(e.loaded);
        all.notes.extend(e.notes);
    }
    for note in &all.notes {
        eprintln!("[input] {note}");
    }
    if !all.loaded.is_empty() {
        eprintln!(
            "[input] {} user controller mapping(s) loaded",
            all.loaded.len()
        );
    }
    let mut b = gilrs::GilrsBuilder::new().add_env_mappings(false);
    if !all.text.is_empty() {
        b = b.add_mappings(&all.text);
    }
    *MAPPINGS.lock().unwrap_or_else(PoisonError::into_inner) = Some(MappingsInfo {
        path,
        file_found,
        loaded: all.loaded,
        notes: all.notes,
    });
    b.build()
}

// ---------------------------------------------------------------------------
// Custom setups (`[input.devices]`)

type Profiles = Arc<BTreeMap<String, Profile>>;

static PROFILES: Mutex<Option<Profiles>> = Mutex::new(None);

/// The custom setups by GUID (config on first use, then live edits).
pub fn profiles() -> Profiles {
    PROFILES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(|| {
            let mut out = BTreeMap::new();
            for (guid, map) in &crate::config::get().input.devices {
                if !settable(guid) {
                    eprintln!(
                        "[config] [input.devices.\"{guid}\"]: the all-zero GUID is every \
                         Xbox-class pad on Windows; ignored"
                    );
                    continue;
                }
                let mut problems = Vec::new();
                let p = Profile::from_map(map, &mut problems);
                for e in problems {
                    eprintln!("[config] [input.devices.\"{guid}\"] {e}; ignored");
                }
                out.insert(guid.clone(), p);
            }
            Arc::new(out)
        })
        .clone()
}

/// Applies a setup to every connected pad of that model at once and saves
/// it (`None`: remove it). The live change stays even if the save fails;
/// the error is for the page to show.
pub fn set_profile(guid: &str, profile: Option<Profile>) -> Result<(), String> {
    let map = profile.as_ref().map(Profile::to_map);
    {
        let mut g = PROFILES.lock().unwrap_or_else(PoisonError::into_inner);
        let cur = g.get_or_insert_with(|| Arc::new(BTreeMap::new()));
        let mut next = (**cur).clone();
        match profile {
            Some(p) => next.insert(guid.to_string(), p),
            None => next.remove(guid),
        };
        *cur = Arc::new(next);
    }
    crate::config::save_device(guid, map.as_ref())
}

/// gilrs state by code spelling, collected once per poll for a pad with a
/// setup (a handful of entries).
pub struct GilrsRaw {
    buttons: Vec<String>,
    axes: Vec<(String, f32)>,
}

impl GilrsRaw {
    pub fn new(g: &gilrs::Gamepad) -> Self {
        let state = g.state();
        Self {
            buttons: state
                .buttons()
                .filter(|(_, d)| d.is_pressed())
                .map(|(c, _)| c.to_string())
                .collect(),
            axes: state
                .axes()
                .map(|(c, d)| (c.to_string(), d.value()))
                .collect(),
        }
    }
}

impl RawState for GilrsRaw {
    fn button(&self, code: &str) -> bool {
        self.buttons.iter().any(|b| b == code)
    }
    fn axis(&self, code: &str) -> f32 {
        self.axes
            .iter()
            .find(|(c, _)| c == code)
            .map_or(0.0, |&(_, v)| v)
    }
}

/// The pad's setup, if its model has one.
pub fn profile_for<'a>(
    profiles: &'a BTreeMap<String, Profile>,
    g: &gilrs::Gamepad,
) -> Option<&'a Profile> {
    if profiles.is_empty() {
        return None;
    }
    profiles.get(&guid(g))
}

// ---------------------------------------------------------------------------
// Raw capture (the guided setup)

/// What a capture step got.
#[derive(Debug, Clone, PartialEq)]
pub struct Captured {
    pub guid: String,
    pub name: String,
    pub input: RawInput,
}

struct Capture {
    want: Want,
    /// Only this model counts (the pad the setup started on).
    guid: Option<String>,
    /// Axis values when the capture armed, per (pad, code): set by the input
    /// thread on its first poll after arming.
    rest: Option<HashMap<(usize, String), f32>>,
    got: Option<Captured>,
    /// A press came from a pad the setup can't key on (nil GUID).
    refused: bool,
}

static CAPTURE: Mutex<Option<Capture>> = Mutex::new(None);
static ARMED: AtomicBool = AtomicBool::new(false);

/// Arms a raw capture for one setup step. `guid`: only that model counts.
pub fn start_capture(want: Want, guid: Option<String>) {
    *CAPTURE.lock().unwrap_or_else(PoisonError::into_inner) = Some(Capture {
        want,
        guid,
        rest: None,
        got: None,
        refused: false,
    });
    ARMED.store(true, Ordering::Relaxed);
}

/// The captured input (taken once; the capture disarms when it stores one).
pub fn take_capture() -> Option<Captured> {
    let mut g = CAPTURE.lock().unwrap_or_else(PoisonError::into_inner);
    let got = g.as_mut().and_then(|c| c.got.take());
    if got.is_some() {
        *g = None;
    }
    got
}

/// A press arrived from a pad that needs no setup (an Xbox-class gamepad on
/// Windows): taken once, for the page to say so.
pub fn take_refused() -> bool {
    let mut g = CAPTURE.lock().unwrap_or_else(PoisonError::into_inner);
    g.as_mut().is_some_and(|c| std::mem::take(&mut c.refused))
}

pub fn cancel_capture() {
    ARMED.store(false, Ordering::Relaxed);
    *CAPTURE.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

/// A capture is waiting (the input thread holds back the settings trigger:
/// the setup may be capturing that very button).
pub fn capturing() -> bool {
    ARMED.load(Ordering::Relaxed)
}

/// Input thread, once per poll before the events: the first poll after
/// arming records where every axis rests.
pub fn capture_poll(gilrs: &Gilrs) {
    if !capturing() {
        return;
    }
    let mut g = CAPTURE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(c) = g.as_mut().filter(|c| c.rest.is_none()) {
        let mut rest = HashMap::new();
        for (id, pad) in gilrs.gamepads() {
            for (code, d) in pad.state().axes() {
                rest.insert((usize::from(id), code.to_string()), d.value());
            }
        }
        c.rest = Some(rest);
    }
}

/// Input thread, per gilrs event while a capture is armed. `true`: the event
/// was taken (it must not count as anything else).
pub fn capture_event(gilrs: &Gilrs, ev: &gilrs::Event) -> bool {
    if !capturing() {
        return false;
    }
    let raw = match ev.event {
        gilrs::EventType::ButtonPressed(_, code) => RawEvent::Pressed(code.to_string()),
        gilrs::EventType::AxisChanged(_, v, code) => RawEvent::Axis(code.to_string(), v),
        _ => return false,
    };
    let mut g = CAPTURE.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(c) = g.as_mut().filter(|c| c.got.is_none() && c.rest.is_some()) else {
        return false;
    };
    let pad = gilrs.gamepad(ev.id);
    let id = guid(&pad);
    if c.guid.as_ref().is_some_and(|want| *want != id) {
        return false;
    }
    let rest = match &raw {
        RawEvent::Axis(code, _) => c
            .rest
            .as_ref()
            .and_then(|r| r.get(&(usize::from(ev.id), code.clone())))
            .copied()
            .unwrap_or(0.0),
        RawEvent::Pressed(_) => 0.0,
    };
    let Some(input) = raw::capture(c.want, &raw, rest) else {
        return false;
    };
    if !settable(&id) {
        c.refused = true;
        return true;
    }
    c.got = Some(Captured {
        guid: id,
        name: pad.name().to_string(),
        input,
    });
    ARMED.store(false, Ordering::Relaxed);
    true
}

// ---------------------------------------------------------------------------
// Diagnostics (the setup page)

/// How gilrs sees a pad.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// An SDL mapping applies: its name, and whether it is one of the
    /// user's (else the bundled DB's).
    Mapped { name: String, user: bool },
    /// No SDL mapping, but the driver reports a standard gamepad layout.
    Standard,
    /// Nothing identifies its buttons: a custom setup (or a mapping) makes
    /// it usable.
    Unrecognised,
}

/// What a raw code is, for the readout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Button,
    Axis,
    Hat,
}

/// One live raw reading.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub code: String,
    pub kind: Kind,
    pub value: f32,
}

/// One connected pad on the setup page.
#[derive(Debug, Clone, PartialEq)]
pub struct PadDiag {
    pub name: String,
    pub guid: String,
    pub status: Status,
    /// Its model has a custom setup (used instead of the gamepad bindings).
    pub custom: bool,
    /// The setup can't key on it (nil GUID: an Xbox-class gamepad on
    /// Windows, fully supported anyway).
    pub settable: bool,
    /// Held buttons and every axis seen so far.
    pub readings: Vec<Reading>,
}

static DIAG_ON: AtomicBool = AtomicBool::new(false);
static DIAG: Mutex<Vec<PadDiag>> = Mutex::new(Vec::new());

/// The setup page is shown: the input thread publishes [`diagnostics`].
pub fn set_diagnostics(on: bool) {
    DIAG_ON.store(on, Ordering::Relaxed);
}

/// The connected pads as of the last poll (empty while the page is closed).
pub fn diagnostics() -> Vec<PadDiag> {
    DIAG.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// How gilrs sees `g` (the setup page and the controller log).
pub fn status(g: &gilrs::Gamepad) -> Status {
    match g.mapping_source() {
        gilrs::MappingSource::SdlMappings => {
            let name = g.map_name().unwrap_or(g.name()).to_string();
            let id = guid(g);
            let user = mappings_info()
                .is_some_and(|m| m.loaded.iter().any(|(gd, n)| *gd == id && *n == name));
            Status::Mapped { name, user }
        }
        // gilrs drops default-mapping entries the device lacks: no South
        // button means the driver gave no gamepad layout (a generic HID
        // joystick on Linux / a raw controller on Windows).
        _ if g.button_code(Button::South).is_some() => Status::Standard,
        _ => Status::Unrecognised,
    }
}

/// Is this code the pad's hat (gilrs's d-pad axes or d-pad buttons made
/// from them), or spelled like one?
fn is_hat(g: &gilrs::Gamepad, code: gilrs::ev::Code) -> bool {
    use gilrs::ev::AxisOrBtn;
    match g.axis_or_btn_name(code) {
        Some(AxisOrBtn::Axis(Axis::DPadX | Axis::DPadY)) => true,
        Some(AxisOrBtn::Btn(
            Button::DPadUp | Button::DPadDown | Button::DPadLeft | Button::DPadRight,
        )) => true,
        _ => raw::looks_like_hat(&code.to_string()),
    }
}

/// Input thread, once per poll: publishes the pads while the page is open.
pub fn diag_poll(gilrs: &Gilrs) {
    if !DIAG_ON.load(Ordering::Relaxed) {
        return;
    }
    let profiles = profiles();
    let pads = gilrs
        .gamepads()
        .filter(|(_, g)| g.is_connected())
        .map(|(_, g)| {
            let id = guid(&g);
            let state = g.state();
            let mut readings: Vec<Reading> = state
                .buttons()
                .filter(|(_, d)| d.is_pressed())
                .map(|(c, d)| Reading {
                    code: c.to_string(),
                    kind: if is_hat(&g, c) {
                        Kind::Hat
                    } else {
                        Kind::Button
                    },
                    value: d.value(),
                })
                .collect();
            readings.extend(state.axes().map(|(c, d)| Reading {
                code: c.to_string(),
                kind: if is_hat(&g, c) { Kind::Hat } else { Kind::Axis },
                value: d.value(),
            }));
            readings.sort_by(|a, b| a.code.cmp(&b.code));
            PadDiag {
                name: g.name().to_string(),
                status: status(&g),
                custom: profiles.contains_key(&id),
                settable: settable(&id),
                guid: id,
                readings,
            }
        })
        .collect();
    *DIAG.lock().unwrap_or_else(PoisonError::into_inner) = pads;
}

/// One connected pad, read once per poll: what it gives the game, the
/// settings overlay's navigation, and whether it holds the settings
/// trigger (its setup's SETTINGS input, else the configured pad button).
pub struct PadRead {
    pub game: Pad,
    pub nav: Pad,
    pub settings: bool,
}

/// Reads `g` through its model's custom setup if it has one, else through
/// the gamepad bindings.
pub fn read_pad(
    profiles: &BTreeMap<String, Profile>,
    g: &gilrs::Gamepad,
    settings_button: Button,
) -> PadRead {
    match profile_for(profiles, g) {
        Some(p) => {
            let raw = GilrsRaw::new(g);
            PadRead {
                game: p.pad(&raw),
                nav: p.nav(&raw),
                settings: p.settings(&raw),
            }
        }
        None => PadRead {
            game: super::gamepad_pad(g),
            nav: super::gamepad_nav_pad(g),
            settings: g.is_pressed(settings_button),
        },
    }
}

static UNRECOGNISED: AtomicBool = AtomicBool::new(false);

/// A connected pad is neither mapped nor set up (the Controls page points it
/// to Controller setup).
pub fn unrecognised_connected() -> bool {
    UNRECOGNISED.load(Ordering::Relaxed)
}

/// Input thread, once per poll: keeps [`unrecognised_connected`] current
/// (cheap: no strings unless a pad has a setup).
pub fn note_unrecognised(gilrs: &Gilrs, profiles: &BTreeMap<String, Profile>) {
    let any = gilrs.gamepads().any(|(_, g)| {
        g.is_connected()
            && g.mapping_source() != gilrs::MappingSource::SdlMappings
            && g.button_code(Button::South).is_none()
            && profile_for(profiles, &g).is_none()
            && settable(&guid(&g))
    });
    UNRECOGNISED.store(any, Ordering::Relaxed);
}

/// Tests: what the setup page shows without an input thread.
#[cfg(test)]
pub fn set_test_diagnostics(pads: Vec<PadDiag>) {
    *DIAG.lock().unwrap_or_else(PoisonError::into_inner) = pads;
}
