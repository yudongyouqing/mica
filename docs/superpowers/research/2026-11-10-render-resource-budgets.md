# Render resource budgets and texture reuse (#106)

## Scope and measured defaults

The limits below govern owned glyph resources, not total Mica Private Bytes,
DirectWrite internals, instance buffers, background images, or driver overhead.
A pane's terminal and strip routers share one atlas identity and bitmap budget.
Router caches have independent local ceilings and shared process ceilings.

| Resource | Local ceiling | Process ceiling | Accounting |
| --- | --- | --- | --- |
| CPU atlas bitmap | 16 MiB per atlas | 128 MiB | `Vec<u8>::capacity()`; reserve before growth, exact-reserve instead of geometric capacity doubling |
| Bitmap deduplication index | 2 MiB per atlas | 16 MiB | Sorted `Vec<AtlasEntry>` actual capacity × entry size; collision candidates compare complete pixel rows |
| GPU atlas images | 32 MiB per renderer/window | 128 MiB | Width × height × RGBA8; includes 4-byte fallback and retired/in-flight textures until submission completion |
| Shaping cache | 2 MiB / 1,024 entries per router | 16 MiB | Owned text/result capacities plus conservative bucket/entry allowance; LRU |
| Character/miss cache | 1 MiB / 8,192 entries per router | 32 MiB | 512 bytes charged per entry, covering inline geometry, buckets/control bytes and retained spare table capacity; byte ceiling usually wins |
| Active + retiring GPU contexts | 32 across all windows | 32 | Constructor acquires a slot; close transfers it to the bounded retirement service |

The initial bitmap remains 512 × 512 (1 MiB); growth adds 256-pixel pages.
The atlas height is capped at min(8,192, the actual device limit), and allocation
arithmetic is checked. Normal append-only growth never moves existing UVs.
The 16 MiB pane limit corresponds to 512 × 8,192 × RGBA8.

Before implementation, a repeatable Windows DirectWrite probe rendered 2,000
CJK characters at 12 pt using Consolas / Microsoft YaHei / Segoe UI Emoji.
One router/atlas pair owned 2,621,440 bitmap bytes (2.5 MiB); 40 retained
router/atlas pairs owned 104,857,600 bytes (100 MiB), taking 4.617 s on this host.
This models pane font payload only; it does not open 40 GUI panes or PTYs. Ligature probing was
negative for that chain, so this workload alone says nothing about shaping
cache memory. The 128 MiB process bitmap default has 28 MiB headroom over this
specific 40-pane synthetic sample; production session admission has its own
lower limit. This is a measured starting point, not a general memory target.

## Identity and invalidation contract

- `identity` is allocated once per real atlas, never inferred from pane IDs,
  pointer reuse, equal revisions, or equal dimensions. Grid and strip share it.
- The sorted bitmap index is bounded separately, charged by actual Vec capacity,
  and reset with its generation. LRU eviction of routing/shaping entries cannot
  append identical bitmaps again. Hashes only narrow candidates; dimensions and
  every pixel row must match before reusing UVs. Metadata exhaustion follows the
  same visible fallback/recovery contract as pixel exhaustion.
- `revision` tracks content changes, including insertion and append-only growth.
  A bounded 256-event dirty journal supports independent renderer cursors.
  Expired journal history falls back to one full upload for that consumer.
- `version` is the layout generation. It changes only in `prepare_frame`, before
  grid, strip or overlay instances are built. Every router clears character and
  shaping caches on generation mismatch; app/QT force all row instances to rebuild.
  Overlay/strip instances are rebuilt afterward in the same frame.
- A full atlas keeps existing UVs and returns a pinned, visible question mark.
  The next frame gets one recovery attempt. Unchanged oversized working sets
  do not reset every frame. Changed visible grid/UI content retries; process
  quota retries require enough newly available bytes for the previously denied
  page. Unrelated release epochs cannot make two panes trigger each other.
- The focused pane's recovery key includes shared title/search/rename/hover text,
  modes and exit state. The QT key distinguishes no exit from a successful exit.
- Oversized or malformed bitmaps cannot trigger infinite growth/panics. DWrite
  raster dimensions/area and retained color-layer coverage are bounded before
  large allocations. Shaping inputs beyond 4,096 characters route individually.
- Missing initial CPU/GPU quota propagates through pane/QT/window creation error
  paths. Existing sessions continue; ordinary quota refusal does not unwind an
  extern Win32 window procedure.

## GPU lifecycle and upload behavior

Each renderer keeps an identity LRU. Same-size additions reuse texture, sampler
and bind group, then call `Queue::write_texture` on the changed rectangle.
Growth creates a larger texture, copies the old image on the GPU and uploads new
ink; newly allocated texture pixels are already zero-initialized by wgpu. Device
replacement starts a new renderer cache, so each device uploads its own copy.
Background-resource changes invalidate cached bindings while retaining images.

