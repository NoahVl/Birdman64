//! User SDL2 controller mappings (docs/notes/input.md "Generic controllers").
//!
//! gilrs ships SDL_GameControllerDB; players can add their own lines, the
//! format SDL2 Gamepad Tool, Steam and `SDL_GAMECONTROLLERCONFIG` use
//! (`GUID,name,a:b0,b:b1,…,platform:Windows,`), in `gamecontrollerdb.txt` in
//! the data folder, or in that environment variable. They load once, when
//! the input thread starts.
//!
//! Lines are checked and normalised before gilrs sees them, because gilrs
//! matches a mapping to a pad by its exact GUID and builds that GUID without
//! two fields newer SDL tools fill in: bytes 2-3 (a CRC of the device name,
//! SDL 2.26+) and bytes 14-15 (SDL's driver signature, e.g. `h` for HIDAPI).
//! `03003086790000000600000000000000` from SDL2 Gamepad Tool is
//! `03000000790000000600000000000000` to gilrs; copied as-is it would never
//! match. Only GUIDs with SDL's vendor/product layout (zero bytes 6-7 and
//! 10-11) are rewritten; older name-hash GUIDs are kept.

/// The SDL platform name gilrs filters mappings by (gilrs's
/// `SDL_PLATFORM_NAME`; lines for other platforms are skipped by it, so they
/// are reported here instead of silently doing nothing).
pub(crate) const PLATFORM: &str = if cfg!(target_os = "windows") {
    "Windows"
} else if cfg!(target_os = "macos") {
    "Mac OS X"
} else if cfg!(target_os = "linux") {
    "Linux"
} else {
    "Unknown"
};

/// The checked mappings from one source.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct UserMappings {
    /// The lines for gilrs (`GilrsBuilder::add_mappings`), normalised.
    pub text: String,
    /// GUIDs (normalised, lower case) and names of the mappings kept.
    pub loaded: Vec<(String, String)>,
    /// Player-facing notes: lines skipped or rewritten, with line numbers.
    pub notes: Vec<String>,
}

/// Checks and normalises SDL mapping text for `platform` (see the module
/// docs). `source` names the file or variable in the notes.
pub(crate) fn parse(text: &str, source: &str, platform: &str) -> UserMappings {
    let mut out = UserMappings::default();
    for (n, line) in text.lines().enumerate() {
        let n = n + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split(',');
        let guid = fields.next().unwrap_or("").trim();
        let name = fields.next().unwrap_or("").trim();
        let rest: Vec<&str> = fields.map(str::trim).filter(|f| !f.is_empty()).collect();
        if guid.len() != 32 || !guid.bytes().all(|b| b.is_ascii_hexdigit()) {
            out.notes.push(format!(
                "{source} line {n}: skipped, the GUID must be 32 hex digits ({guid:?})"
            ));
            continue;
        }
        if name.is_empty() {
            out.notes.push(format!(
                "{source} line {n}: skipped, the controller name is missing"
            ));
            continue;
        }
        if let Some(bad) = rest.iter().find(|f| !f.contains(':')) {
            out.notes.push(format!(
                "{source} line {n}: skipped, {bad:?} is not a key:value entry"
            ));
            continue;
        }
        let elements = rest.iter().filter(|f| !f.starts_with("platform:")).count();
        if elements == 0 {
            out.notes.push(format!(
                "{source} line {n}: skipped, it maps no buttons or axes"
            ));
            continue;
        }
        if let Some(p) = rest.iter().find_map(|f| f.strip_prefix("platform:"))
            && p.trim() != platform
        {
            out.notes.push(format!(
                "{source} line {n}: skipped, it is for {} (this is {platform})",
                p.trim()
            ));
            continue;
        }
        let fixed = gilrs_guid(guid);
        if fixed != guid.to_ascii_lowercase() {
            out.notes.push(format!(
                "{source} line {n}: GUID {guid} used as {fixed} (the form gilrs matches)"
            ));
        }
        out.text.push_str(&fixed);
        out.text.push(',');
        out.text.push_str(name);
        for f in &rest {
            out.text.push(',');
            out.text.push_str(f);
        }
        out.text.push_str(",\n");
        out.loaded.push((fixed, name.to_string()));
    }
    out
}

