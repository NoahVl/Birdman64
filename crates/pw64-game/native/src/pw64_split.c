/*
 * Split-screen layout (splitscreen.md): with the flag on, a test runs two
 * pilots (pw64_players.c, game.c.patch): kernel channel 0 = player 1's
 * camera (top or left view), channel 1 = player 2's (bottom or right).
 * Viewport/frustum per camera: pw64_split_viewport (camera.c.patch).
 */
int pw64_split = 0; /* 0 off, 1 on (two players; set before boot) */

/* View arrangement: 0 top/bottom, 1 left/right. Rust may set it any time
 * (pw64_game::set_split_layout, also live from the settings screen); each
 * test start latches it into pw64_split_lr (game.c.patch), which is what
 * the viewports and the renderer tags read for the rest of the test. */
int pw64_split_layout = 0;
int pw64_split_lr = 0;

/* Player 2's pilot (PILOT_*), -1 = fly player 1's. Only player 2's vehicle
 * load and the level's model list read it (game.c.patch, level.c.patch). */
int pw64_p2_pilot = -1;
