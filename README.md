# VSBlox

Roblox Studio as a panel inside VS Code. Windows-only prototype.

## How it works

1. The extension opens a dark panel with tiny 4px marker squares in its corners.
2. `helper/` (Rust) captures the VS Code window on a background thread, finds the
   corners, and pins the real Roblox Studio window exactly on top of the panel.
   VS Code moves are picked up instantly through a window-event hook, so Studio
   follows drags without waiting for the next capture. Studio is made an *owned*
   window of VS Code, so it stays above VS Code and minimizes with it.
3. When the panel is hidden, minimized, or covered (command palette, menus),
   Studio is hidden until the panel is clear again.
4. **VSBlox: Set Roblox Studio Size** (or the `vsblox.studioSize` setting) picks
   `fit` or a fixed size like `1920x1080`. A fixed size is centered in the panel.
   Whatever doesn't fit is cut off at the panel edge, which also covers Studio's
   minimum window size.
5. Studio's Windows title bar is hidden by lining up the area below it with the
   panel and clipping the title bar and borders off (`vsblox.hideTitleBar`).
   Studio's own menus stay. Nothing about the Studio window is restyled.
6. Closing the panel puts Studio back where it was.

Nothing inside Studio is modified. It is only moved, resized and shown/hidden.

## Try it

1. Open this folder in VS Code and press **F5**. This builds the helper and opens
   an Extension Development Host window.
2. In that window run **VSBlox: Open Roblox Studio Panel** from the command palette.
3. Roblox Studio launches automatically if it isn't running (turn off with
   `vsblox.autoLaunchStudio`). An already-open Studio is docked as is.

Package and install it in your normal VS Code:

    node extension/scripts/package.js
    code --install-extension extension/vsblox-0.0.1.vsix --force

Debug the helper without touching any windows:

    helper/target/release/vsblox-helper.exe --probe --host-exe Code.exe

Set `VSBLOX_DUMP` to a folder to also save each capture as a .bmp.

## Known limits

- One VS Code window with the panel at a time (all windows share one marker color).
- Dragging Studio by its title bar moves it away briefly; it snaps back.
- The helper captures VS Code ~10 times a second on a background thread to track
  the panel's position inside the window (~26 ms per capture on a 1440p window).
- macOS: not yet. The plan is a `mac.rs` next to `win.rs` using the
  Accessibility API (`AXUIElement`) to move Studio and ScreenCaptureKit to find the marker.
