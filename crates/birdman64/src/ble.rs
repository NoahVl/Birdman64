//! Switch 2 pads (Joy-Con 2 L/R, Pro Controller 2) over Bluetooth LE.
//!
//! Uses `btleplug` (WinRT GATT backend on Windows); the pads are connected
//! unpaired, exactly the way the console does it — no pairing handshake, no
//! "Add a device". Note that Switch 2 pads do **not** speak HID-over-GATT
//! (service 0x1812 / report 0x2A4D): they expose a proprietary GATT service
//! with an input-report characteristic (notifications) and a command
//! characteristic (writes). The layout, UUIDs and handshake here follow the
//! MIT-licensed Linux bridge research (`trevlars/switch2-controllers-linux`,
//! which credits `Nadeflore/switch2-controllers` et al.) — see
//! docs/notes/input.md "Switch 2 pads over BLE".
//!
//! L and R Joy-Cons are merged into one virtual N64 pad (positional mapping,
//! like the gilrs gamepad defaults in `input.rs`); a Pro Controller 2 is one
//! pad on its own. The virtual pad is merged into controller 1 next to the
//! gilrs pads (its state is read by the input thread's poll loop).
//!
//! End-to-end unverified — see the "unverified" list in docs/notes/input.md.

use btleplug::api::{
    Central, CentralEvent, Characteristic, Manager as _, Peripheral as _, ScanFilter,
    ValueNotification, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures::{StreamExt, executor::block_on};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use crate::config;
use crate::input::{
    CONT_A, CONT_B, CONT_C_UP, CONT_DOWN, CONT_L, CONT_LEFT, CONT_R, CONT_RIGHT, CONT_START,
    CONT_UP, CONT_Z, Pad, c_stick_buttons, stick_to_n64,
};
use crate::opts;
use uuid::Uuid;

/// How often the scanner looks for new (or reconnecting) pads.
const SCAN_INTERVAL: Duration = Duration::from_secs(3);

// --------------------------------------------------------------------- //
// Switch 2 GATT protocol (see the module doc; offsets in 12-bit LE)      //
// --------------------------------------------------------------------- //

/// Input-report characteristic (notify): 63-byte reports.
const INPUT_REPORT_UUID: &str = "ab7de9be-89fe-49ad-828f-118f09df7fd2";
/// Command characteristic (write without response): 0x91-protocol commands.
const COMMAND_WRITE_UUID: &str = "649d4ac9-8eb7-4e6c-af44-1ea54fe5f005";
/// Command-response characteristic (notify): replies to written commands.
const COMMAND_RESPONSE_UUID: &str = "c765a961-d9d8-4d36-a20a-5315b111836a";

struct CharUuids {
    input: Uuid,
    command: Uuid,
    response: Uuid,
}

/// The three characteristic UUIDs, parsed once.
fn uuids() -> &'static CharUuids {
    static U: OnceLock<CharUuids> = OnceLock::new();
    U.get_or_init(|| CharUuids {
        input: Uuid::parse_str(INPUT_REPORT_UUID).unwrap(),
        command: Uuid::parse_str(COMMAND_WRITE_UUID).unwrap(),
        response: Uuid::parse_str(COMMAND_RESPONSE_UUID).unwrap(),
    })
}

// 0x91 command framing / subcommands (per the bridge research; only what the
// handshake needs — no rumble or calibration reads yet).
const CMD_FEATURE: u8 = 0x0c;
/// Player LEDs: data = 8 bytes, the first a bitmask of the 4 LEDs (bit 0 =
/// the first LED); same frame SDL's Switch 2 driver sends over USB.
const CMD_LED: u8 = 0x09;
const SUBCMD_LED_SET_PLAYER: u8 = 0x07;
const SUBCMD_FEATURE_INIT: u8 = 0x02;
const SUBCMD_FEATURE_ENABLE: u8 = 0x04;
/// Feature bits: 0x03 (unknown, always sent by the bridge) | motion.
const FEATURE_FLAGS: u8 = 0x03 | 0x04;

// Button bitmask (u32 LE, report bytes 4..8), named by the Nintendo labels
// (A east, B south, X north, Y west). Shared by Pro / Joy-Con / GC BLE
// reports. Bits we don't map: HOME, CAPTURE, C, GR/GL, stick-clicks.
const B_Y: u32 = 1 << 0;
const B_X: u32 = 1 << 1;
const B_B: u32 = 1 << 2;
const B_A: u32 = 1 << 3;
const B_SR_R: u32 = 1 << 4;
const B_SL_R: u32 = 1 << 5;
const B_R: u32 = 1 << 6;
const B_ZR: u32 = 1 << 7;
const B_MINUS: u32 = 1 << 8;
const B_PLUS: u32 = 1 << 9;
const B_HOME: u32 = 1 << 12;
const B_CAPTURE: u32 = 1 << 13;
/// Open/close the settings overlay (never reach the game): minus is Select
/// on a Nintendo pad (the gilrs settings default), Home/Capture are where a
/// Switch player looks for a system menu.
const SETTINGS_BUTTONS: u32 = B_MINUS | B_HOME | B_CAPTURE;
const B_DOWN: u32 = 1 << 16;
const B_UP: u32 = 1 << 17;
const B_RIGHT: u32 = 1 << 18;
const B_LEFT: u32 = 1 << 19;
const B_SR_L: u32 = 1 << 20;
const B_SL_L: u32 = 1 << 21;
const B_L: u32 = 1 << 22;
const B_ZL: u32 = 1 << 23;

/// Which half a device is: state slot and button mapping differ per side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    JoyConL,
    JoyConR,
    Pro,
}

// --------------------------------------------------------------------- //
// Report parsing (pure; unit-tested without hardware)                   //
// --------------------------------------------------------------------- //

/// One device's decoded state from an input report.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PadReport {
    /// Raw Switch 2 button bitmask (see `*_buttons` translations below).
    buttons: u32,
    /// Left stick, x right / y up, each -1..1.
    left: (f32, f32),
    /// Right stick, same axes.
    right: (f32, f32),
}

/// Decodes a raw input-report notification (63 bytes; 0x91 protocol layout:
/// u32 LE timestamp, u32 LE buttons, then two 3-byte packed sticks).
/// Reports shorter than the sticks are rejected.
pub fn parse_report(bytes: &[u8]) -> Option<PadReport> {
    if bytes.len() < 16 {
        return None;
    }
    let buttons = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    Some(PadReport {
        buttons,
        left: stick12(&bytes[10..13]),
        right: stick12(&bytes[13..16]),
    })
}

