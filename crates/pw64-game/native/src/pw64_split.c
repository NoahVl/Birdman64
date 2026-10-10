/*
 * Split-screen layout (splitscreen.md): with the flag on, a test runs two
 * pilots (pw64_players.c, game.c.patch): kernel channel 0 = player 1's
 * camera (top or left view), channel 1 = player 2's (bottom or right).
 * Viewport/frustum per camera: pw64_split_viewport (camera.c.patch).
 */
int pw64_split = 0; /* 0 off, 1 on (two players; set before boot) */

/* Live on/off (game.c.patch pw64_split_apply/pw64_split_poll): Rust's
 * settings row asks with pw64_split_req (0/1, -1 = nothing pending); the
 * game applies it in a menu state that holds no test and clears it. The
 * main menu's Players item switches directly. pw64_split_hint (Rust): 1 =
 * player 2's controller isn't connected (the main menu shows a hint). */
int pw64_split_req = -1;
int pw64_split_hint = 0;

/* View arrangement: 0 top/bottom, 1 left/right. Rust may set it any time
 * (pw64_game::set_split_layout, also live from the settings screen); each
 * test start latches it into pw64_split_lr (game.c.patch), which is what
 * the viewports and the renderer tags read for the rest of the test. */
int pw64_split_layout = 0;
int pw64_split_lr = 0;

/* Share best score: 1 = when another player's run beat player 1's, player
 * 1's save entry for the test is rescored with that run (game.c.patch
 * pw64_share_best, after the tallies). Rust may set it any time
 * (pw64_game::set_split_share, also live from the settings screen). */
int pw64_split_share = 0;

/* Player 2's pilot (PILOT_*), -1 = fly player 1's. Only player 2's vehicle
 * load and the level's model list read it (game.c.patch, level.c.patch). */
int pw64_p2_pilot = -1;