CPU aliases to the active texture/binding are removed before eviction. Both
window/process texture leases stay in the `on_submitted_work_done` retirement
closure until the last submitted GPU use completes; the closure then destroys
the texture and returns the leases. Pending resources count toward the same
budget, preventing unlimited create/evict loops in one frame. Pressure polls
nonblockingly and uses visible foreground replacement blocks until a later frame
can allocate. Blank/background quads retain their normal colors via `ink_mask=0`.
Resource diagnostics are limited to once per five seconds per atlas/renderer.

The Win32 driver-shell `forget` workaround remains. Before forgetting the shell,
main window and QT explicitly release atlas resources. One process-wide retirement
worker polls these devices. A 32-slot permit acquired during renderer construction
covers active + queued + polling contexts, including the worker's vector and
bounded channel. Thread creation failure returns an ordinary constructor error.
A failed enqueue retains the item *and its slot*, yielding a maximum of 32 bounded
fault records rather than releasing admission early or blocking a window thread.
A permanently hung device can retain its slots/resources; new admission then
fails visibly instead of growing the retirement queue without bound.

GPU byte figures are RGBA image payload, not a promise about physical VRAM.
Backend allocation granularity, staging alignment and delayed driver destruction
remain outside this accounting. Temporary DWrite buffers and caller-owned row/
instance output are separate from retained bitmap/cache budgets. Coverage area
is capped at 512² pixels, total retained color-layer coverage at 512² bytes,
color layers at 64, and the color canvas at 512² RGBA-f32 pixels. These structural
caps avoid arbitrary single-glyph allocations; no total process-memory or soak
claim follows from the retained-resource ceilings.

## Verification and measurements

Initial red tests observed atlas height 34,304 after 10,000 40×40 inserts,
2,048 retained unique shaping keys, and a second upload for the identical atlas.
All three fail for the intended missing policy on the old implementation.

Real wgpu device paths measured these net uploaded texel payloads:

| Replay | Upload payload | New atlas textures | Result |
| --- | ---: | ---: | --- |
| Two distinct 256² atlases with equal revision | 524,288 B | 2 | Both identities retained; red/blue pixel readback correct |
| Add 6×9 glyph without resize | 216 B | 0 | Existing texture and bind group retained |
| Grow then insert 8×250 glyph | 8,000 B | 1 | Old pixels copied on GPU, existing pixel UVs retained |
| 40 alternating warm atlas bindings | 0 B | 0 | No bind-group creation either |
| Second renderer receives later 2×2 insertion | 16 B | 0 | Independent dirty cursor, no globally cleared dirty state |

`AtlasStats.upload_bytes` sums rectangles from the actual `write_texture` calls;
it excludes wgpu/backend padding and is not a PCIe bus measurement. The replay
reported 532,504 B / 3 atlas creations / 3 bind-group creations cumulatively,
with 1,048,580 owned GPU image bytes including the fallback.

Regression coverage includes equal-revision identities, per-pane scissor/pixel
routing, green fallback ink on black backgrounds, local/process budget fill and
release, unchanged-revision recovery, expired dirty history, short/long unique
shaping runs, LRU recency and byte pressure, shared strip/grid generation,
lower-device-limit and DPI rebuild identity, and two static oversized panes.
The 32-context retirement test also models an incomplete real-device retirement
job. Charging zero constructor slots instead of one makes its admission assertion
fail (exit 101); restoring the charge passes. Omitting shared UI from the key also
makes the hover-only recovery assertion fail (exit 101), then passes on restoration.
A separate red test proved identical bitmap insertion formerly returned new UVs;
after the fix 10,000 repetitions retain one rect/revision, forced hash collisions
preserve distinct pixels, and metadata capacity is released on generation/Drop.
The metadata regression also holds a live atlas at quota denial, releases a
second atlas, then verifies recovery with the original unchanged working-set key.

The post-change isolated DirectWrite replay retained the same 104,857,600 bitmap
bytes. A PowerShell parent sampled the probe child `PrivateMemorySize64` every
20 ms: 44 samples, peak 120,336,384 B (114.76 MiB), successful exit, 1.433 s inside
the test / 1.596 s wall time. These are measurements of this probe process, not
of the user's Mica process. Font/system caches and concurrent machine load were
not controlled between runs, so elapsed values are not a speedup claim. Bitmap,
index and cache budgets are retained-resource ceilings, not a 128 MiB limit on
total Private Bytes.

At this revision, `cargo test -j2 -p mica-render --lib -- --test-threads=1`
reported 75 passed / 2 ignored, including real GPU pixel/upload/retirement paths.
Workspace Clippy with all targets and `-D warnings` passed. The earlier full
workspace run passed app units, crash/input/IPC/window lifecycle integrations and
core units, then stopped at the pre-existing PowerShell OSC133 canary's echoed
marker early return. That test correction belongs to #107/#114, not this fix.
Final combined gate results will be appended after synchronizing that correction.
Full 8-hour soak and physical driver-memory profiling are not performed by these
short tests.
