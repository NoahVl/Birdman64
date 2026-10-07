/*
 * Split-screen layout (splitscreen.md): with the flag on, a hang glider
 * test runs two pilots (pw64_players.c, game.c.patch): kernel channel 0 =
 * player 1's camera (top half), channel 1 = player 2's (bottom half).
 * Viewport/frustum per camera: pw64_split_viewport (camera.c.patch).
 */
int pw64_split = 0; /* 0 off, 1 top/bottom (left/right comes later) */