/// 3 packed bytes → two 12-bit axes (x low, y high), normalized to -1..1.
/// Raw y already grows upward (verified on a real Joy-Con 2, 2026-10-02),
/// which is what `input.rs` wants; SDL negates it only because SDL's y axis
/// points down.
/// This is the nominal full 12-bit range; real sticks stop far short of it,
/// so the report loop rescales with `StickCal`.
fn stick12(b: &[u8]) -> (f32, f32) {
    let (x, y) = raw12(b);
    let axis = |raw: u16| ((raw as f32 - 2048.0) / 2048.0).clamp(-1.0, 1.0);
    (axis(x), axis(y))
}

/// 3 packed bytes → the two raw 12-bit axes (x low, y high).
fn raw12(b: &[u8]) -> (u16, u16) {
    let v = b[0] as u32 | ((b[1] as u32) << 8) | ((b[2] as u32) << 16);
    ((v & 0xFFF) as u16, (v >> 12) as u16)
}

/// Raw stick center (12-bit midpoint).
const STICK_CENTER: u16 = 2048;
/// Smallest assumed travel from center: a real stick reaches past this, so
/// a full push always ends up as full deflection. Real Joy-Con 2 full pushes
/// measured 1112..1332 per direction (2026-10-02).
const STICK_MIN_TRAVEL: u16 = 1050;

/// Auto-ranging stick calibration, per pad connection. A real Joy-Con 2
/// stick stops far short of the 12-bit range, so with the nominal range a
/// full push read as ~60%: slow steering, and menus (which need 75%,
/// results.c) didn't move. Each axis direction scales to the furthest raw
/// value seen so far (at least `STICK_MIN_TRAVEL`), so after the first
/// full push in a direction it reaches 1.0. Stands in for reading the
/// pad's stored factory calibration over commands (not ported).
#[derive(Debug, Clone, Copy)]
struct StickCal {
    /// Per axis (lx, ly, rx, ry): furthest travel seen below / above center.
    neg: [u16; 4],
    pos: [u16; 4],
}

impl StickCal {
    fn new() -> Self {
        Self {
            neg: [STICK_MIN_TRAVEL; 4],
            pos: [STICK_MIN_TRAVEL; 4],
        }
    }

    /// Rescales a report's sticks (raw report bytes in, normalized axes into
    /// `rep`). Returns true when a range grew by a logging step.
    fn apply(&mut self, bytes: &[u8], rep: &mut PadReport) -> bool {
        let (lx, ly) = raw12(&bytes[10..13]);
        let (rx, ry) = raw12(&bytes[13..16]);
        let mut grew = false;
        let mut axis = |i: usize, raw: u16| {
            let (travel, sign) = if raw >= STICK_CENTER {
                (raw - STICK_CENTER, 1.0)
            } else {
                (STICK_CENTER - raw, -1.0)
            };
            let range = if sign > 0.0 {
                &mut self.pos[i]
            } else {
                &mut self.neg[i]
            };
            if travel > *range {
                grew |= travel / 100 > *range / 100;
                *range = travel;
            }
            sign * (travel as f32 / *range as f32).min(1.0)
        };
        rep.left = (axis(0, lx), axis(1, ly));
        rep.right = (axis(2, rx), axis(3, ry));
        grew
    }
}

// --------------------------------------------------------------------- //
// Virtual pad mapping (positional, mirrors input.rs gamepad defaults)   //
// --------------------------------------------------------------------- //

/// Switch 2 button mask → N64 bit, one table per device kind.
type ButtonMap = &'static [(u32, u16)];

/// Joy-Con L: arrow buttons → D-pad, L and the rail SL/SR → L, ZL → Z.
/// Minus is a settings button (`SETTINGS_BUTTONS`), not Start.
const JOYCON_L_MAP: ButtonMap = &[
    (B_L | B_SL_L | B_SR_L, CONT_L),
    (B_ZL, CONT_Z),
    (B_UP, CONT_UP),
    (B_DOWN, CONT_DOWN),
    (B_LEFT, CONT_LEFT),
    (B_RIGHT, CONT_RIGHT),
];

/// Joy-Con R (held upright, as half of a pair): A → A, B / Y → B, X (north)
/// → C-up, R / ZR and the rail SL/SR → R, plus → Start.
const JOYCON_R_MAP: ButtonMap = &[
    (B_A, CONT_A),
    (B_B | B_Y, CONT_B),
    (B_X, CONT_C_UP),
    (B_R | B_ZR | B_SL_R | B_SR_R, CONT_R),
    (B_PLUS, CONT_START),
];

/// Pro Controller 2: the Joy-Con R face table plus L → L, ZL → Z and the
/// D-pad; minus is unmapped.
const PRO_MAP: ButtonMap = &[
    (B_A, CONT_A),
    (B_B | B_Y, CONT_B),
    (B_X, CONT_C_UP),
    (B_L, CONT_L),
    (B_ZL, CONT_Z),
    (B_R | B_ZR, CONT_R),
    (B_PLUS, CONT_START),
    (B_UP, CONT_UP),
    (B_DOWN, CONT_DOWN),
    (B_LEFT, CONT_LEFT),
    (B_RIGHT, CONT_RIGHT),
];

fn map_buttons(b: u32, map: ButtonMap) -> u16 {
    map.iter()
        .filter(|&&(mask, _)| b & mask != 0)
        .fold(0, |acc, &(_, bit)| acc | bit)
}

/// Latest report per device kind (`Kind as usize`; `None` = not connected /
/// no report yet). Two pads of the same kind would share a slot.
type Slots = [Option<PadReport>; 3];

static STATE: Mutex<Slots> = Mutex::new([None, None, None]);

/// Merges all connected pads into one virtual N64 pad: the L Joy-Con's (or
/// Pro's) left stick is the main stick, the R Joy-Con's (or Pro's) right
/// stick drives the C buttons (same model as a two-stick gilrs gamepad),
/// buttons are OR'd and the larger main stick wins (`Pad::merge`). Sticks go
/// through `input::stick_to_n64` — the same radial-deadzone logic.
pub fn virtual_pad(state: &Slots) -> Pad {
    let mut pad = Pad::default();
    for (kind, rep) in [Kind::JoyConL, Kind::JoyConR, Kind::Pro].iter().zip(state) {
        let Some(rep) = rep else { continue };
        let (map, main_stick, c_stick) = match kind {
            Kind::JoyConL => (JOYCON_L_MAP, true, false),
            Kind::JoyConR => (JOYCON_R_MAP, false, true),
            Kind::Pro => (PRO_MAP, true, true),
        };
        let mut p = Pad {
            button: map_buttons(rep.buttons, map),
            ..Pad::default()
        };
        if main_stick {
            (p.stick_x, p.stick_y) = stick_to_n64(rep.left.0, rep.left.1);
        }
        if c_stick {
            p.button |= c_stick_buttons(rep.right.0, rep.right.1);
        }
        pad = pad.merge(p);
    }
    pad
}

