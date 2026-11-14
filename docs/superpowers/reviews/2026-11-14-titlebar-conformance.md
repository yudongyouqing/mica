# Titlebar and startup size verification

User chose the current Windows Terminal window as the size reference. A read-only
PMv2 query measured outer 2032x1041 physical pixels at DPI 144; DPI-unaware queries
returned virtualized dimensions and were not used. Personal configuration receives
`window-size-pixels = 2032,1041`. The portable default remains cell-based.

## Reference and scope

Read the Ghostty official homepage image and configuration reference on 2026-11-14:

- https://ghostty.org/
- https://ghostty.org/docs/config/reference#window-title-font-family
- https://learn.microsoft.com/en-us/windows/terminal/customize-settings/themes

Ghostty explicitly separates UI title fonts from monospaced terminal fonts;
Windows Terminal separates focused/unfocused window chrome. Mica uses Segoe UI
with Windows CJK/emoji/icon fallback. The user's centered highlighted title is a
Mica choice, not a claim that all terminals use this arrangement. Windows caption
buttons and existing drag/double-click behavior remain in their native positions.

The exact physical-pixel override and title-highlight keys are Mica extensions.
`window-title-font-family` aliases the existing `strip-font-family`. All 49
canonical defaults remain in mica-core/config/defaults; pixel size takes effect
on new windows, title appearance supports configuration reload.

## Changes and gaps closed

- Title centering formerly used only the area left of caption buttons. Now it
  uses the entire viewport center, reserves a symmetric safe area and truncates
  before the controls. Empty/very narrow titles omit text safely.
- UI fonts formerly inherited terminal fonts and used ink width / M width as
  advance. DirectWrite design advances are now cached for proportional UI text;
  grid rendering still uses terminal cell metrics. The additional cache has a
  16 KiB / 256-entry cap charged to the existing shared character budget.
- Title backdrop is a configurable rounded accent with enable/color/opacity/
  radius controls and inactive dimming. A fitting title no longer needlessly
  reserves an ellipsis; overflow tab count cannot expand the viewport width.
- A minimized HWND may report nonzero icon dimensions. A real test exposed
  background PTYs collapsing to 6x1 instead of 86x17 during reload. WM_SIZE now
  rejects SIZE_MINIMIZED before touching grids/GPU state; reload uses the cached
  real viewport while iconic. Restoration follows normal resize behavior.

## Evidence

- Before changes, configuration regression was red (new keys ignored), and
  centering regression reported bounds 212..244 in a 600-wide viewport.
- Native startup test verifies exact outer 2032x1041 using a PMv2 reader.
- UI boundary tests cover long CJK/short/empty titles, widths 160/240/600, DPI
  scales 1/1.5/2, caption exclusion, highlight disable and overflow tabs.
- Native DirectWrite test checks W/i/space advances, cached lookup and unchanged
  terminal cell metrics. Existing renderer/atlas budget regressions pass.
- Real watcher regression verifies inactive PTYs retain their size through a
  minimized appearance reload; the old nonzero-icon path was observed failing.
- Workspace: 366 passed, five ignored. Workspace/all-target clippy with
  `-D warnings`, fmt and diff checks pass. Final independent review has no blockers.

An isolated native preview could not be captured correctly behind the foreground
game. It was cleaned up without sending input to the game. Instead the checked
`titlebar_preview` example exported a PNG using actual DirectWrite glyphs and
the actual wgpu renderer, with focused single/multi-tab, long-title and inactive
states; that artifact was visually inspected. This is a renderer export, not a
desktop screenshot or a claim of full multi-monitor visual certification.

Reproduce without opening a window:

```powershell
cargo run -p mica-render --example titlebar_preview -- output.png
```
