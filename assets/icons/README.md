# Project Icons

These icons are project-owned bitmap assets generated for OpenAI Codex Proxy.
They are not Freedesktop theme icon names, desktop-environment assets, or
operating-system-specific resources.

## Files

- `openai-codex-proxy-launcher.png`: transparent source PNG for launcher-style
  packaging assets.
- `openai-codex-proxy-tray.png`: transparent source PNG for tray-style assets.
- `source/*-chromakey.png`: original generated images on flat chroma-key
  backgrounds, retained as regeneration references.
- `hicolor/*/apps/openai-codex-proxy.png`: launcher icons copied into Linux
  AppImage metadata.
- `tray/*/openai-codex-proxy-tray-*.png`: tray icon state variants for stopped,
  starting, running, and failed states.
- `tray/raw/*/*.argb`: ARGB pixmaps embedded into the Linux StatusNotifier tray
  implementation with `include_bytes!`.
- `menu/*.png`: menu action icons embedded into the Linux tray menu.
- `*-preview.png`: local preview sheets used to inspect the generated outputs.

## Generation Notes

The source images were generated with the built-in image generation tool on a
flat removable chroma-key background. The transparent PNGs were created with the
local imagegen chroma-key removal helper, then resized into the runtime and
packaging variants.
