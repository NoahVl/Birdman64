/*
 * Our own menu strings in the game's 16-bit text encoding (splitscreen.md
 * "Glyph encoding"). Menu items, test names and hints are s16 strings of
 * glyph indices into the text font (font 6; menu.c, uvFontPrintStr16),
 * ended by 0xFFE (end of line) and -1 (end of string). These helpers turn
 * plain ASCII into that form, so native menus (free flight, later player 2
 * menus) print through the game's own text path.
 *
 * A glyph index is the character's position in the font's STRG table (what
 * uvFontPrintStr does for ASCII fonts): font 6 starts "0-9 A-Z a-z" with a
 * space at 0x42 and punctuation after it. Glyphs 0x60.. repeat 0-9 A-Z a-z
 * in the bold style the main menus use (textFmtInt's +0x60 digits).
 */
#include "common.h"
#include <uv_graphics.h>
#include <uv_string.h>

#define PW64_TEXT_FONT 6
#define PW64_TEXT_SPACE 0x42
#define PW64_TEXT_BOLD 0x60

/* Glyph for one ASCII character: the font's own table when the font is
 * loaded (covers punctuation), else the fixed layout above; anything the
 * font lacks becomes a space. */
static s16 pw64_text_glyph(char c, s32 bold) {
    ParsedUVFT* font = (gGfxUnkPtrs != NULL) ? gGfxUnkPtrs->fonts[PW64_TEXT_FONT] : NULL;
    char* pos;
    s16 g = -1;

    if (c >= '0' && c <= '9') {
        g = c - '0';
    } else if (c >= 'A' && c <= 'Z') {
        g = 0x0A + (c - 'A');
    } else if (c >= 'a' && c <= 'z') {
        g = 0x24 + (c - 'a');
    }
    if (g >= 0) {
        return bold ? g + PW64_TEXT_BOLD : g;
    }
    if ((c != ' ') && (font != NULL) && (font->str != NULL)) {
        pos = uvStrchr(font->str, c);
        if ((pos != NULL) && ((pos - font->str) < PW64_TEXT_BOLD)) {
            return (s16)(pos - font->str);
        }
    }
    return PW64_TEXT_SPACE;
}

/* Encodes `ascii` into `out` (room for `max` s16, terminators included):
 * glyphs, 0xFFE, -1. Returns the glyph count (truncated to fit). `bold`
 * picks the main-menu letter style for digits and letters. */
int pw64_text_encode_style(const char* ascii, short* out, int max, int bold) {
    s32 n = 0;

    if (max < 2) {
        return 0;
    }
    while ((ascii[n] != '\0') && (n < max - 2)) {
        out[n] = pw64_text_glyph(ascii[n], bold);
        n++;
    }
    out[n] = 0xFFE;
    out[n + 1] = -1;
    return n;
}

int pw64_text_encode(const char* ascii, short* out, int max) {
    return pw64_text_encode_style(ascii, out, max, 0);
}