/// Stores one device's latest report. While BLE is switched off a late
/// report from a pad thread that hasn't noticed yet is dropped, so the
/// virtual pad stays idle.
fn set_slot(kind: Kind, rep: Option<PadReport>) {
    if rep.is_some() && !ON.load(Ordering::Acquire) {
        return;
    }
    STATE.lock().unwrap()[kind as usize] = rep;
}

/// Latest merged virtual pad (idle until a pad has connected, and while BLE
/// is switched off). Read by the input thread's poll loop every 4 ms.
pub fn pad() -> Pad {
    if !ON.load(Ordering::Acquire) {
        return Pad::default();
    }
    virtual_pad(&STATE.lock().unwrap())
}

/// Any BLE pad connected (has sent a report)? U26: a gilrs pad leaving is
/// not "the last controller" while one is.
pub(crate) fn any_connected() -> bool {
    ON.load(Ordering::Acquire) && STATE.lock().unwrap().iter().any(Option::is_some)
}

/// A settings button (minus / Home / Capture) held on any BLE pad. The input
/// thread edge-detects it like the gilrs settings button.
pub(crate) fn settings_pressed() -> bool {
    ON.load(Ordering::Acquire)
        && STATE
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .any(|r| r.buttons & SETTINGS_BUTTONS != 0)
}

// --------------------------------------------------------------------- //
// Options: PW64_BLE / [input.ble] enabled / PW64_BLE_LIST               //
// --------------------------------------------------------------------- //

/// The built-in default (`[input.ble] enabled`): off. `config.rs` compares
/// saved values against it (U18: a default is removed, not written).
pub(crate) const DEFAULT: bool = false;

/// `PW64_BLE=<1|0>`, parsed once (the settings screen asks every frame; a
/// bad value warns once and falls through to the config).
fn env_setting() -> Option<bool> {
    static ENV: OnceLock<Option<bool>> = OnceLock::new();
    *ENV.get_or_init(
        || match std::env::var("PW64_BLE").as_deref().map(str::trim) {
            Ok("1" | "true" | "yes" | "on") => Some(true),
            Ok("0" | "false" | "no" | "off") => Some(false),
            Ok(other) => {
                eprintln!("[ble] PW64_BLE={other:?} not understood (use 1 or 0); ignoring");
                None
            }
            Err(_) => None,
        },
    )
}

/// `PW64_BLE=<1|0>` or `[input.ble] enabled`: BLE pads on/off at startup.
/// Default off. Same precedence as every option (opts::precedence): env >
/// config > default; a present-but-unparsable env value warns and falls
/// through. At runtime the settings screen switches it (`set_enabled`).
pub(crate) fn enabled() -> bool {
    opts::precedence(env_setting(), config::get().input.ble, DEFAULT)
}

// --------------------------------------------------------------------- //
// Runtime switch (settings screen) + status                             //
// --------------------------------------------------------------------- //

/// `init` ran: the input thread is on (`PW64_NO_INPUT` unset). Without it
/// BLE can't be switched on at all.
static AVAILABLE: AtomicBool = AtomicBool::new(false);
/// BLE wanted right now. The scanner thread follows it each cycle (scan +
/// claim, or stop scanning + disconnect every claimed pad); pad threads
/// leave as soon as they see it cleared.
static ON: AtomicBool = AtomicBool::new(false);
/// What the scanner thread found: `ADAPTER_*`.
static ADAPTER: AtomicU8 = AtomicU8::new(ADAPTER_UNKNOWN);
const ADAPTER_UNKNOWN: u8 = 0;
const ADAPTER_OK: u8 = 1;
const ADAPTER_NONE: u8 = 2;
/// The scanner thread, started lazily on the first switch-on (never when BLE
/// stays off) and kept for the whole run (see `run` for why it never
/// restarts). `None` inside: the spawn failed.
static SCANNER: OnceLock<Option<thread::Thread>> = OnceLock::new();

/// Called once from `main` when the input thread runs: makes BLE switchable
/// and applies the startup setting (`enabled`).
pub fn init() {
    AVAILABLE.store(true, Ordering::Release);
    set_enabled(enabled());
}

/// Why the settings row can't be changed: `PW64_NO_INPUT` (no controller
/// input at all) or `PW64_BLE` (the env var wins over the setting, like
/// every option). `None`: the row works.
pub(crate) fn locked_by() -> Option<&'static str> {
    if !AVAILABLE.load(Ordering::Acquire) {
        Some("PW64_NO_INPUT")
    } else if env_setting().is_some() {
        Some("PW64_BLE")
    } else {
        None
    }
}

/// BLE pads switched on right now (the settings row's value).
pub(crate) fn is_on() -> bool {
    ON.load(Ordering::Acquire)
}

/// Switches BLE pads on or off at runtime (settings screen), no restart:
/// on starts the scanner thread if it isn't running yet; off drops every
/// pad's state at once (the virtual pad goes idle) and wakes the scanner,
/// which stops scanning and disconnects the claimed pads (their threads
/// then exit). A no-op without the input thread (`PW64_NO_INPUT`).
pub(crate) fn set_enabled(on: bool) {
    if !AVAILABLE.load(Ordering::Acquire) {
        return;
    }
    ON.store(on, Ordering::Release);
    if !on {
        *STATE.lock().unwrap() = [None; 3];
    }
    let scanner = if on {
        SCANNER.get_or_init(start_scanner).as_ref()
    } else {
        SCANNER.get().and_then(Option::as_ref)
    };
    // Act now, not at the end of the scanner's 3 s sleep.
    if let Some(t) = scanner {
        t.unpark();
    }
}

fn start_scanner() -> Option<thread::Thread> {
    match thread::Builder::new().name("pw64-ble".into()).spawn(run) {
        Ok(h) => Some(h.thread().clone()),
        Err(e) => {
            eprintln!("[ble] could not start thread: {e}");
            None
        }
    }
}

/// What the settings screen shows under the BLE row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    /// Switched off.
    Off,
    /// On, the scanner hasn't looked for an adapter yet.
    Starting,
    /// On, but this PC has no (usable) Bluetooth adapter.
    NoAdapter,
    /// On and scanning; no pad has sent a report yet.
    Searching,
    /// These pads are connected (player-facing names, L before R).
    Connected(Vec<&'static str>),
}

/// The current status (`status_of` over the live state).
pub(crate) fn status() -> Status {
    status_of(
        ON.load(Ordering::Acquire),
        ADAPTER.load(Ordering::Acquire),
        &STATE.lock().unwrap(),
    )
}

