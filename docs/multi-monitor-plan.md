# beewm Multi-Monitor & Hotplug — Implementation Plan

Status: in progress — **Phase 0 ✅**, **Phase 1 ✅**, **Phase 2 ✅**, **Phase 3 ✅**,
**Phase 4 ▶ partial** (config stanza + output naming done & tested;
`wlr-output-management` protocol deferred). 2B/3 and the Phase-4 apply path are
gated and **hardware-untested**. Next: on-hardware validation, then
`wlr-output-management`.
Scope: full multi-output support (static multi-head, runtime hotplug, per-output
workspaces, window migration, output-management protocols).

## Progress log
- **Phase 0** — output registry (`OutputCtx`, `Beewm::add_output`), resolvers
  (`focused_output`, `output_for_window`, `output_for_surface`,
  `output_under_point` in `src/compositor/state/output.rs`), all 19 single-output
  `outputs().next()` sites routed through resolvers, `Workspace.output` seeded.
  No behavior change on one monitor.
- **Phase 1** — per-output active workspace: `OutputCtx.active_workspace` is
  authoritative; global `Beewm::active_workspace` field replaced by an
  `active_workspace()`/`set_active_workspace()` accessor pair. i3 `switch_workspace`
  semantics via pure `plan_workspace_switch`; `focus_output_in_direction` /
  `move_window_to_output` + `FocusOutput`/`MoveWindowToOutput` actions and
  `focus_output_*` / `move_to_output_*` config keywords. 8 new pure-function unit
  tests.
- **Phase 2** — multi-head backend, in two parts:
  - **2A relayout split**: `relayout` → `relayout_all` (loops outputs) +
    `relayout_output(&Output)`; `tiling_usable_geometry_for`/
    `floating_usable_rect_for`/`remap_floating_windows_for` parametrized by
    output/workspace. Behavior-identical on one output; tested.
  - **2B DRM backend**: `GpuData` now holds `Vec<SurfaceData>` (one
    `DrmCompositor` + `Output` per connected CRTC, sharing the device's single
    `GlesRenderer`). `init_gpu` enumerates connectors, packs outputs
    left-to-right, routes vblank by CRTC, renders each surface; session
    pause/resume and `add_output` iterate surfaces. **Gated on
    `BEEWM_MULTI_OUTPUT`** — default drives only the first connector, structurally
    equivalent to before. ⚠️ The multi-surface path is **not yet validated on real
    dual-monitor hardware** (no DRM in the dev env); needs an on-hardware smoke
    test of scan-out/FPS before the gate is flipped to default.
    Deferred to later phases: per-output `needs_render` (currently one global flag
    re-renders all surfaces; idle outputs return `is_empty` so it's cheap), and
    per-output animation-suppression precision.
- **Phase 3** — hotplug, in two parts:
  - **State layer** (`state/output.rs`, tested): `Beewm::remove_output` migrates a
    removed output's workspaces to a survivor via the pure `plan_output_removal`
    (Vec-index reindex), unmaps the gone output's windows, repacks positions,
    pulls floats back on-screen (`reclamp_floating_windows` — also fixes review
    bug **B4**), refocuses, relays out. Zero-output interval keeps everything in
    memory. `handle_output_geometry_changed` handles mode/scale changes.
    `recompute_output_positions` repacks left-to-right. 3 new pure unit tests.
  - **Backend** (`udev.rs`): `SurfaceData` gains its `connector`; `GpuData` retains
    `drm_fd` + device resources (`renderer_formats`/`color_formats`/`cursor_size`)
    so surfaces can be built at runtime. `build_surface_for_connector` is extracted
    and shared by initial enumeration and `rescan_connectors`, which diffs the live
    connector set on `UdevEvent::Changed`: builds a surface (+ `add_output`) for
    each newly-connected display and tears down (+ `remove_output`) any vanished
    one. **Gated on `BEEWM_MULTI_OUTPUT`**; default still log-only. ⚠️ Runtime
    surface create/teardown is **not validated on hardware** (no DRM in dev env).
    Not yet handled: multi-GPU hot-add (second GPU), mode-change-only events.
- **Phase 4 (partial)** — config-driven output management:
  - **Output naming**: outputs are now named by human-readable connector name
    (`DP-3`, `eDP-1`, `HDMI-A-1`) via `connector_name` instead of the opaque
    handle — better for bars/`wlr-randr` and required for config matching.
  - **`output <name> …` config stanza** (`config/parser.rs`, tested): parses
    `position X Y`, `mode WxH[@Hz]`, `disable|enable` into `Config.outputs`.
    Applied at enumeration **and** on hotplug (`resolve_connector_mode`,
    `config_for_output`, configured position vs auto-pack, skip disabled). 4 new
    parser tests; commented example added to the generated default config.
  - **Deferred**: `wlr-output-management-unstable-v1` (runtime `wlr-randr`/kanshi
    protocol — pure blind backend mutation), per-output `scale`/`transform` apply,
    `wlr-output-power-management` (DPMS).

---

## 0. Guiding principles

1. **Incremental and always-green.** Six phases, each independently compilable,
   testable, and shippable. Phase 0 changes *no behavior* on a single monitor; it
   only removes the structural assumption. Every later phase is additive.
2. **One source of truth per fact.** The owning output of a workspace, the active
   workspace of an output, and the focused output each live in exactly one field.
   No derived value is also stored.
3. **The layout engine does not change.** `DwindleTree` / `MasterStack` are already
   per-workspace and output-agnostic — they compute geometry against whatever
   `screen: Geometry` we pass. Multi-output = "relayout each visible workspace
   against *its* output's usable rect," not a layout rewrite.
4. **Mirror the proven Smithay topology.** The device/surface split below is the
   same shape as Smithay's `anvil` udev backend (`HashMap<DrmNode, Device>` →
   `HashMap<crtc, Surface>`). We are not inventing a backend structure.
