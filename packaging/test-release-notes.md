## 8BitDo 64 controller test build 2 (not a normal release)

This is **1.5.1 plus a fix for the 8BitDo 64 in DInput ("D") mode**, and the
controller log from test build 1. Nothing else changed.

**What to check (Windows):**

1. Download `Birdman64-windows-x64.zip` and unzip it.
2. Set the controller's back switch to **D** (DInput), plug it in and start
   the game.
3. C-up, C-down, C-left and C-right should now act as the matching C buttons
   (also in the Controls menu), and pressing R should no longer leave C-up
   stuck.
4. With the switch on **S** (Switch), the controller's input is ignored and a
   message suggests moving the switch to D.

If something is still off, create an **empty file named `pad-debug.txt`** next
to `Birdman64.exe` before starting, reproduce it, and attach the file to the
issue.