/// Pure status logic (unit-tested without hardware).
fn status_of(on: bool, adapter: u8, slots: &Slots) -> Status {
    if !on {
        return Status::Off;
    }
    let connected: Vec<&'static str> = [Kind::JoyConL, Kind::JoyConR, Kind::Pro]
        .into_iter()
        .zip(slots)
        .filter(|(_, s)| s.is_some())
        .map(|(k, _)| kind_label(k))
        .collect();
    match adapter {
        _ if !connected.is_empty() => Status::Connected(connected),
        ADAPTER_NONE => Status::NoAdapter,
        ADAPTER_OK => Status::Searching,
        _ => Status::Starting,
    }
}

/// A device kind's player-facing name.
fn kind_label(k: Kind) -> &'static str {
    match k {
        Kind::JoyConL => "Joy-Con 2 (L)",
        Kind::JoyConR => "Joy-Con 2 (R)",
        Kind::Pro => "Pro Controller 2",
    }
}

/// `PW64_BLE_LIST=1`: scan for a few seconds, print every device found and
/// exit — the diagnostic for "why doesn't my pad show up". Runs from `main`
/// before anything else (no ROM needed), like `PW64_NO_INPUT` env handling.
pub fn list_and_exit() {
    eprintln!("[ble] PW64_BLE_LIST: scanning for 8 s (bring the pads into range) …");
    let scan: btleplug::Result<()> = block_on(async {
        let manager = Manager::new().await?;
        for adapter in manager.adapters().await? {
            adapter.start_scan(ScanFilter::default()).await?;
            let mut seen: Vec<PeripheralId> = Vec::new();
            for _ in 0..8 {
                thread::sleep(Duration::from_secs(1));
                for p in adapter.peripherals().await? {
                    // No properties yet (or gone again): try next second.
                    if seen.contains(&p.id()) {
                        continue;
                    }
                    let Ok(Some(props)) = p.properties().await else {
                        continue;
                    };
                    seen.push(p.id());
                    print_device(&props);
                }
            }
        }
        Ok(())
    });
    match &scan {
        Ok(()) => eprintln!("[ble] scan complete"),
        Err(e) => eprintln!("[ble] scan failed: {e}"),
    }
    std::process::exit(if scan.is_ok() { 0 } else { 1 });
}

fn print_device(props: &btleplug::api::PeripheralProperties) {
    let manu: Vec<String> = props
        .manufacturer_data
        .iter()
        .map(|(id, data)| {
            format!(
                "{id:04x}:{}",
                data.iter().map(|b| format!("{b:02x}")).collect::<String>()
            )
        })
        .collect();
    eprintln!(
        "[ble] {:?} addr {} rssi {:?} services {:?} manu {}",
        props.local_name,
        props.address,
        props.rssi,
        props.services,
        manu.join(" ")
    );
    if let Some(kind) = classify_props(props) {
        eprintln!(
            "[ble]   ^ Switch 2 pad ({kind:?}) — switch on Settings > Controls > \
             Switch 2 controllers (or PW64_BLE=1) to use it"
        );
    }
}

// --------------------------------------------------------------------- //
// Discovery + connection                                                //
// --------------------------------------------------------------------- //

/// Nintendo's Bluetooth SIG company id (advertisement manufacturer data).
const NINTENDO_COMPANY_ID: u16 = 0x0553;

/// An advertisement → device kind: by name, else by Nintendo manufacturer
/// data. Seen on real Joy-Con 2 in Sync mode (2026-10-02): no local name at
/// all, only `0553:01 00 03 7e05 6620 …` — USB vendor 0x057E + product id
/// (u16 LE) at bytes 3..7.
fn classify_props(props: &btleplug::api::PeripheralProperties) -> Option<Kind> {
    props
        .local_name
        .as_deref()
        .and_then(classify)
        .or_else(|| classify_manufacturer(props.manufacturer_data.get(&NINTENDO_COMPANY_ID)?))
}

/// Nintendo manufacturer data → device kind, by USB product id (same ids as
/// SDL's `USB_PRODUCT_NINTENDO_SWITCH2_*`). The NSO GameCube pad (0x2073)
/// has its own layout: not mapped.
fn classify_manufacturer(data: &[u8]) -> Option<Kind> {
    let vendor = u16::from_le_bytes(data.get(3..5)?.try_into().ok()?);
    let product = u16::from_le_bytes(data.get(5..7)?.try_into().ok()?);
    match (vendor, product) {
        (0x057E, 0x2066) => Some(Kind::JoyConR),
        (0x057E, 0x2067) => Some(Kind::JoyConL),
        (0x057E, 0x2069) => Some(Kind::Pro),
        _ => None,
    }
}

/// Advertisement name → device kind. Switch 1 pads ("Joy-Con (L)", "Pro
/// Controller") speak BLE HID with a different report format — not supported
/// here, so they are ignored.
fn classify(name: &str) -> Option<Kind> {
    let n = name.to_ascii_lowercase();
    if n.contains("pro controller 2") {
        Some(Kind::Pro)
    } else if n.contains("joy-con 2") || n.contains("joycon 2") {
        match (
            n.contains("(l)") || n.ends_with('l'),
            n.contains("(r)") || n.ends_with('r'),
        ) {
            (true, _) => Some(Kind::JoyConL),
            (_, true) => Some(Kind::JoyConR),
            _ => None,
        }
    } else {
        None
    }
}

/// Peripherals owned by a pad thread; released when it exits so the pad can
/// be claimed again after a drop.
type Claimed = Arc<Mutex<Vec<PeripheralId>>>;

