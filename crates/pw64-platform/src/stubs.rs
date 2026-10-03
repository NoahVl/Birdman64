//! Placeholder definitions for every symbol the decomp C imports but nothing
//! implements yet. The list is exactly the link's unresolved-symbol set
//! (kernel + app + libultra audio/sp, whole-archive).
//!
//! Functions panic with their name when called. To implement one, delete it
//! here and add a real `#[unsafe(no_mangle)] extern "C"` definition in the
//! subsystem's module, with the C prototype quoted in the doc comment.
//! Data symbols are zero-filled placeholders of at least the C object's size.

#![allow(non_upper_case_globals, non_snake_case)]

/// Defines panicking `extern "C"` stubs. The C prototype is kept as the doc
/// string; the Rust signature is irrelevant because the stub never returns.
#[allow(unused_macros)]
macro_rules! stubs {
    ($($name:ident => $proto:literal;)*) => {$(
        #[doc = $proto]
        #[unsafe(no_mangle)]
        pub extern "C-unwind" fn $name() -> ! {
            panic!(concat!("unimplemented platform stub: ", stringify!($name), "  [", $proto, "]"))
        }
    )*};
}

/// Defines zero-filled placeholder data symbols of `$len` bytes (8-aligned).
macro_rules! data_stubs {
    ($($name:ident: [$len:expr] => $decl:literal;)*) => {$(
        #[doc = $decl]
        #[unsafe(no_mangle)]
        pub static mut $name: Aligned<$len> = Aligned([0; $len]);
    )*};
}

/// 8-byte-aligned byte blob (u64 alignment for the ucode symbols).
#[repr(C, align(8))]
pub struct Aligned<const N: usize>(pub [u8; N]);

// --- libultra globals (low RAM on N64) ---
data_stubs! {
    // 56 modes x sizeof(OSViMode) (0x50 on N64; a little headroom for host padding).
    osViModeTable: [{ 56 * 0x60 }] => "extern OSViMode osViModeTable[];";
}

// --- RSP microcode symbols: only their addresses go into OSTask ---
data_stubs! {
    rspbootTextStart: [8] => "extern long long int rspbootTextStart[];";
    rspbootTextEnd: [8] => "extern long long int rspbootTextEnd[];";
    gspFast3DTextStart: [8] => "extern long long int gspFast3DTextStart[];";
    gspFast3DDataStart: [8] => "extern long long int gspFast3DDataStart[];";
    gspF3DEX_fifoTextStart: [8] => "extern long long int gspF3DEX_fifoTextStart[];";
    gspF3DEX_fifoDataStart: [8] => "extern long long int gspF3DEX_fifoDataStart[];";
    aspMainTextStart: [8] => "extern long long int aspMainTextStart[];";
    aspMainDataStart: [8] => "extern long long int aspMainDataStart[];";
}
