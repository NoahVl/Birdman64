//! `PW64_START=<class>:<vehicle>:<test>[:<pilot>[:<file>]]`: boot straight
//! into a test, skipping the title and menus. The patched title state
//! (game.c.patch) asks once via `pw64_direct_start`; after the test the game
//! returns to its menus as usual. Results are saved to the chosen file, as
//! when that file was picked in the file menu.

use std::sync::atomic::{AtomicBool, Ordering};

/// One test to boot into (decomp enum values, see task.h).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Start {
    pub file: i32,
    pub class: i32,
    pub vehicle: i32,
    pub test: i32,
    pub pilot: i32,
}

/// Parses the `PW64_START` value. Class `beginner|a|b|pilot`; vehicle
/// `hg|rb|gc|cb|sd|jh|bm` (or full names); test 1-based; pilot
/// `lark|goose|hawk|kiwi|ibis|robin` (default lark); file 1|2 (default 1).
pub fn parse(s: &str) -> Result<Start, String> {
    let parts: Vec<String> = s.split(':').map(|p| p.trim().to_ascii_lowercase()).collect();
    if !(3..=5).contains(&parts.len()) {
        return Err("expected <class>:<vehicle>:<test>[:<pilot>[:<file>]]".into());
    }
    let class = match parts[0].as_str() {
        "beginner" | "0" => 0,
        "a" | "1" => 1,
        "b" | "2" => 2,
        "pilot" | "p" | "3" => 3,
        c => return Err(format!("unknown class `{c}` (beginner, a, b, pilot)")),
    };
    let vehicle = match parts[1].as_str() {
        "hg" | "hangglider" | "hang_glider" => 0,
        "rb" | "rocketbelt" | "rocket_belt" => 1,
        "gc" | "gyrocopter" => 2,
        "cb" | "cannonball" => 3,
        "sd" | "skydiving" | "sky_diving" => 4,
        "jh" | "jumblehopper" | "jumble_hopper" => 5,
        "bm" | "birdman" => 6,
        v => return Err(format!("unknown vehicle `{v}` (hg, rb, gc, cb, sd, jh, bm)")),
    };
    let test = match parts[2].parse::<i32>() {
        Ok(t @ 1..=8) => t - 1,
        _ => return Err(format!("bad test `{}` (1-based)", parts[2])),
    };
    let pilot = match parts.get(3).map(String::as_str) {
        None | Some("lark") => 0,
        Some("goose") => 1,
        Some("hawk") => 2,
        Some("kiwi") => 3,
        Some("ibis") => 4,
        Some("robin" | "hooter") => 5,
        Some(p) => return Err(format!("unknown pilot `{p}`")),
    };
    let file = match parts.get(4).map(String::as_str) {
        None | Some("1") => 0,
        Some("2") => 1,
        Some(f) => return Err(format!("bad file `{f}` (1 or 2)")),
    };
    Ok(Start { file, class, vehicle, test, pilot })
}

static ASKED: AtomicBool = AtomicBool::new(false);

/// Called by the patched `gameUpdateStateTitle`: fills the out params and
/// returns 1 the first time when `PW64_START` is set and valid, else 0.
///
/// # Safety
/// All pointers must be valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pw64_direct_start(
    file: *mut i32,
    class: *mut i32,
    vehicle: *mut i32,
    test: *mut i32,
    pilot: *mut i32,
) -> i32 {
    if ASKED.swap(true, Ordering::Relaxed) {
        return 0;
    }
    let Ok(v) = std::env::var("PW64_START") else {
        return 0;
    };
    match parse(&v) {
        Ok(s) => {
            eprintln!("[start] PW64_START={v} → {s:?}");
            // SAFETY: the C passes addresses of its locals.
            unsafe {
                *file = s.file;
                *class = s.class;
                *vehicle = s.vehicle;
                *test = s.test;
                *pilot = s.pilot;
            }
            1
        }
        Err(e) => {
            eprintln!("[start] ignoring PW64_START={v}: {e}");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        assert_eq!(
            parse("a:hg:2").unwrap(),
            Start { file: 0, class: 1, vehicle: 0, test: 1, pilot: 0 }
        );
        assert_eq!(
            parse("Pilot:Gyrocopter:3:ibis:2").unwrap(),
            Start { file: 1, class: 3, vehicle: 2, test: 2, pilot: 4 }
        );
        for bad in ["a:hg", "c:hg:1", "a:xx:1", "a:hg:0", "a:hg:1:bob", "a:hg:1:lark:3"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
