# Titlebar and startup size

User request: match the current Windows Terminal window size, center the title,
improve its font using Ghostty as reference, add a restrained title highlight,
and check related layout/interaction gaps. Existing user sessions stay running.

The reference Windows Terminal window was measured with a per-monitor-v2 query:
outer 2032x1041 physical pixels, DPI 144. Add optional `window-size-pixels = w,h`
as a Mica-specific exact outer-size override; empty retains cell-based sizing.
Store the measured size in personal overrides, rather than a machine-specific
portable default. Changes apply to new windows, not the current session.

Use `window-title-font-family` (aliasing existing `strip-font-family`) with Segoe
UI by default, plus Windows CJK/emoji/icon fallback. Terminal fonts remain grid
fonts. Add cached font advance measurements for UI text only; bitmap ink width
and the width of M are not substitutes for proportional text advances.

Title alignment is center by default, using the whole viewport center and a
symmetric safe area that excludes caption buttons. Left alignment is optional.
Ellipsis appears only when necessary. A configurable rounded title backdrop
uses the palette accent at a low intensity, dims when inactive, and can be
disabled. It remains part of the normal drag/double-click title region.

All defaults live in mica-core/config/defaults. Validate size/align/color/opacity
and radius, document restart/reload boundaries and preserve personal overrides.
Test long/CJK/empty titles, narrow windows, overflow tabs, caption exclusion,
proportional advance, DPI scaling and actual native startup dimensions. Inspect
an isolated GUI before updating installed files; never close the user's Mica.

References accessed 2026-11-14:
- https://ghostty.org/docs/config/reference#window-title-font-family
  (system UI font, explicitly not constrained to monospaced faces).
- https://learn.microsoft.com/en-us/windows/terminal/customize-settings/themes
  (separate window chrome and focused/unfocused appearance).

The centered highlighted title is the user's chosen Mica design, not a claim
that every terminal or operating system uses the same titlebar arrangement.
