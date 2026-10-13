/*
 * Split-screen pilot collisions (splitscreen.md "Interactions toggle"):
 * 1 = the two pilots can crash into each other (a sphere test feeding the
 * existing crash path), 0 = they pass through. The game reads it at each
 * test start; Rust may set it at any time (also live from the settings
 * screen's Controls page).
 */
int pw64_split_collide = 0; /* 1: split-screen pilots collide (read at test start) */
