# Plan — `wisp`: single-player FPS wizard game (baseline)

## Context

A new Bevy project at `/home/luke/src/wisp`. The end goal is a single-player first-person wizard game centered on **physics-based magic with emergent gameplay** (spells exert forces, propagate, combine). Spells are selected via a radial menu (held `F`).

This plan covers **only the baseline**: project skeleton, FPS movement controller, look/jump, equipped-spell data model, and a radial menu *skeleton* (no actual spells yet). Magic implementation is the next phase, intentionally deferred.

Stack preferences pulled from sibling projects:
- **marble** — closest reference: Bevy 0.17, `avian3d` 0.4, `bevy_enhanced_input` 0.20, dynamic rigid body with force-driven movement, `bevy-inspector-egui` for dev.
- **graybeard** — modular `Plugin`-per-module layout, dynamic body + force application, ground detection via raycast.
- **btdd** — modern `bevy_enhanced_input` idioms: multiple `InputContext`s with `ContextActivity` toggled per frame, observers (`On<Start<Action>>`, `On<Fire<Action>>`) for handlers, `require_reset: true` on actions to prevent cross-context cascade.

Confirmed decisions:
- Project name: **wisp**
- Controller: **dynamic rigid body driven by forces** (so spells can later push, throw, ragdoll the player — central to the physics-magic vision).

## Stack

Match marble's known-good versions to avoid bleeding-edge churn:

```toml
[package]
name = "wisp"
version = "0.1.0"
edition = "2024"

[dependencies]
avian3d = "0.4"
bevy = "0.17"
bevy-inspector-egui = "0.35"
bevy_enhanced_input = "0.20"
```

Skip `bevy_skein` for now (no Blender assets yet) and `bevy_hanabi` (no VFX yet). Add when needed.

## File layout

Flat, mirroring marble's brevity. Each module owns its own `Plugin`.

```
wisp/
├── Cargo.toml
└── src/
    ├── main.rs           # App, DefaultPlugins, PhysicsPlugins, our plugins, cursor grab
    ├── player/
    │   ├── mod.rs        # pub mod controller; pub mod spells; pub struct PlayerPlugin
    │   ├── controller.rs # dynamic-body FPS controller + look + jump + ground check
    │   └── spells.rs     # SpellId enum, EquippedSpells component, ActiveSpell resource
    ├── ui/
    │   ├── mod.rs        # pub mod radial_menu; pub struct UiPlugin
    │   └── radial_menu.rs # 8-segment radial overlay, hover->select on F release
    ├── input.rs          # InputAction definitions + Player/Menu InputContexts + bindings
    └── world.rs          # test level: ground plane, light, scattered dynamic props
```

## Player controller (`player/controller.rs`)

Modeled on marble's `apply_movement` / `apply_jump` pattern, but **first-person** instead of orbit-cam.

**Entity composition** (spawned in `world.rs` setup):
- `Player` marker component (`pub struct Player;`)
- `RigidBody::Dynamic`
- `Collider::capsule(0.4, 1.2)` (radius, height; full body ~1.7m tall)
- `LockedAxes::ROTATION_LOCKED` except yaw — easiest: lock all rotations and store yaw in a `Facing` component the controller writes (graybeard pattern). Body itself stays upright; rotation is driven by us.
- `LinearDamping(0.5)`, `Friction::new(0.0)` (we control speed via force clamping, not friction — friction makes wall-slide and slope behavior unpredictable for an FPS feel)
- `Mass(80.0)`
- `Restitution::new(0.0)`
- Child `Camera3d` at local translation `(0.0, 0.7, 0.0)` (eye height above capsule center) with `PerspectiveProjection { fov: 90°.to_radians(), .. }`
- Child `RayCaster` pointed down `-Y` with `max_distance = 0.25` from feet, for ground detection (graybeard pattern)

**`Facing` component:**
```rust
#[derive(Component, Default)]
pub struct Facing {
    pub yaw: f32,    // radians, body rotation
    pub pitch: f32,  // radians, camera-only pitch, clamped ±85°
    pub grounded: bool,
}
```

**Systems / observers** (registered in `PlayerPlugin`):
- `apply_look` (observer on `Fire<Look>`): mouse delta → update `Facing.yaw` and `Facing.pitch` (clamp pitch). Sensitivity constant.
- `apply_rotation` (system, `Update`): write `Facing.yaw` to `Rotation` (yaw-only quat) on the body; write `Facing.pitch` to the child camera's local `Transform.rotation` (pitch-only quat). Splitting keeps the body upright while letting the camera look up/down.
- `apply_movement` (observer on `Fire<Movement>`): WASD vector → desired horizontal velocity in world space (rotated by `Facing.yaw`). Compute required force using marble's pattern:
  ```
  desired_ground_vel = move_dir.normalize_or_zero() * MAX_SPEED
  current_ground_vel = (linvel.x, 0, linvel.z)
  delta = desired_ground_vel - current_ground_vel
  accel = delta.clamp_length_max(MAX_ACCEL * dt) / dt
  apply_force(accel * mass)
  ```
  Constants: `MAX_SPEED = 6.0`, `MAX_ACCEL = 60.0` (ground), reduced to `10.0` when airborne so the player can't strafe-cheese in midair.
- `apply_jump` (observer on `Start<Jump>`): if `Facing.grounded`, apply vertical impulse (~`mass * 5.0` upward).
- `ground_check` (system, `FixedUpdate`): read `RayHits` from the downward `RayCaster`, set `Facing.grounded`.
- `cursor_grab` (system, `Update`): on window focus / on `Esc`, toggle `CursorGrabMode::Locked` + cursor visibility.

## Spell data model (`player/spells.rs`)