/// The scanner thread (started by `set_enabled(true)`, lives for the whole
/// run). While `ON`, pads are scanned for continuously; each found pad gets
/// its own connection thread whose reports update the virtual pad state
/// (`pad()`), merged into controller 1 by the input thread. While off it
/// parks: no scan, no claims, every claimed pad disconnected.
fn run() {
    // The adapters are fetched once and held for the whole run: every
    // `Manager::adapters` builds a fresh WinRT watcher + radio-state
    // handler, so re-fetching per cycle (or per switch-on) leaks them.
    // Holding them also keeps their event streams alive, which the pad
    // threads use to notice disconnects. Only "no adapter at all" retries
    // (slowly, and only while on): nothing was built then.
    let adapters = loop {
        if !ON.load(Ordering::Acquire) {
            thread::park();
            continue;
        }
        match block_on(async { Manager::new().await?.adapters().await }) {
            Ok(a) if !a.is_empty() => break a,
            Ok(_) => eprintln!("[ble] no Bluetooth adapter"),
            Err(e) => eprintln!("[ble] no Bluetooth adapter: {e}"),
        }
        ADAPTER.store(ADAPTER_NONE, Ordering::Release);
        thread::park_timeout(NO_ADAPTER_RETRY);
    };
    ADAPTER.store(ADAPTER_OK, Ordering::Release);
    // btleplug's WinRT `start_scan` registers another advertisement handler
    // on every call, so the scan is never restarted per cycle: it starts
    // once per switch-on and stops at switch-off (a handler per player
    // toggle — bounded, unlike the per-cycle leak).
    let mut scanning = vec![false; adapters.len()];
    let claimed = Claimed::default();
    loop {
        let on = ON.load(Ordering::Acquire);
        for (adapter, scanning) in adapters.iter().zip(&mut scanning) {
            match (on, *scanning) {
                (true, false) => match block_on(adapter.start_scan(ScanFilter::default())) {
                    Ok(()) => *scanning = true,
                    Err(e) => eprintln!("[ble] scan: {e}"),
                },
                (false, true) => {
                    if let Err(e) = block_on(adapter.stop_scan()) {
                        eprintln!("[ble] stop scan: {e}");
                    }
                    *scanning = false;
                }
                _ => {}
            }
            let r = if on {
                block_on(claim_new_pads(adapter, &claimed))
            } else {
                block_on(disconnect_claimed(adapter, &claimed))
            };
            if let Err(e) = r {
                eprintln!("[ble] scan: {e}");
            }
        }
        if on {
            thread::park_timeout(SCAN_INTERVAL);
        } else if claimed.lock().unwrap().is_empty() {
            // Nothing left to drop: sleep until switched on again.
            thread::park();
        } else {
            // A pad thread still winding down (mid-connect): try again.
            thread::park_timeout(Duration::from_millis(500));
        }
    }
}

/// How often the scanner looks for a Bluetooth adapter again while BLE is
/// on but none was found (a dongle plugged in later).
const NO_ADAPTER_RETRY: Duration = Duration::from_secs(10);

/// Switch-off: disconnects every claimed pad. btleplug emits
/// `DeviceDisconnected` for a local disconnect, which ends the pad thread's
/// stream; the thread then releases its claim and exits.
async fn disconnect_claimed(adapter: &Adapter, claimed: &Claimed) -> btleplug::Result<()> {
    let ids = claimed.lock().unwrap().clone();
    for id in ids {
        if let Ok(p) = adapter.peripheral(&id).await {
            let _ = p.disconnect().await;
        }
    }
    Ok(())
}

/// Starts a pad thread for every unclaimed Switch 2 pad seen so far.
async fn claim_new_pads(adapter: &Adapter, claimed: &Claimed) -> btleplug::Result<()> {
    for p in adapter.peripherals().await? {
        let id = p.id();
        if claimed.lock().unwrap().contains(&id) {
            continue;
        }
        // No properties (the peripheral just vanished): skip, not a panic.
        let Ok(Some(props)) = p.properties().await else {
            continue;
        };
        let Some(kind) = classify_props(&props) else {
            continue;
        };
        let name = props.local_name.unwrap_or_else(|| kind_label(kind).into());
        // Switched off while this cycle ran: leave it to the next one.
        if !ON.load(Ordering::Acquire) {
            break;
        }
        claimed.lock().unwrap().push(id);
        eprintln!("[ble] connecting to {name} ({kind:?})");
        stream_pad(adapter.clone(), p, kind, Arc::clone(claimed));
    }
    Ok(())
}

/// One pad, one thread: connect, handshake, pump reports until the pad
/// disconnects, then give it back to the scanner (a sleep/wake cycle
/// reconnects it — btleplug re-lists it as a fresh peripheral). Connection
/// errors retry a few times first.
fn stream_pad(adapter: Adapter, p: Peripheral, kind: Kind, claimed: Claimed) {
    let r = std::thread::Builder::new()
        .name("pw64-pad".into())
        .spawn(move || {
            let mut failures = 0;
            loop {
                let result = block_on(connect_and_stream(&adapter, &p, kind));
                set_slot(kind, None);
                // Drop the WinRT device + cached services either way.
                let _ = block_on(p.disconnect());
                match result {
                    Ok(()) => break,
                    // Switched off (settings): no retries.
                    Err(_) if !ON.load(Ordering::Acquire) => break,
                    Err(e) => {
                        failures += 1;
                        if failures > 5 {
                            eprintln!("[ble] {kind:?}: giving up after {failures} failures: {e}");
                            break;
                        }
                        eprintln!("[ble] {kind:?}: {e}; retrying");
                        thread::sleep(SCAN_INTERVAL);
                    }
                }
            }
            claimed.lock().unwrap().retain(|id| *id != p.id());
        });
    if let Err(e) = r {
        eprintln!("[ble] could not start pad thread: {e}");
    }
}

/// What the pad thread waits on: an input report or an adapter event.
enum PadEvent {
    Note(ValueNotification),
    Central(CentralEvent),
}

