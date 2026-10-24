## Controller test build 3 (not a normal release)

This is **1.5.1 plus the 8BitDo 64 (DInput) fix and a fix for the Switch Online
N64 controller's C buttons**, and the controller log. Nothing else changed.

**What to check (Windows):**

1. Download `Birdman64-windows-x64.zip` and unzip it.
2. Plug in the controller and start a mission.
3. With the **8BitDo 64 on D** or a **real Switch Online N64 controller**,
   C-up, C-down, C-left and C-right should act as the matching C buttons
   (also in the Controls menu), and pressing R should not leave the camera
   stuck.
4. The "Switch mode isn't supported" message from test 2 is gone: pads are no
   longer ignored. The 8BitDo on **S** will still send phantom presses, so use D.

If something is still off, create an **empty file named `pad-debug.txt`** next
to `Birdman64.exe` before starting, reproduce it, and attach the file to the
issue.
