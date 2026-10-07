/*
 * Split-screen spike (splitscreen.md): with the flag on, the flight view is
 * drawn twice per frame — kernel channel 0 = player 1's normal camera
 * (top half), channel 1 = a rear view from the same spot (bottom half).
 * Proves two channels, two viewports and two world passes fit the
 * display-list budget (PW64_DL_LEN). No second pilot or HUD yet.
 * Viewport/frustum and the rear-view pass live in camera.c.patch.
 */
int pw64_split = 0; /* 0 off, 1 top/bottom (left/right comes later) */
