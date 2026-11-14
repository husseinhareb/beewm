# Diagnosing low in-game FPS under beewm

Symptom: a game that runs at full speed under other Wayland compositors is
stuck at ~20–30 FPS under beewm, with GPU utilisation also stuck low
(~20–30 %). Low FPS *and* low GPU means the game is **idle / blocked most of
the time**, not GPU-bound — so the throttle is on the compositor side, not the
game.

This document explains the two fixes beewm ships for this and the
instrumentation you can turn on to *prove* where any remaining bottleneck is on
your own hardware.

## The main fix: dmabuf feedback (clients were using shm)

The biggest cause, confirmed from real captures, was that clients — including
XWayland/glamor, which is how Steam/Proton games render — were falling back to
**shared-memory (shm) buffers** instead of GPU **dmabuf** buffers. Every frame
was a CPU buffer uploaded to the GPU, which caps the frame rate and keeps GPU
utilisation low. In the logs this shows up as `dmabuf=0 shm=N` on every commit
and a `beewm::dmabuf` warning.

The reason: beewm advertised only the **legacy v3** `wp_linux_dmabuf` global
(`DmabufState::create_global`). Modern Mesa EGL and XWayland use the dmabuf
**feedback** protocol (v4) to discover the render device and the
format/modifier tranches the compositor can import; without it they cannot
negotiate a shared buffer and silently drop to shm.

beewm now advertises the global **with default feedback**
(`create_global_with_default_feedback`), built from the GPU's **render node**
device id and the formats the GL renderer can actually import. That puts clients
back on the dmabuf fast path (and is also what makes direct scan-out of
fullscreen games possible). Startup logs the result:

```
beewm::dmabuf: advertising dmabuf with default feedback (v4) … main_device=… formats=N
```

If you ever see `formats=0` or a fallback warning here, dmabuf import is broken
at the EGL level and clients will still use shm.

## The second fix: pipelined frame callbacks

A Wayland client (and XWayland, which games usually run through) decides when to
render its next frame from the `wl_surface.frame` callback the compositor sends.

beewm used to send that callback **from the vblank handler** — i.e. only once
the previous frame was already on screen. That couples the client's render and
the compositor's composite into the *single* refresh interval that follows each
vblank:

```
vblank ──▶ send callback ──▶ client renders ──▶ commit ──▶ composite ──▶ queue ──▶ (next vblank)
          └──────────────── must all fit in one refresh interval ───────────────┘
```

If that chain doesn't fit in one interval (very easy at 144/240 Hz, or under
any per-frame hiccup), the frame slips to the *next* vblank and the rate halves,
thirds, etc. — exactly the 20–30 FPS symptom.

beewm now sends the frame callback **as soon as the frame is queued for
scan-out** (default) and flushes it immediately, so the client renders its next
frame *in parallel* with the current one being displayed, and the per-vblank
work is just compositing an already-committed buffer:

```
vblank ──▶ composite already-committed frame ──▶ queue ──▶ send callback
                                                              └─▶ client renders during scan-out
```

`wp_presentation` feedback is still reported at the *real* vblank, so timing
reported to clients stays accurate.

### A/B proving the fix

The old behaviour is preserved behind an environment variable so you can
measure the difference on the *same* build:

```sh
# Fix ON (default): pipelined callbacks
beewm

# Fix OFF: legacy at-vblank callbacks
BEEWM_FRAME_CALLBACK_AT_VBLANK=1 beewm
```

Run the same game both ways and compare the in-game FPS counter and the
`beewm::presentation present_fps` log (below). If the fix is what restored the
frame rate, the default run will present at the monitor refresh rate and the
`AT_VBLANK` run will cap low.

## Turning on the instrumentation

All diagnostics are emitted on dedicated tracing targets and rolled up into
1–2 s windows, so they stay readable even at high refresh rates. Enable the ones
you need with `RUST_LOG`:

```sh
RUST_LOG="\
beewm::presentation=info,\
beewm::frame=info,\
beewm::commit=info,\
beewm::dmabuf=warn,\
beewm::sync=info" beewm 2>&1 | tee /tmp/beewm-fps.log
```

(Everything else stays at its normal level; add `,warn` at the front if you
also want general warnings.)

## Reading the logs

### `beewm::presentation` — is the compositor presenting at refresh rate?

On startup you get the selected mode once:

```
selected output mode width=2560 height=1440 refresh_mhz=144000 refresh_hz=144 frame_interval_us=6944
```

Confirm `refresh_hz` matches your monitor and `frame_interval_us` is what you
expect (≈16666 @60 Hz, ≈6944 @144 Hz). A wrong value here mis-paces every
client. Then, every ~2 s while frames are flowing:

```
present cadence over 2.00s present_fps=143.9 vblanks=288 expected_hz=144 avg_interval_us=6948 min_interval_us=6900 max_interval_us=7100 feedback_presented=288
```