Just data — no behavior. Magic comes next phase.

```rust
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum SpellId {
    #[default]
    Empty,
    Placeholder1, Placeholder2, Placeholder3, Placeholder4,
    Placeholder5, Placeholder6, Placeholder7, Placeholder8,
}

impl SpellId {
    pub fn label(self) -> &'static str { /* "Empty", "Slot 1", ... */ }
}

#[derive(Component)]
pub struct EquippedSpells(pub [SpellId; 8]);

impl Default for EquippedSpells {
    fn default() -> Self {
        Self([
            SpellId::Placeholder1, SpellId::Placeholder2,
            SpellId::Placeholder3, SpellId::Placeholder4,
            SpellId::Placeholder5, SpellId::Placeholder6,
            SpellId::Placeholder7, SpellId::Placeholder8,
        ])
    }
}

#[derive(Resource, Default)]
pub struct ActiveSpell(pub SpellId);
```

Spawned alongside `Player`. Left-click does nothing yet — it'll dispatch on `ActiveSpell` in the magic phase.

## Input (`input.rs`)

Two contexts, toggled mutually exclusively. Follow btdd's pattern: `ContextActivity::<Ctx>` inserted when state changes; actions use `require_reset: true`.

**Actions** (all `#[derive(InputAction)]`):
- `Movement` — `Vec2` output, WASD via `Cardinal::wasd_keys()`
- `Look` — `Vec2` output, mouse motion (scaled)
- `Jump` — `bool`, `Space`
- `OpenRadial` — `bool`, `KeyF`
- `Fire` — `bool`, mouse left (no-op handler this phase, just wires the input)

**Contexts:**
- `PlayerContext` — Movement, Look, Jump, Fire, OpenRadial. Active by default.
- `RadialMenuContext` — Look (reused for cursor angle), OpenRadial (to detect release). Active while menu is open.

**State resource:**
```rust
#[derive(Resource, Default, PartialEq, Eq)]
pub enum InputMode { #[default] Player, RadialMenu }
```

A `sync_contexts` system inserts the correct `ContextActivity` when `InputMode` changes (btdd pattern, minus the per-frame diff complexity since we only have 2 contexts).

## Radial menu (`ui/radial_menu.rs`)

Skeleton — visual + selection wiring, no spell content.

- Press-and-hold `F`: observer on `Start<OpenRadial>` sets `InputMode::RadialMenu`, ungrabs cursor, spawns `RadialMenuRoot` UI node.
- While open: a system reads mouse delta (via `Look` observer accumulating to a `RadialCursor` resource holding `Vec2`), computes angle from center, picks segment index `(angle.to_degrees() / 45.0).round() as usize % 8`. Highlight that segment.
- Render: 8 `Node`s arranged in a ring (computed absolute positions). Center label shows current `ActiveSpell`. Hovered segment changes background color.
- Release `F`: observer on `Complete<OpenRadial>` (or `End`) writes the hovered segment's `SpellId` to `ActiveSpell`, despawns the menu, sets `InputMode::Player`, regrabs cursor.

**v0 acceptance:** segments show "Slot 1".."Slot 8"; releasing F prints `info!("active spell = {label}")`. No casting yet.

## Test world (`world.rs`)

- Ground: `Mesh3d(cuboid 50×0.5×50)`, `Collider::cuboid(25.0, 0.25, 25.0)`, `RigidBody::Static`, neutral gray material.
- ~6 dynamic props: mixed cubes/spheres (`RigidBody::Dynamic`, mass 1–5kg) scattered in front of the player spawn. These exist so once magic lands you can immediately throw / push them — useful smoke-test for the controller (you can run into them and see they react).
- `DirectionalLight` + low ambient.
- Player spawned at `(0, 2, 5)` looking down `-Z`.

## `main.rs`

```rust
fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins,
            PhysicsPlugins::default(),
            EguiPlugin::default(),
            WorldInspectorPlugin::new(),     // dev only — fine for now
            input::InputPlugin,
            player::PlayerPlugin,
            ui::UiPlugin,
            world::WorldPlugin,
        ))
        .run();
}
```

## Reference files (existing sibling repos)

- `/home/luke/src/marble/src/character/controller.rs` — force-application pattern, `bevy_enhanced_input` action/observer idiom (lines 41–117).
- `/home/luke/src/graybeard/src/character/controller.rs` — split yaw-on-body / pitch-on-camera (lines 37–84), ground detection via `RayHits` (lines 116–120), `move_towards`-style velocity integration (lines 92–104).
- `/home/luke/src/btdd/src/input/mod.rs` — multi-context registration, `ContextActivity` toggling, `register_observers` aggregation (lines 24–69, 259–448).
- `/home/luke/src/btdd/src/input/bindings.rs` — `action()` helper with `require_reset: true` (lines 33–53), `actions!` macro use (lines 57–77).

## Verification

1. `cd /home/luke/src/wisp && cargo run` — game opens, cursor locked.
2. WASD moves the camera horizontally, mouse looks, Space jumps. Player doesn't tip over.
3. Walking into a prop pushes it (physics works both ways — confirms dynamic body is the right call).
4. Hold `F` → cursor releases, 8-segment ring appears, moving mouse highlights segments.
5. Release `F` → ring closes, cursor relocks, log shows `active spell = Slot N` matching the hovered segment.
6. `Esc` toggles cursor lock for debugging.
7. Inspector window (egui) lists the player entity with `Facing`, `EquippedSpells`, etc.

## Out of scope (next phase: magic)

- Actual spell behaviors / projectiles / fields
- Spell selection persistence / loadouts
- Animations, sounds, VFX
- Enemies / AI
- Saving / loading
- Menus beyond the radial
