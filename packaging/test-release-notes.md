## 8BitDo 64 controller test build (not a normal release)

This is **1.5.1 plus a controller log**, to track down the 8BitDo 64 problems
(C-buttons, Switch mode, ZR). Nothing else changed.

**How to use it (Windows):**

1. Download `Birdman64-windows-x64.zip` and unzip it.
2. Create an **empty file named `pad-debug.txt`** next to `Birdman64.exe`
   (portable mode: the same folder as `portable.txt` / `pw64.toml`).
3. Plug in the 8BitDo 64, start the game and open a mission.
4. Press **slowly, one at a time, with a pause between**: C-up, C-down,
   C-left, C-right, then R (ZR), then the left stick in all four directions.
   Do the same in the other mode (DInput / Switch) after restarting the game.
5. Close the game and attach `pad-debug.txt` to the issue, saying which mode
   (DInput or Switch) each run was in. Delete the file between modes, or
   write down which part of the log is which.

The log lists the controller's name, ids and mapping, every raw button/axis
event, and what the game receives. It contains no personal data.