- `present_fps ≈ expected_hz` → the compositor *is* putting frames on screen at
  the refresh rate; any low in-game number is then the game's own cap, not
  beewm.
- `present_fps` stuck at ~25 with `avg_interval_us` ≈ 2–3× the expected interval
  → the compositor is only queuing a flip every 2–3 vblanks. Pair this with the
  `beewm::commit` latency below to see why.

### `beewm::commit` — client commit rate and responsiveness

```
surface commit rate surface=12 commits=144 commits_per_s=143.8 shm=0 dmabuf=144 cb_to_commit_avg_us=1200 cb_to_commit_max_us=4300
```

- `commits_per_s` is the client's actual frame rate. Compare against
  `present_fps`.
- `cb_to_commit_avg_us` is **the key discriminator**: how long after beewm
  invited the client to draw it actually committed.
  - **Small** (a few ms) but `commits_per_s` is low → the client responds
    instantly, so the cap is on the compositor side (pacing/scheduling). The
    pipelining fix targets exactly this.
  - **Large** (≳ one refresh interval) → the client / its GPU / its buffer
    release is the bottleneck, not beewm. Check `beewm::sync` and `dmabuf`.
- `shm` should be `0` for a game. If `shm > 0` you also get a `beewm::dmabuf`
  warning (next section).

### `beewm::dmabuf` — did the client fall back to software buffers?

```
surface committed shm buffers — not on the dmabuf fast path surface=12 shm=144 dmabuf=0
```

If you see this for a game, it is uploading a CPU buffer to the GPU every frame
— that alone wrecks performance. Causes to check: `BEEWM_NO_DMABUF` set, the
`wp_linux_dmabuf` global not advertised, or a format/modifier mismatch. A
healthy game shows `dmabuf=<fps>`, `shm=0` and produces *no* `beewm::dmabuf`
line.

### `beewm::sync` — are explicit-sync fences stalling the client?

On startup:

```
explicit sync enabled (linux-drm-syncobj-v1 with eventfd waits)
```

Then while a client uses explicit sync:

```
explicit-sync fence activity over 2.00s installed=288 cleared=288 pending=0 avg_wait_us=900 max_wait_us=3000
```

- `pending` should hover near 0. If it grows without bound, acquire fences are
  never signalling and clients are wedged.
- `avg_wait_us` is roughly the client's GPU render time we wait on; a few ms is
  normal. Tens of ms means fence signalling is slow.

To rule explicit sync out entirely, restart with `BEEWM_NO_EXPLICIT_SYNC=1`. If
that fixes the FPS, the problem is in the explicit-sync path.

### `beewm::frame` — direct scan-out vs GPU composition

One line is logged on every transition between the two primary-plane paths:

```
primary-plane path changed: DIRECT SCANOUT is_scanout=true fullscreen_active=true fullscreen_is_x11=true overlay_count=0 cursor_plane_used=1 ...
```

and a summary every ~1 s:

```
frame-stats over 1.00s fps=144 scanout=144 composition=0 empty=0 ... fullscreen_active=true fullscreen_is_x11=true
```

- A fullscreen game should be `DIRECT SCANOUT` (`scanout` ≈ `frames`). If it is
  stuck on `GPU COMPOSITION`, something on top of it is preventing primary-plane
  promotion — check `count_borders` / `count_layers_above` / `overlay_count` in
  the transition line (all should be 0 for a clean fullscreen game; the cursor
  is allowed because it lives on its own plane).
- `fullscreen_is_x11` tells you whether the game is going through XWayland.
- High `avg_render_us` would indicate the compositor's own composite is slow
  (it should not be for a single fullscreen quad).

## Quick decision tree

1. `present_fps` ≈ refresh and game still low → game's own cap (not beewm).
2. `present_fps` low + `cb_to_commit_avg_us` small → compositor pacing; confirm
   the fix with the `BEEWM_FRAME_CALLBACK_AT_VBLANK` A/B test.
3. `present_fps` low + `cb_to_commit_avg_us` large → client/GPU side; check
   `beewm::sync` (`pending` rising, large `avg_wait_us`) and `beewm::dmabuf`
   (shm fallback).
4. Game on `GPU COMPOSITION` instead of `DIRECT SCANOUT` → see the `beewm::frame`
   transition line for what is blocking promotion.

## Notes for maintainers

- The pacing change trades up to one refresh interval of extra latency for
  throughput robustness; this is the standard high-throughput compositor model
  and is the right trade for a WM that runs games. `BEEWM_FRAME_CALLBACK_AT_VBLANK`
  exists for comparison and can be removed once the behaviour is settled.
- The stat-aggregation helpers (`CommitTracker`, `PresentStats`, `SyncStats`)
  live in `src/compositor/diagnostics.rs` and are unit-tested. The pacing itself
  lives in `render_frame` / the vblank handler in
  `src/compositor/backend/udev.rs` and needs real DRM/KMS hardware to exercise,
  which is why the logs above are the verification path.
```
