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

/// Human-readable class names, indexed by class id.
const CLASS_NAMES: [&str; 4] = ["beginner", "a", "b", "pilot"];

/// Human-readable vehicle names, indexed by vehicle id.
const VEHICLE_NAMES: [&str; 7] = [
    "hang glider",
    "rocket belt",
    "gyrocopter",
    "cannonball",
    "sky diving",
    "jumble hopper",
    "birdman",
];

/// Number of tests each class has per vehicle (0 = the combination doesn't
/// exist), mirroring `taskGetTestCount` (decomp task.c:184, which counts the
/// UPWT task files loaded in `taskInit`, task.c:118): hang glider/rocket
/// belt/gyrocopter have 1/2/3/3 tests in beginner/a/b/pilot; cannonball has
/// 4 tests in beginner/a/b and none in pilot; sky diving and jumble hopper
/// have 1 test each in beginner/a/b; birdman has 4 in every class.
const TEST_COUNTS: [[i32; 7]; 4] = [
    // hg rb gc cb sd jh bm
    [1, 1, 1, 4, 1, 1, 4], // beginner
    [2, 2, 2, 4, 1, 1, 4], // a
    [3, 3, 3, 4, 1, 1, 4], // b
    [3, 3, 3, 0, 0, 0, 4], // pilot
];

/// Number of tests `class` has for `veh` (0 = combination doesn't exist).
fn test_count(class: i32, veh: i32) -> i32 {
    TEST_COUNTS
        .get(class as usize)
        .and_then(|r| r.get(veh as usize))
        .copied()
        .unwrap_or(0)
}

/// Parses the `PW64_START` value. Class `beginner|a|b|pilot`; vehicle
/// `hg|rb|gc|cb|sd|jh|bm` (or full names); test 1-based and must exist for
/// that class+vehicle; pilot `lark|goose|hawk|kiwi|ibis|robin` (default
/// lark); file 1|2 (default 1).
pub fn parse(s: &str) -> Result<Start, String> {
    let parts: Vec<String> = s
        .split(':')
        .map(|p| p.trim().to_ascii_lowercase())
        .collect();
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
        v => {
            return Err(format!(
                "unknown vehicle `{v}` (hg, rb, gc, cb, sd, jh, bm)"
            ));
        }
    };
    let test = match parts[2].parse::<i32>() {
        Ok(t) if (1..=test_count(class, vehicle)).contains(&t) => t - 1,
        _ => {
            let count = test_count(class, vehicle);
            if count == 0 {
                return Err(format!(
                    "class {} has no {} tests",
                    CLASS_NAMES[class as usize], VEHICLE_NAMES[vehicle as usize]
                ));
            }
            return Err(format!(
                "class {} {} has tests 1-{count}",
                CLASS_NAMES[class as usize], VEHICLE_NAMES[vehicle as usize]
            ));
        }
    };
    let pilot = match parts.get(3) {
        None => 0,
        Some(p) => parse_pilot(p)?,
    };
    let file = match parts.get(4).map(String::as_str) {
        None | Some("1") => 0,
        Some("2") => 1,
        Some(f) => return Err(format!("bad file `{f}` (1 or 2)")),
    };
    Ok(Start {
        file,
        class,
        vehicle,
        test,
        pilot,
    })
}

/// Pilot name → decomp `PILOT_*` id (lark 0, goose 1, hawk 2, kiwi 3,
/// ibis 4, robin/hooter 5), case-insensitive.
pub fn parse_pilot(s: &str) -> Result<i32, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "lark" => Ok(0),
        "goose" => Ok(1),
        "hawk" => Ok(2),
        "kiwi" => Ok(3),
        "ibis" => Ok(4),
        "robin" | "hooter" => Ok(5),
        p => Err(format!(
            "unknown pilot `{p}` (lark, goose, hawk, kiwi, ibis, robin)"
        )),
    }
}

/// Called by the patched title state on a `PW64_START` boot (split-screen):
/// player 2's pilot from `PW64_P2_PILOT`, or -1 (unset/invalid: player 1's).
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pw64_p2_pilot_env() -> i32 {
    let Ok(v) = std::env::var("PW64_P2_PILOT") else {
        return -1;
    };
    match parse_pilot(&v) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[start] ignoring PW64_P2_PILOT: {e}");
            -1
        }
    }
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
            Start {
                file: 0,
                class: 1,
                vehicle: 0,
                test: 1,
                pilot: 0
            }
        );
        assert_eq!(
            parse("Pilot:Gyrocopter:3:ibis:2").unwrap(),
            Start {
                file: 1,
                class: 3,
                vehicle: 2,
                test: 2,
                pilot: 4
            }
        );
        // last valid test of each bonus/multi-test combination
        for valid in ["beginner:cb:4", "pilot:bm:4", "a:sd:1", "b:jh:1"] {
            assert!(parse(valid).is_ok(), "{valid}");
        }
        for bad in [
            "a:hg",
            "c:hg:1",
            "a:xx:1",
            "a:hg:0",
            "a:hg:1:bob",
            "a:hg:1:lark:3",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_pilots() {
        let names = ["lark", "goose", "hawk", "kiwi", "ibis", "robin"];
        for (i, n) in names.iter().enumerate() {
            assert_eq!(parse_pilot(n), Ok(i as i32));
        }
        assert_eq!(parse_pilot(" Hooter "), Ok(5));
        assert!(parse_pilot("bob").is_err());
        assert!(parse_pilot("").is_err());
    }

    #[test]
    fn rejects_missing_tests() {
        // test number too high for the class+vehicle
        for bad in ["beginner:rb:2", "a:rb:3", "b:hg:4", "a:cb:5", "pilot:bm:5"] {
            let e = parse(bad).unwrap_err();
            assert!(e.contains("has tests 1-"), "{bad}: {e}");
        }
        // class+vehicle combination doesn't exist
        for bad in ["pilot:cb:1", "pilot:sd:1", "pilot:jh:1"] {
            let e = parse(bad).unwrap_err();
            assert!(e.contains("no"), "{bad}: {e}");
        }
    }

    #[test]
    fn test_counts_match_rom() {
        assert_eq!(test_count(0, 0), 1); // beginner hang glider
        assert_eq!(test_count(1, 1), 2); // a rocket belt
        assert_eq!(test_count(2, 3), 4); // b cannonball
        assert_eq!(test_count(3, 3), 0); // pilot cannonball: doesn't exist
        assert_eq!(test_count(3, 6), 4); // pilot birdman
        assert_eq!(test_count(9, 9), 0); // out of range
    }
}