/// Connects one pad and pumps its input reports into `STATE` until it
/// disconnects. The disconnect comes from the adapter's event stream: on
/// WinRT the notification stream is a broadcast owned by the peripheral and
/// never ends on its own, so waiting for it would leave the pad's last
/// report (held buttons / stick) stuck in `STATE` forever.
async fn connect_and_stream(adapter: &Adapter, p: &Peripheral, kind: Kind) -> btleplug::Result<()> {
    // Subscribe before connecting so a disconnect can't slip past.
    let events = adapter.events().await?;
    p.connect().await?;
    p.discover_services().await?;
    let chars = p.characteristics();
    let u = uuids();
    // Not a Switch 2 pad after all (name matched but the service isn't
    // there): give up quietly, the scanner moves on.
    let Some(input) = chars.iter().find(|c| c.uuid == u.input) else {
        eprintln!("[ble] {kind:?}: no input-report characteristic; not connecting");
        return Ok(());
    };
    let command = chars.iter().find(|c| c.uuid == u.command);
    let response = chars.iter().find(|c| c.uuid == u.response);
    handshake(p, command, response).await?;
    p.subscribe(input).await?;
    let notes = p.notifications().await?;
    if !p.is_connected().await? {
        return Ok(()); // dropped during the handshake
    }
    eprintln!("[ble] {kind:?}: streaming input reports");
    let id = p.id();
    let mut stream =
        futures::stream::select(notes.map(PadEvent::Note), events.map(PadEvent::Central));
    let mut leds = LedRetry::new();
    let mut cal = StickCal::new();
    while let Some(ev) = stream.next().await {
        // Switched off in the settings: leave even if the scanner's
        // disconnect raced past this connection (reports keep coming).
        if !ON.load(Ordering::Acquire) {
            eprintln!("[ble] {kind:?}: switched off");
            break;
        }
        match ev {
            // Other notifications: command responses etc.
            PadEvent::Note(n) if n.uuid == input.uuid => {
                if let Some(mut rep) = parse_report(&n.value) {
                    if cal.apply(&n.value, &mut rep) {
                        // The real travel, for the notes (stick range).
                        eprintln!(
                            "[ble] {kind:?}: stick travel -{:?} +{:?} (lx ly rx ry)",
                            cal.neg, cal.pos
                        );
                    }
                    set_slot(kind, Some(rep));
                }
                if let Some(command) = command
                    && leds.due(std::time::Instant::now())
                {
                    // A failed write is retried on the next due report.
                    let _ = send_player_leds(p, command).await;
                    if leds.sends == LED_MAX_SENDS {
                        eprintln!("[ble] {kind:?}: player LED not acknowledged");
                    }
                }
            }
            PadEvent::Note(n) if n.uuid == u.response => {
                if leds.on_response(&n.value) {
                    eprintln!(
                        "[ble] {kind:?}: player LED set (after {} send(s))",
                        leds.sends
                    );
                }
            }
            PadEvent::Central(CentralEvent::DeviceDisconnected(d)) if d == id => {
                eprintln!("[ble] {kind:?}: disconnected");
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Writes the handshake that makes the pad stream input reports. Isolated in
/// one function so verifying on real pads only ever touches this.
// TODO(verify on real pads): the sequence (subscribe the command-response
// characteristic before writing anything, then feature init + enable) comes
// from the Linux BLE bridge research, not from a real pad over WinRT. If pads
// start streaming after only the input-report subscription, this whole
// function can become a no-op.
async fn handshake(
    p: &Peripheral,
    command: Option<&Characteristic>,
    response: Option<&Characteristic>,
) -> btleplug::Result<()> {
    if let Some(response) = response {
        // The pad writes command replies as soon as they land; it needs the
        // CCCD subscribed before the first command.
        p.subscribe(response).await?;
    }
    let Some(command) = command else {
        return Ok(()); // input-only fallback: maybe reports flow anyway
    };
    let flags = [FEATURE_FLAGS, 0x00, 0x00, 0x00];
    p.write(
        command,
        &build_command(CMD_FEATURE, SUBCMD_FEATURE_INIT, &flags),
        WriteType::WithoutResponse,
    )
    .await?;
    p.write(
        command,
        &build_command(CMD_FEATURE, SUBCMD_FEATURE_ENABLE, &flags),
        WriteType::WithoutResponse,
    )
    .await?;
    Ok(())
}

/// Lights the player LED (ends the pairing sweep). Sent from the report
/// loop, not the handshake: right after the feature commands the pad
/// sometimes dropped it (seen on real Joy-Con 2), so it is resent until the
/// pad answers on the command-response characteristic (`LedRetry`).
async fn send_player_leds(p: &Peripheral, command: &Characteristic) -> btleplug::Result<()> {
    // All BLE pads feed controller 1 (the game is single-player).
    p.write(
        command,
        &build_command(CMD_LED, SUBCMD_LED_SET_PLAYER, &player_leds(1)),
        WriteType::WithoutResponse,
    )
    .await
}

/// Resend policy for the player-LED command, clocked by incoming input
/// reports (no async timers without a runtime): send, then resend every
/// `LED_RETRY_EVERY` until acknowledged, at most `LED_MAX_SENDS` times.
struct LedRetry {
    sends: u32,
    last: Option<std::time::Instant>,
    acked: bool,
}

const LED_RETRY_EVERY: Duration = Duration::from_millis(300);
const LED_MAX_SENDS: u32 = 10;

impl LedRetry {
    fn new() -> Self {
        Self {
            sends: 0,
            last: None,
            acked: false,
        }
    }

    /// Whether to (re)send now; records the send.
    fn due(&mut self, now: std::time::Instant) -> bool {
        let due = !self.acked
            && self.sends < LED_MAX_SENDS
            && self
                .last
                .is_none_or(|t| now.duration_since(t) >= LED_RETRY_EVERY);
        if due {
            self.sends += 1;
            self.last = Some(now);
        }
        due
    }

    /// A command-response notification: the reply's first byte echoes the
    /// command id.
    fn on_response(&mut self, value: &[u8]) -> bool {
        let ack = !self.acked && value.first() == Some(&CMD_LED);
        self.acked |= ack;
        ack
    }
}

/// LED data for player `n` (1..=4): the Switch's pattern, LED n alone lit.
fn player_leds(n: u8) -> [u8; 8] {
    let mut data = [0u8; 8];
    data[0] = 1 << (n.clamp(1, 4) - 1);
    data
}

/// Frames one command for the command characteristic (0x91 protocol):
/// `<cmd> 91 01 <sub> 00 <len> 00 00 <data>`.
fn build_command(command: u8, subcommand: u8, data: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + data.len());
    frame.extend_from_slice(&[
        command,
        0x91,
        0x01,
        subcommand,
        0x00,
        data.len() as u8,
        0x00,
        0x00,
    ]);
    frame.extend_from_slice(data);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{CONT_C_DOWN, CONT_C_RIGHT};

    const SLOT_L: usize = Kind::JoyConL as usize;
    const SLOT_R: usize = Kind::JoyConR as usize;
    const SLOT_PRO: usize = Kind::Pro as usize;

    /// Synthetic 63-byte input report: timestamp, buttons, two packed sticks.
    fn report(buttons: u32, lx: u32, ly: u32, rx: u32, ry: u32) -> Vec<u8> {
        let mut b = vec![0u8; 63];
        b[0..4].copy_from_slice(&0x11223344u32.to_le_bytes());
        b[4..8].copy_from_slice(&buttons.to_le_bytes());
        pack12(&mut b[10..13], lx, ly);
        pack12(&mut b[13..16], rx, ry);
        b
    }

    fn pack12(out: &mut [u8], x: u32, y: u32) {
        let v = (x & 0xFFF) | ((y & 0xFFF) << 12);
        out.copy_from_slice(&v.to_le_bytes()[..3]);
    }

    #[test]
    fn parse_rejects_short_and_accepts_center() {
        assert!(parse_report(&[0u8; 8]).is_none());
        assert!(parse_report(&[0u8; 15]).is_none());
        let r = parse_report(&report(B_A, 2048, 2048, 2048, 2048)).unwrap();
        assert_eq!(r.buttons, B_A);
        assert_eq!(r.left, (0.0, 0.0));
        assert_eq!(r.right, (0.0, 0.0));
    }

    #[test]
    fn stick_axes_normalize_y_up() {
        let r = parse_report(&report(0, 4095, 2048, 2048, 4095)).unwrap();
        assert!((r.left.0 - 1.0).abs() < 1e-3, "raw max → full right");
        assert!((r.right.1 - 1.0).abs() < 1e-3, "raw max → full up");
        let r = parse_report(&report(0, 0, 0, 0, 0)).unwrap();
        assert_eq!(r.left.0, -1.0);
        assert_eq!(r.left.1, -1.0, "raw 0 → full down");
        // Clamped past the 12-bit range.
        assert!(r.left.0 <= 1.0 && r.left.0 >= -1.0);
    }

    #[test]
    fn joycon_l_maps_stick_and_shoulders() {
        let mut state: Slots = [None; 3];
        state[SLOT_L] = parse_report(&report(
            B_L | B_ZL | B_MINUS | B_SL_L,
            2048 + 1000,
            2048,
            2048,
            2048,
        ));
        let pad = virtual_pad(&state);
        assert_eq!(
            pad.button,
            CONT_L | CONT_Z,
            "L→L, ZL→Z, SL→L; minus is the settings button, never the game's"
        );
        // (3048 - 2048) / 2048 = 0.488; past the radial deadzone that
        // rescales to (0.488 - 0.12) / 0.88 = 0.419 → 7 + 0.419·73 ≈ 38.
        assert_eq!(pad.stick_x, 38);
        assert_eq!(pad.stick_y, 0);
        // The R half's right stick would be C buttons; the L stick isn't.
        assert_eq!(pad.button & (CONT_A | CONT_R), 0);
    }

    #[test]
    fn joycon_r_maps_face_buttons_and_c_stick() {
        let mut state: Slots = [None; 3];
        state[SLOT_R] = parse_report(&report(
            B_A | B_B | B_Y | B_X | B_R | B_ZR | B_PLUS | B_SR_R,
            2048,
            2048,
            4095,
            2048, // right stick pushed right → C-right
        ));
        let pad = virtual_pad(&state);
        assert_eq!(
            pad.button,
            CONT_A | CONT_B | CONT_C_UP | CONT_R | CONT_START | CONT_C_RIGHT
        );
        // Main stick untouched (the R half only drives C buttons).
        assert_eq!((pad.stick_x, pad.stick_y), (0, 0));
    }

    #[test]
    fn merge_lr_joycons_into_one_pad() {
        let mut state: Slots = [None; 3];
        state[SLOT_L] = parse_report(&report(B_L, 4095, 2048, 2048, 2048));
        state[SLOT_R] = parse_report(&report(B_A | B_ZR, 2048, 2048, 4095, 2048));
        let pad = virtual_pad(&state);
        assert_eq!(pad.button, CONT_A | CONT_L | CONT_R | CONT_C_RIGHT);
        assert_eq!(pad.stick_x, 80, "main stick comes from the L half");
        assert!(pad.button & CONT_C_RIGHT != 0, "C buttons from the R half");
        // Unconnected halves leave no stick residue.
        let mut state: Slots = [None; 3];
        state[SLOT_R] = parse_report(&report(0, 2048, 2048, 2048, 2048));
        assert_eq!(virtual_pad(&state), Pad::default());
    }

    #[test]
    fn pro_maps_positionally_with_dpad() {
        let mut state: Slots = [None; 3];
        state[SLOT_PRO] = parse_report(&report(
            B_A | B_B
                | B_X
                | B_Y
                | B_UP
                | B_DOWN
                | B_LEFT
                | B_RIGHT
                | B_ZL
                | B_ZR
                | B_L
                | B_R
                | B_PLUS,
            0,
            2048,
            2048,
            0, // right stick down (raw y grows upward)
        ));
        let pad = virtual_pad(&state);
        assert_eq!(
            pad.button,
            CONT_A
                | CONT_B
                | CONT_C_UP
                | CONT_L
                | CONT_Z
                | CONT_R
                | CONT_START
                | CONT_UP
                | CONT_DOWN
                | CONT_LEFT
                | CONT_RIGHT
                | CONT_C_DOWN
        );
        assert_eq!(pad.stick_x, -80, "left stick → main stick");
        assert_eq!(pad.stick_y, 0);
    }

    #[test]
    fn face_buttons_by_nintendo_position() {
        // X is north on Nintendo pads → C-up; Y (west) → B. (The first
        // version had these swapped on the Pro, Xbox-style.)
        for slot in [SLOT_R, SLOT_PRO] {
            let mut state: Slots = [None; 3];
            state[slot] = parse_report(&report(B_X, 2048, 2048, 2048, 2048));
            assert_eq!(virtual_pad(&state).button, CONT_C_UP);
            state[slot] = parse_report(&report(B_Y, 2048, 2048, 2048, 2048));
            assert_eq!(virtual_pad(&state).button, CONT_B);
        }
        // The L Joy-Con's arrow buttons are the D-pad.
        let mut state: Slots = [None; 3];
        state[SLOT_L] = parse_report(&report(B_UP | B_LEFT, 2048, 2048, 2048, 2048));
        assert_eq!(virtual_pad(&state).button, CONT_UP | CONT_LEFT);
    }

    #[test]
    fn larger_main_stick_wins() {
        // L Joy-Con and Pro both drive the main stick: the deflected one wins
        // instead of whichever slot comes last.
        let mut state: Slots = [None; 3];
        state[SLOT_L] = parse_report(&report(0, 4095, 2048, 2048, 2048));
        state[SLOT_PRO] = parse_report(&report(0, 2048, 2048, 2048, 2048));
        assert_eq!(virtual_pad(&state).stick_x, 80);
    }

    #[test]
    fn classify_names() {
        assert_eq!(classify("Joy-Con 2 (L)"), Some(Kind::JoyConL));
        assert_eq!(classify("Joy-Con 2 (R)"), Some(Kind::JoyConR));
        assert_eq!(classify("Pro Controller 2"), Some(Kind::Pro));
        assert_eq!(classify("joycon 2l"), Some(Kind::JoyConL));
        // Switch 1 pads use BLE HID — deliberately out of scope.
        assert_eq!(classify("Joy-Con (L)"), None);
        assert_eq!(classify("Pro Controller"), None);
        assert_eq!(classify("Nintendo Switch Pro Controller"), None);
        assert_eq!(classify("Xbox Wireless Controller"), None);
    }

    /// A real stick's full push (well short of the 12-bit range) becomes
    /// full deflection; ranges grow with the furthest push seen.
    #[test]
    fn stick_cal_scales_to_real_travel() {
        let mut cal = StickCal::new();
        let mut apply = |lx: u32, ly: u32| {
            let b = report(0, lx, ly, 2048, 2048);
            let mut rep = parse_report(&b).unwrap();
            cal.apply(&b, &mut rep);
            rep
        };
        // At the floor travel: already full (nominal would give ~0.5).
        let floor = u32::from(STICK_MIN_TRAVEL);
        let r = apply(2048 + floor, 2048 - floor / 2);
        assert_eq!(r.left.0, 1.0);
        assert!((r.left.1 + 0.5).abs() < 1e-3);
        // A longer push grows that direction only; then it is the new 1.0.
        let r = apply(2048 + 1300, 2048);
        assert_eq!(r.left.0, 1.0);
        let r = apply(2048 + 650, 2048);
        assert!((r.left.0 - 0.5).abs() < 1e-3);
        let r = apply(2048 - floor, 2048);
        assert_eq!(r.left.0, -1.0, "negative side keeps its own range");
        let r = apply(2048, 2048);
        assert_eq!(r.left, (0.0, 0.0));
        assert_eq!(r.right, (0.0, 0.0));
    }

    /// A full diagonal on the round Joy-Con gate (each axis ~0.71 of its
    /// cardinal travel) reaches the N64 octagon corner, which the game reads
    /// as full on both axes (input::stick_to_n64).
    #[test]
    fn joycon_full_diagonal_is_full_on_both_axes() {
        let mut cal = StickCal::new();
        let mut pad = |lx: u32, ly: u32| {
            let b = report(0, lx, ly, 2048, 2048);
            let mut rep = parse_report(&b).unwrap();
            cal.apply(&b, &mut rep);
            stick_to_n64(rep.left.0, rep.left.1)
        };
        // Learn the real travel (1200) in all four directions.
        for (x, y) in [(3248, 2048), (848, 2048), (2048, 3248), (2048, 848)] {
            pad(x, y);
        }
        let d = (1200.0 * std::f32::consts::FRAC_1_SQRT_2) as u32; // 848
        for (sx, sy) in [(1i32, 1i32), (1, -1), (-1, 1), (-1, -1)] {
            let (x, y) = pad((2048 + sx * d as i32) as u32, (2048 + sy * d as i32) as u32);
            assert!(x.abs() >= 70 && y.abs() >= 70, "({sx},{sy}) → {x},{y}");
            assert_eq!((x.signum() as i32, y.signum() as i32), (sx, sy));
        }
    }

    /// Settings buttons never reach the game's pad, on either half.
    #[test]
    fn settings_buttons_are_not_game_buttons() {
        for b in [B_MINUS, B_HOME, B_CAPTURE] {
            for slot in [SLOT_L, SLOT_R, SLOT_PRO] {
                let mut state: Slots = [None; 3];
                state[slot] = parse_report(&report(b, 2048, 2048, 2048, 2048));
                assert_eq!(virtual_pad(&state).button, 0, "bit {b:#x} slot {slot}");
            }
        }
    }

    /// Real advertisements captured with `PW64_BLE_LIST` (nameless Joy-Con 2).
    #[test]
    fn classify_manufacturer_data() {
        let hex = |s: &str| -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        };
        let r = hex("0100037e0566200001000000000000000f00000000000000");
        let l = hex("0100037e0567200001000000000000000f00000000000000");
        assert_eq!(classify_manufacturer(&r), Some(Kind::JoyConR));
        assert_eq!(classify_manufacturer(&l), Some(Kind::JoyConL));
        assert_eq!(
            classify_manufacturer(&hex("0100037e056920")),
            Some(Kind::Pro)
        );
        assert_eq!(classify_manufacturer(&hex("0100037e057320")), None);
        assert_eq!(classify_manufacturer(&hex("0100")), None);
    }

    #[test]
    fn status_follows_switch_adapter_and_slots() {
        let none: Slots = [None; 3];
        assert_eq!(status_of(false, ADAPTER_OK, &none), Status::Off);
        assert_eq!(status_of(true, ADAPTER_UNKNOWN, &none), Status::Starting);
        assert_eq!(status_of(true, ADAPTER_NONE, &none), Status::NoAdapter);
        assert_eq!(status_of(true, ADAPTER_OK, &none), Status::Searching);
        let mut both: Slots = [None; 3];
        both[SLOT_R] = parse_report(&report(0, 2048, 2048, 2048, 2048));
        both[SLOT_L] = both[SLOT_R];
        assert_eq!(
            status_of(true, ADAPTER_OK, &both),
            Status::Connected(vec!["Joy-Con 2 (L)", "Joy-Con 2 (R)"])
        );
        // Off wins over stale slots (they are cleared on switch-off anyway).
        assert_eq!(status_of(false, ADAPTER_OK, &both), Status::Off);
    }

    /// Without `init` (the input thread off, as in tests and with
    /// `PW64_NO_INPUT`) BLE can't be switched on and the row is locked.
    #[test]
    fn switch_is_a_no_op_without_input() {
        set_enabled(true);
        assert!(!is_on());
        assert_eq!(locked_by(), Some("PW64_NO_INPUT"));
        assert_eq!(pad(), Pad::default());
        assert!(!any_connected());
        assert!(SCANNER.get().is_none(), "no thread started");
    }

    #[test]
    fn command_framing() {
        assert_eq!(
            build_command(CMD_FEATURE, SUBCMD_FEATURE_ENABLE, &[0x07, 0, 0, 0]),
            vec![
                0x0c, 0x91, 0x01, 0x04, 0x00, 0x04, 0x00, 0x00, 0x07, 0, 0, 0
            ]
        );
        assert_eq!(
            build_command(CMD_LED, SUBCMD_LED_SET_PLAYER, &player_leds(1)),
            vec![
                0x09, 0x91, 0x01, 0x07, 0x00, 0x08, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0
            ]
        );
        assert_eq!(player_leds(3)[0], 0b0100);
        // Resend until acknowledged, spaced, capped.
        let t0 = std::time::Instant::now();
        let mut r = LedRetry::new();
        assert!(r.due(t0));
        assert!(!r.due(t0 + Duration::from_millis(100)), "too soon");
        assert!(r.due(t0 + LED_RETRY_EVERY));
        assert!(!r.on_response(&[0x0c, 0x01]), "other command's reply");
        assert!(r.on_response(&[CMD_LED, 0x01]));
        assert!(!r.due(t0 + LED_RETRY_EVERY * 5), "acked: done");
        let mut r = LedRetry::new();
        let sent = (0..50u32)
            .filter(|&i| r.due(t0 + LED_RETRY_EVERY * i))
            .count();
        assert_eq!(sent as u32, LED_MAX_SENDS);
        assert_eq!(player_leds(9)[0], 0b1000, "clamped to 4 players");
    }
}