5. **Single GPU, multiple heads first.** Phases 2–3 target one DRM device driving
   many connectors (laptop iGPU + external; desktop GPU + N monitors) — ~95% of
   users. Cross-GPU buffer offload is Phase 5.

---

## 1. Target data model

### 1.1 Output registry (compositor state)

Add to `Beewm` (`src/compositor/state/mod.rs`):

```rust
/// Per-output compositor-side state. Backend/render state (DrmCompositor,
/// renderer) is keyed separately in the backend, not here.
pub struct OutputCtx {
    pub output: Output,
    /// Index into the global `workspaces` pool currently shown on this output.
    pub active_workspace: usize,
    /// Top-left of this output in the global Space coordinate space.
    /// Mirrors what we passed to `space.map_output`.
    pub position: Point<i32, Logical>,
}

pub outputs: Vec<OutputCtx>,
/// Index into `outputs` that currently owns keyboard focus and receives
/// newly-spawned windows / `switch_workspace`. Always valid while
/// `outputs` is non-empty.
pub focused_output: usize,
```

`Output` is `Clone + Eq + Hash`, so it doubles as the stable id; we never invent a
parallel id type. `lock_surfaces: HashMap<Output, LockSurface>` already proves this
pattern works.

### 1.2 Workspace ownership

Workspaces remain a **global pool** (`Vec<Workspace>`) — this preserves the i3-style
numbered-workspace UX, the `workspace N` IPC command, and `publish_workspace_state`.
Add the owning output to each workspace:

```rust
// src/model/workspace.rs
pub struct Workspace<W = ()> {
    pub windows: Vec<W>,
    pub focused_idx: Option<usize>,
    pub fullscreen: Option<W>,
    /// The output this workspace is currently homed on. A workspace is
    /// *visible* iff `outputs[output].active_workspace` points back at it.
    pub output: usize,   // index into Beewm.outputs
}
```

Invariants:
- Every workspace has exactly one `output`.
- For each output `o`, `outputs[o].active_workspace` indexes a workspace whose
  `.output == o`.
- `active_workspace` (the old global field) is **removed**; callers use
  `self.outputs[self.focused_output].active_workspace`. A thin accessor
  `fn active_workspace(&self) -> usize` keeps the diff small.