/// The GUID as gilrs builds it for the same device: name CRC (bytes 2-3)
/// and driver signature (bytes 14-15) zeroed, for GUIDs in SDL's
/// bus/vendor/product/version layout (bytes 6-7 and 10-11 zero). Others,
/// and anything that isn't 32 hex digits, come back lower-cased only.
pub(crate) fn gilrs_guid(guid: &str) -> String {
    let g = guid.to_ascii_lowercase();
    let b = g.as_bytes();
    let hex_ok = b.len() == 32 && b.iter().all(u8::is_ascii_hexdigit);
    // Hex digit offsets: byte i is chars 2i..2i+2.
    let zero = |byte: usize| &g[byte * 2..byte * 2 + 2] == "00";
    if !hex_ok || !(zero(6) && zero(7) && zero(10) && zero(11)) {
        return g;
    }
    format!("{}0000{}0000", &g[..4], &g[8..28])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_and_driver_bytes_are_zeroed() {
        // The issue's device (SDL2 Gamepad Tool, SDL 2.26+): CRC 0x8630.
        assert_eq!(
            gilrs_guid("03003086790000000600000000000000"),
            "03000000790000000600000000000000"
        );
        // Not 32 digits: untouched apart from case.
        assert_eq!(gilrs_guid("0300B7E6"), "0300b7e6");
        // SDL HIDAPI driver signature ('h') in byte 14, plus a CRC.
        assert_eq!(
            gilrs_guid("0300b7e67e0500001920000000016800"),
            "030000007e0500001920000000010000"
        );
        // Already in gilrs form: unchanged (lower-cased).
        assert_eq!(
            gilrs_guid("030000005E040000E002000000000000"),
            "030000005e040000e002000000000000"
        );
        // An old name-hash GUID (nonzero bytes 6-7): kept.
        assert_eq!(
            gilrs_guid("05000000ab12cd34ef56000000000000"),
            "05000000ab12cd34ef56000000000000"
        );
    }

    #[test]
    fn lines_are_checked_and_reported() {
        let text = "\
# my clone
03003086790000000600000000000000,USB Joystick,a:b2,b:b1,leftx:a0,lefty:a1,platform:Windows,

nothex,Bad,a:b0,
03000000790000000600000000000000,,a:b0,
03000000790000000600000000000000,No elements,platform:Windows,
03000000790000000600000000000000,Mac pad,a:b0,platform:Mac OS X,
03000000790000000600000000000000,Typo,a:b0,oops,
030000007900000006000000000000AA,Any platform,a:b0,b:b1
";
        let m = parse(text, "gamecontrollerdb.txt", "Windows");
        assert_eq!(
            m.loaded,
            vec![
                (
                    "03000000790000000600000000000000".into(),
                    "USB Joystick".into()
                ),
                (
                    "03000000790000000600000000000000".into(),
                    "Any platform".into()
                ),
            ]
        );
        assert_eq!(
            m.text,
            "03000000790000000600000000000000,USB Joystick,a:b2,b:b1,leftx:a0,lefty:a1,\
             platform:Windows,\n03000000790000000600000000000000,Any platform,a:b0,b:b1,\n"
        );
        let notes = m.notes.join("\n");
        for want in [
            "line 2: GUID 03003086790000000600000000000000 used as",
            "line 4: skipped, the GUID",
            "line 5: skipped, the controller name",
            "line 6: skipped, it maps no",
            "line 7: skipped, it is for Mac OS X (this is Windows)",
            "line 8: skipped, \"oops\"",
        ] {
            assert!(notes.contains(want), "{want} in:\n{notes}");
        }
        // A line without `platform:` loads everywhere; its driver byte
        // (byte 15 = 0xaa) is dropped like the CRC.
        assert!(notes.contains("line 9: GUID 030000007900000006000000000000AA used as"));
    }
}
