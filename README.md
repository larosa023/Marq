# Marq

A hyper-lightweight screen snipping tool for Windows. It lives in the system tray, and every snip goes straight to your clipboard.

## Install (one time)

1. Download **Marq.exe** from the [latest release](https://github.com/larosa023/Marq/releases/latest).
2. Put it in a folder you own, for example `Documents\Marq`. Avoid `Program Files`, because Marq can't update itself there.
3. Double-click it. If Windows shows "Windows protected your PC", click **More info**, then **Run anyway**.
4. Optional: right-click the tray icon and turn on **Start with Windows**.

## Use

| Action | How |
| --- | --- |
| Start a snip | **Win+Shift+S** (changeable), or click the tray icon |
| Region | Drag a box |
| Window | Press **W** (or click *Window*), then click a window |
| Full screen | Press **S** (or click *Screen*), then click a monitor |
| Settings | Click the **gear** in the snip toolbar, or right-click the tray icon → *Settings* |
| Cancel | **Esc** or right-click |

**Changing the shortcut:** open Settings, click **Change**, and press the keys you want (for example Ctrl+Shift+S or PrtScn). Only that one shortcut is active; **Reset** puts it back to Win+Shift+S. Settings also has **Start with Windows**.

Marq takes over its shortcut before Windows sees it (the same way PowerToys remaps keys), so Win+Shift+S works whether or not Microsoft's Snipping Tool is installed. One limitation: while an app running as administrator is focused, Windows doesn't pass shortcuts to Marq; use the tray icon there.

The snip is copied to the clipboard immediately and saved as a PNG to **Pictures › Screenshots**, the same folder Windows uses for screenshots, so every snip shows up in File Explorer's **Gallery** and the Photos app. A preview card appears in the corner; click it to mark up. Markup is saved back to that same file. You can turn saving off, or open the folder, in Settings; the tray menu also has *Open Screenshots folder*.

**Markup:** Pen (**P**), Highlighter (**H**), Arrow (**A**), colors **1–5**, Undo (**Ctrl+Z**), close (**Esc**). Your marked-up image is re-copied to the clipboard automatically a moment after each change, and again when you close the window.

## Updates

Marq checks GitHub for new versions shortly after starting and every few hours. When one is out, you get a notification. Click it, or right-click the tray icon and choose **Update to vX**. Marq downloads the new version, verifies it, swaps itself and restarts. You never need to download a new exe by hand.

## Publishing a new version

1. Make your changes.
2. Raise `version` in `Cargo.toml`, for example `0.1.0` → `0.1.1`. You can do this right on GitHub with the pencil icon.
3. Commit to `main`.

GitHub Actions builds Marq on Windows and publishes the release with `Marq.exe` and `latest.txt` (version and SHA-256). Running copies pick it up automatically. Pushes that don't change the version are just build-checked.