### 1.3 Semantics (i3 model)

- **`switch_workspace(n)`**: if workspace `n` is currently visible on some output
  `O`, move keyboard focus to `O` (don't relocate the workspace). Otherwise show
  `n` on the focused output: set `workspaces[n].output = focused_output`,
  `outputs[focused_output].active_workspace = n`, hide the previously-shown
  workspace, relayout that output.
- **`move_to_workspace(n)`**: move the focused window into workspace `n` (which may
  live on another output). It becomes visible wherever `n` is shown.
- **New actions**: `FocusOutput(Direction)`, `MoveWindowToOutput(Direction)`.
  Direction resolves via output geometry centers (reuse the directional-scoring
  logic in `state/focus.rs`).

---

## 2. Backend topology (DRM/udev)

Replace the single `UdevData.gpu: Option<GpuData>` with a device → surface map.

```rust
struct UdevData {
    state: Beewm,
    display: Display<Beewm>,
    primary_node: Option<DrmNode>,
    devices: HashMap<DrmNode, DrmDeviceState>,
}

struct DrmDeviceState {
    drm: DrmDevice,
    gbm: GbmDevice<DrmDeviceFd>,
    renderer: GlesRenderer,            // one EGL context per device
    allocator: GbmAllocator<DrmDeviceFd>,
    exporter: GbmFramebufferExporter<DrmDeviceFd>,
    drm_notifier_token: RegistrationToken,
    /// One render surface per active CRTC (= per connected display).
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    /// Connector → crtc currently driving it, for hotplug diffing.
    connectors: HashMap<connector::Handle, crtc::Handle>,
}

struct SurfaceData {
    output: Output,
    compositor: DrmCompositor<GbmAllocator<DrmDeviceFd>,
                              GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>,
    can_render: bool,
    pending_presentation_feedback: Option<OutputPresentationFeedback>,
    frame_stats: FrameStats,
    present_stats: PresentStats,
}
```

Why this shape:
- `DrmCompositor` is **per-CRTC** by design — one composited scanout target per
  display. Sharing one `GlesRenderer`/EGL context across a device's surfaces is the
  normal arrangement and avoids N EGL contexts.
- VBlank events carry `(crtc)`; the handler now keys into
  `devices[node].surfaces[crtc]` instead of the single `gpu`.
- Multi-GPU is just `devices.len() > 1`; rendering an output on a non-primary GPU
  and scanning out elsewhere is the Phase 5 offload concern.

### 2.1 Render loop change (`run_udev`)

Today: one `render_frame(&mut data)` against `data.gpu`. New: iterate surfaces that
need a frame and can render:

```rust
for (node, device) in &mut data.devices {
    for (crtc, surface) in &mut device.surfaces {
        if surface.can_render && output_needs_render(&data.state, &surface.output) {
            render_surface(&mut data.state, device, crtc, surface);
        }
    }
}
```

`needs_render` becomes **per output**: replace the single `Beewm.needs_render: bool`
with a per-output dirty flag (`HashSet<Output>` of dirty outputs, or a `dirty: bool`
on `OutputCtx`). Damage from a commit dirties the output(s) that surface is mapped
on; a relayout dirties the affected output; a focus change dirties the focused
output. This avoids re-rendering an idle external monitor when only the laptop
panel changed — important for power and for not stealing the scanout fast path.

The existing render element builders already take `&Output`
(`window_render_elements(.., &output, ..)`, `layer_render_elements(.., &output, ..)`,
`send_frame_callbacks(.., &output, ..)`, `collect_presentation_feedback(.., &output, ..)`,
`update_primary_scanout_output(.., &output, ..)`) — so `render_surface` is largely
the body of today's `render_frame` with `gpu.output` → `surface.output` and
`gpu.compositor` → `surface.compositor`. **No render-element code changes in Phase 2.**

### 2.2 Startup enumeration

Replace `udev.rs:447-480` (`if data.gpu.is_none()` — first device, first connector)
with: for every udev device, `init_device`; for every device,
`scan_connectors` → create a `SurfaceData` + `Output` for **each** connected
connector with modes; position each output (see §3). `map_output` each into the
Space.

---

## 3. Output positioning

Outputs must occupy disjoint regions of the global Space coordinate space.

- **Default policy:** left-to-right in connector-handle order, packed at `y=0`:
  `x_next += output_mode.width_logical`. Deterministic and matches what most users
  expect for two side-by-side monitors.
- **Config override (Phase 4):**
  ```
  output eDP-1  position 0 0      scale 1.5
  output DP-3   position 2560 0   mode 2560x1440@165  transform normal
  output HDMI-A-1 disable
  ```
  Keyed by connector name (`Output::name()`), applied at enumeration and on hotplug.
- On any add/remove/mode-change, **recompute all positions**, `map_output` with new
  coords, update `OutputCtx.position`, then relayout every visible workspace and
  reclamp floats (ties into bug B4 from the review).

---

## 4. Resolving "which output" — the 17 call sites

Phase 0 introduces resolver methods so no call site hard-codes `outputs().next()`:

```rust
impl Beewm {
    fn focused_output(&self) -> Option<&Output>;
    fn output_ctx_mut(&mut self, output: &Output) -> Option<&mut OutputCtx>;
    fn output_for_window(&self, w: &Window) -> Option<Output>;     // via workspace.output, fallback element_geometry→output_under
    fn output_for_surface(&self, s: &WlSurface) -> Option<Output>;
    fn active_workspace_for(&self, output: &Output) -> usize;
    fn usable_geometry_for(&self, output: &Output) -> Option<Geometry>;     // layer non_exclusive_zone of *that* output
    fn floating_usable_rect_for(&self, output: &Output) -> Option<Rectangle<i32, Logical>>;
    fn output_under_point(&self, p: Point<f64, Logical>) -> Option<Output>;
}
```

Migration table (every current `outputs().next()` / single-output use):

| File:line | Current use | Phase-0 resolver |
|---|---|---|
| `state/window_lifecycle.rs:19` `centered_floating_data` | output for a new dialog | `output_for_window(window)` ?? `focused_output()` |
| `state/window_lifecycle.rs:401,414` `floating_usable_rect`/`tiling_usable_geometry` | usable rect | take `&Output` param; callers pass the window's/focused output |
| `state/window_lifecycle.rs:696` `float_window` | center a float | `output_for_window` |
| `state/window_lifecycle.rs:760` `show_fullscreen_window` | size to output | output of the window's workspace |
| `state/window_lifecycle.rs:834` `relayout` | **becomes `relayout_output(&Output)`**; a `relayout_all()` loops visible outputs | per-output |
| `handlers.rs:137` `prime_surface_scale_state` | scale hint | `output_for_surface` (Phase 5 for true per-output scale) |
| `handlers.rs:393` layer-arrange on commit | output owning the layer | layer surface's output (stored at map time) |
| `handlers.rs:551,556` `fullscreen_request` | output geometry | output of target window's workspace |
| `handlers.rs:744,763` `new_layer_surface` | which output to map layer | client-requested output ?? `focused_output()` |
| `handlers.rs:803` `layer_destroyed` | layer's output | stored layer→output |
| `state/decorations.rs:95` border vs layer overlap | output of the bordered window | `output_for_window` |
| `state/tiling.rs:118` `rectangle_covers_output` | "covers screen" test | output under the rect |
| `input/pointer.rs:58` `surface_under` | already `output_under(pos)` ?? next | keep `output_under`, drop the `next()` fallback's single-output bias |
| `input/pointer.rs:157,293` motion clamp | clamp to one output | clamp to the **output under the new pos**; allow edge crossing (§6) |
| `xwayland/wm.rs:200,204` `apply_x11_fullscreen` | output geometry | output of the X11 window's workspace |
| `winit.rs:349,406` | nested = single output | resolver returns the one winit output |
| `popup.rs:214-228` | already multi-output aware (`output_under` + fallback) | minor: prefer parent's output |

After Phase 0, **grep `outputs().next()` must return zero hits in `src/`** (except a
single definition inside `focused_output()`'s fallback).

---

## 5. Window migration (output removal)

When output `O` is removed (disconnect or GPU gone), pick a survivor `S` (the new
`focused_output`, else `outputs[0]`):

1. **Workspaces homed on `O`:** for each `w` with `workspaces[w].output == O_idx`,
   set `workspaces[w].output = S_idx`. They become *hidden* on `S` (S keeps its own
   active workspace) unless `S` had none.
2. **`O`'s active workspace:** if `S` currently shows an empty/never-used workspace,
   optionally promote `O`'s active workspace to visible on `S`; otherwise it just
   joins `S`'s hidden pool. Default: keep `S`'s active workspace; `O`'s content is
   reachable via `switch_workspace`.
3. **Floating windows** whose stored position lay inside `O`'s old region: translate
   into `S`'s usable rect and clamp (reuse the B4 reclamp routine). Tiled windows
   need no translation — they're repositioned by the relayout.
4. **Focus:** if `focused_output == O`, set it to `S` and refocus `S`'s active
   workspace's focused window.
5. **`lock_surfaces.remove(&O)`**; if locked and `S` lacks a lock surface it renders
   solid black (existing behavior).
6. Drop the `SurfaceData`, `space.unmap_output(&O)`, remove from `outputs`,
   recompute positions, `relayout_all()`.

**Zero-output interval** (lid closed, everything gone): keep all state in memory,
render nothing, no panics (the resolvers return `None` and callers no-op). On
reconnect, re-enumerate and remap — workspaces and windows survive because they were
never destroyed, only unmapped from the Space.

---

## 6. Input across outputs

- **Pointer motion** (`input/pointer.rs:153-228`, `:289-355`): today clamps `new_pos`
  to `outputs().next()` geometry. New: compute the global bounding region; clamp to
  the **output under the cursor**, and when the pointer crosses a shared edge into a
  neighbor output's region, hand off to that output (clamp to the neighbor instead).
  Practically: find `output_under_point(new_pos)`; if `None` (gap between mismatched
  resolutions), clamp back to the previous output's nearest edge. This gives smooth
  multi-monitor pointer traversal without a cursor falling into a dead zone.
- **Focus-follows-mouse** already keys off the surface under the pointer; once motion
  spans outputs, crossing into another output's window updates `focused_output` to
  that window's output. Add: update `focused_output` whenever keyboard focus moves to
  a window on a different output.
- **Pointer warp (Phase 5, optional):** on `FocusOutput`, warp the cursor to the
  newly focused output's center for discoverability (niri/sway behavior).

---

## 7. Protocols & config (Phase 4)

- **`wlr-output-management-unstable-v1`** (`OutputManagementManagerState`): lets
  `wlr-randr`, `kanshi`, `wdisplays` enumerate and configure heads. Apply requested
  position/mode/scale/transform/enabled through the same `apply_output_config` path
  used at enumeration. This is the single most-requested multi-monitor tool API.
- **`wlr-output-power-management-unstable-v1`**: DPMS off/on per output (pairs with
  the idle-notify work from the main review).
- **`output` config stanza** (§3) parsed in `config/parser.rs`; hot-reloadable via
  the existing `apply_config_reload` (recompute positions + relayout).
- **`xdg-output`** is already advertised
  (`OutputManagerState::new_with_xdg_output`) — verify per-output logical
  geometry/name is emitted for each output (bars rely on it).

---

## 8. Phase breakdown (deliverables & exit criteria)

**Phase 0 — De-assume one output (no behavior change).**
- Add `OutputCtx`, `Beewm.outputs`, `focused_output`, `Workspace.output`.
- Add all resolver methods (§4). Rewrite the 17 sites.
- `relayout` → `relayout_output` + `relayout_all`.
- Exit: single monitor behaves identically; `grep outputs().next()` ≈ 0;
  new headless tests for resolvers pass.

**Phase 1 — Per-output workspace model.**
- Remove global `active_workspace`; implement i3 `switch_workspace`/`move_to_workspace`
  semantics; add `FocusOutput`/`MoveWindowToOutput` actions + config keywords.
- Per-output `dirty`/needs_render.
- Exit: with a single output, all existing workspace tests pass; new model tests
  (two synthetic outputs in the headless harness) pass.

**Phase 2 — Static multi-head backend.**
- Refactor `GpuData` → `DrmDeviceState`/`SurfaceData`; enumerate all connectors;
  position outputs; per-surface render loop + per-(node,crtc) VBlank.
- Exit: two physically-connected monitors both light up, each with its own
  workspace, windows tile per monitor, pointer crosses between them.

**Phase 3 — Hotplug.**
- `UdevEvent::Changed` connector diff (add/remove/mode-change) with debounce;
  `Added`/`Removed` device handling; window migration (§5); zero-output interval.
- Exit: plug/unplug an external monitor at runtime with no crash, no lost windows;
  unplug-while-focused migrates focus; replug restores.

**Phase 4 — Output management + config.**
- `wlr-output-management`, `output` config stanza, hot-reload, `xdg-output` audit.
- Optional `wlr-output-power-management`.
- Exit: `wlr-randr`/`kanshi` can reposition/scale/disable; config persists layout.

**Phase 5 — Polish.**
- True per-output fractional scale (mixed-DPI), pointer warp on output focus,
  per-output gamma (`wlr-gamma-control`), multi-GPU render offload.

---

## 9. Testing strategy

Build the headless harness recommended in the main review (`tests/compositor_state.rs`,
`Display::new()` + `Beewm::new`) — it is a prerequisite for Phases 0–1 and gives most
of the multi-output coverage *without* real DRM:

- `resolver_returns_focused_output_for_new_window`
- `two_outputs_each_keep_independent_active_workspace`
- `switch_to_workspace_visible_elsewhere_moves_focus_not_workspace`
- `move_window_to_workspace_on_other_output_keeps_it_until_shown`
- `removing_output_migrates_its_workspaces_to_survivor`
- `removing_focused_output_moves_focus_and_refocuses_window`
- `floating_window_on_removed_output_is_reclamped_into_survivor`
- `zero_outputs_then_readd_restores_all_windows`
- `relayout_uses_each_outputs_own_usable_rect` (different exclusive zones per output)
- `pointer_crossing_shared_edge_enters_neighbor_output`
- `output_positions_recompute_on_mode_change_and_reclamp_floats`

DRM-backed paths (Phases 2–3) are validated manually with the existing
`beewm::frame`/`beewm::presentation` diagnostics, asserting each output reaches its
refresh rate and scanout independently.

---

## 10. Risk register

| Risk | Mitigation |
|---|---|
| Phase 0 refactor is broad (touches ~12 files) | No behavior change; land behind tests; the 17-site table is exhaustive so nothing is missed. |
| Scanout fast path regresses with N surfaces | Per-output dirty flags so an idle monitor never re-renders; keep `FrameFlags::DEFAULT` (see overlay-scanout memory). |
| Shared `GlesRenderer` across CRTCs contention | Single EGL context per *device* is the standard anvil arrangement; profile with `beewm::frame`. |
| Hotplug event bursts cause churn/flicker | Debounce `Changed` (coalesce within ~50 ms) before diffing connectors. |
| Workspace-ownership invariants drift | Centralize all mutations of `Workspace.output` / `OutputCtx.active_workspace` in a few methods; `debug_assert` the invariants (mirrors the focus-cache debug_assert already in `note_keyboard_focus_change`). |
| Multi-GPU buffer sharing complexity | Explicitly out of scope until Phase 5; Phases 2–4 target single-GPU multi-head. |

---

## 11. Rollback / safety

Each phase is a separate branch/PR off `main`. Phase 0–1 are pure compositor-state
changes guarded by the headless tests and a no-op on one monitor, so they can ship
ahead of any backend change. Phase 2 (backend) is the only one that cannot be
feature-flagged trivially; gate the multi-connector enumeration behind an env flag
(`BEEWM_MULTI_OUTPUT=1`) during stabilization, defaulting to first-connector-only,
then flip the default once dual-head is proven. The `runtime_flags` module already
provides this opt-in pattern.
