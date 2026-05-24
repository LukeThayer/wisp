use avian3d::prelude::LinearVelocity;
use bevy::camera::visibility::{NoFrustumCulling, RenderLayers};
use bevy::{gltf::Gltf, mesh::skinning::SkinnedMesh, prelude::*};

use bevy_enhanced_input::prelude::Action;

use crate::input::CycleCharacter;
use crate::player::{Facing, LocalPlayer, LocalWizardBody, Player, SELF_BODY_LAYER};

/// Available characters the local player can cycle through. Each variant
/// points at a `.glb` produced by `tools/character/port_character.py` and
/// must carry the same animation track names listed in the constants
/// below (they all derive from the Polysplit skeleton + the Mixamo
/// retarget that lives in `wizard.glb`).
#[derive(Clone, Copy, Debug)]
pub struct CharacterDef {
    pub id: &'static str,
    pub asset: &'static str,
    pub label: &'static str,
}

/// Registry of available characters. Ordered — the cycle action just
/// advances `CurrentCharacter.index` through this list modulo `len()`.
#[derive(Resource)]
pub struct CharacterRegistry {
    pub characters: Vec<CharacterDef>,
}

impl Default for CharacterRegistry {
    fn default() -> Self {
        // One entry: the unified character glb that holds every class
        // outfit + face/hair variant. Class selection happens via
        // `PartSelection` (see `player::parts`), NOT by swapping the
        // asset. The registry stays as a single-entry slot so the
        // existing load/swap plumbing keeps compiling without a
        // bigger refactor; it'll be flattened in a follow-up.
        Self {
            characters: vec![CharacterDef {
                id: "character",
                asset: "character.glb",
                label: "Character",
            }],
        }
    }
}

/// Index of the currently-selected character in `CharacterRegistry`.
/// `Changed<CurrentCharacter>` drives `apply_character_change` to swap
/// out the loaded gltf + the local body's SceneRoot.
#[derive(Resource, Default)]
pub struct CurrentCharacter {
    pub index: usize,
}

impl CurrentCharacter {
    pub fn current<'a>(&self, registry: &'a CharacterRegistry) -> Option<&'a CharacterDef> {
        registry.characters.get(self.index)
    }
    pub fn advance(&mut self, registry: &CharacterRegistry) {
        if !registry.characters.is_empty() {
            self.index = (self.index + 1) % registry.characters.len();
        }
    }
}

/// Animation clip names baked into every character `.glb`. Using the plain
/// locomotion clips for base movement; the `casting_*` variants are
/// reserved for the in-progress spell-cast animation pass.
const IDLE_CLIP: &str = "idle";
const WALK_F_CLIP: &str = "walk_forward";
const WALK_B_CLIP: &str = "walk_backward";
const WALK_L_CLIP: &str = "walk_left";
const WALK_R_CLIP: &str = "walk_right";
const FALL_CLIP: &str = "falling";
const CAST_IDLE_CLIP: &str = "casting_idle";
const CAST_WALK_F_CLIP: &str = "casting_walk_forward";
const CAST_WALK_B_CLIP: &str = "casting_walk_backward";
const CAST_WALK_L_CLIP: &str = "casting_walk_left";
const CAST_WALK_R_CLIP: &str = "casting_walk_right";

const WALK_MIN_SPEED: f32 = 0.2;
/// Player horizontal speed at which the walk clips play at their normal
/// authored speed (weight = 1). Used to scale the locomotion blend down
/// to idle as speed drops to zero. Set ~80% of the controller's
/// MAX_SPEED so the blend hits "full walk" a little before top speed
/// rather than only at the cap.
const LOCOMOTION_REF_SPEED: f32 = 3.5;

#[derive(Resource, Default)]
pub struct WizardAssets {
    gltf: Handle<Gltf>,
    pub(crate) graph: Option<Handle<AnimationGraph>>,
    pub(crate) idle: Option<AnimationNodeIndex>,
    pub(crate) walk_f: Option<AnimationNodeIndex>,
    pub(crate) walk_b: Option<AnimationNodeIndex>,
    pub(crate) walk_l: Option<AnimationNodeIndex>,
    pub(crate) walk_r: Option<AnimationNodeIndex>,
    pub(crate) falling: Option<AnimationNodeIndex>,
    pub(crate) cast_idle: Option<AnimationNodeIndex>,
    pub(crate) cast_walk_f: Option<AnimationNodeIndex>,
    pub(crate) cast_walk_b: Option<AnimationNodeIndex>,
    pub(crate) cast_walk_l: Option<AnimationNodeIndex>,
    pub(crate) cast_walk_r: Option<AnimationNodeIndex>,
}

impl WizardAssets {
    pub(crate) fn ready(&self) -> bool {
        self.graph.is_some() && self.idle.is_some()
    }

    /// Read-only access to the active character's gltf handle. The
    /// recolor module needs this to look up `named_materials` for
    /// the genericRGBMat_Body / genericRGBMat_Objects slots.
    pub fn gltf(&self) -> &Handle<Gltf> {
        &self.gltf
    }
}

pub fn load_wizard(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    registry: Res<CharacterRegistry>,
    current: Res<CurrentCharacter>,
) {
    let Some(def) = current.current(&registry) else {
        warn!("load_wizard: CurrentCharacter index {} out of range", current.index);
        return;
    };
    info!("character: initial load {:?} from {:?}", def.label, def.asset);
    commands.insert_resource(WizardAssets {
        gltf: asset_server.load(def.asset),
        ..default()
    });

    // Top-left label showing the current character. Lets the user verify
    // visually that the cycle (C key) is actually swapping assets and
    // which one is rendering right now.
    commands.spawn((
        CurrentCharacterLabel,
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(12.0),
            top: Val::Px(12.0),
            padding: UiRect::all(Val::Px(6.0)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
        children![(
            Text::new(def.label),
            TextFont {
                font_size: 18.0,
                ..default()
            },
            TextColor(Color::WHITE),
        )],
    ));
}

/// HUD label that mirrors the active `CurrentCharacter`. Update path:
/// `update_character_label` rewrites its child `Text` whenever
/// `CurrentCharacter` changes.
#[derive(Component)]
pub struct CurrentCharacterLabel;

pub fn update_character_label(
    registry: Res<CharacterRegistry>,
    current: Res<CurrentCharacter>,
    label: Query<&Children, With<CurrentCharacterLabel>>,
    mut texts: Query<&mut Text>,
) {
    if !current.is_changed() {
        return;
    }
    let Some(def) = current.current(&registry) else {
        return;
    };
    for children in &label {
        for child in children.iter() {
            if let Ok(mut text) = texts.get_mut(child) {
                text.0 = def.label.to_string();
            }
        }
    }
}

/// On `Changed<CurrentCharacter>` (set by the cycle-character input system),
/// re-load the gltf and clear the animation-graph state so
/// `build_graph_when_loaded` rebuilds for the new character. The local
/// body's SceneRoot is also swapped (see `swap_local_body_scene`).
pub fn apply_character_change(
    asset_server: Res<AssetServer>,
    registry: Res<CharacterRegistry>,
    current: Res<CurrentCharacter>,
    mut wizard: ResMut<WizardAssets>,
) {
    if !current.is_changed() || current.is_added() {
        return;
    }
    let Some(def) = current.current(&registry) else {
        return;
    };
    info!("character: switching to {:?} ({})", def.label, def.asset);
    *wizard = WizardAssets {
        gltf: asset_server.load(def.asset),
        ..default()
    };
}

pub fn build_graph_when_loaded(
    mut wizard: ResMut<WizardAssets>,
    gltfs: Res<Assets<Gltf>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
) {
    if wizard.ready() {
        return;
    }
    let Some(gltf) = gltfs.get(&wizard.gltf) else {
        return;
    };

    let mut graph = AnimationGraph::new();
    let root = graph.root;
    let mut add = |name: &str| -> Option<AnimationNodeIndex> {
        gltf.named_animations
            .get(name)
            .map(|clip| graph.add_clip(clip.clone(), 1.0, root))
    };

    wizard.idle = add(IDLE_CLIP);
    wizard.walk_f = add(WALK_F_CLIP);
    wizard.walk_b = add(WALK_B_CLIP);
    wizard.walk_l = add(WALK_L_CLIP);
    wizard.walk_r = add(WALK_R_CLIP);
    wizard.falling = add(FALL_CLIP);
    wizard.cast_idle = add(CAST_IDLE_CLIP);
    wizard.cast_walk_f = add(CAST_WALK_F_CLIP);
    wizard.cast_walk_b = add(CAST_WALK_B_CLIP);
    wizard.cast_walk_l = add(CAST_WALK_L_CLIP);
    wizard.cast_walk_r = add(CAST_WALK_R_CLIP);

    if wizard.idle.is_none() {
        warn!("wizard.glb is missing animation \"{IDLE_CLIP}\"");
        return;
    }

    wizard.graph = Some(graphs.add(graph));
}

/// The glTF loader spawns an AnimationPlayer entity inside the scene; attach
/// our graph + transitions when it appears.
/// Skinned meshes (the wizard body) get culled based on their bind-pose
/// AABB, which collapses to a near-point at the entity origin. Once the
/// origin leaves the view frustum the renderer drops the entire mesh —
/// even though the animated vertices are still on-screen — making the
/// wizard flicker out as soon as the player's feet leave the camera.
/// Tagging the mesh with `NoFrustumCulling` keeps it rendered regardless
/// of where the bind-pose AABB sits.
/// Detects the rising edge of the `CycleCharacter` input and advances
/// `CurrentCharacter`. The actual swap happens in
/// `apply_character_change` + `swap_local_body_scene` reacting to the
/// `Changed<CurrentCharacter>` fired by this advance.
pub fn cycle_character_on_input(
    action: Option<Single<&Action<CycleCharacter>>>,
    mut prev_pressed: Local<bool>,
    registry: Res<CharacterRegistry>,
    mut current: ResMut<CurrentCharacter>,
) {
    let pressed = action.map(|a| **a.into_inner()).unwrap_or(false);
    let edge = pressed && !*prev_pressed;
    *prev_pressed = pressed;
    if !edge {
        return;
    }
    current.advance(&registry);
}

/// Swap the local Player's body SceneRoot when `CurrentCharacter` changes.
///
/// The body is identified by being a child of the local Player and having
/// a `SceneRoot` component. Replacing the `SceneRoot` handle causes
/// Bevy's gltf loader to despawn the existing scene tree (including its
/// `AnimationPlayer`) and spawn the new one. `attach_animation_graph`
/// then picks up the new player on a later frame and reattaches the
/// (also-just-rebuilt) graph.
pub fn swap_local_body_scene(
    asset_server: Res<AssetServer>,
    registry: Res<CharacterRegistry>,
    current: Res<CurrentCharacter>,
    mut bodies: Query<&mut SceneRoot, (With<LocalPlayer>, Without<Player>)>,
) {
    if !current.is_changed() || current.is_added() {
        return;
    }
    let Some(def) = current.current(&registry) else {
        return;
    };
    let new_handle = asset_server.load(GltfAssetLabel::Scene(0).from_asset(def.asset));
    for mut scene_root in &mut bodies {
        scene_root.0 = new_handle.clone();
    }
}

pub fn disable_skinned_mesh_culling(
    mut commands: Commands,
    pending: Query<Entity, (With<SkinnedMesh>, Without<NoFrustumCulling>)>,
) {
    for entity in &pending {
        commands.entity(entity).insert(NoFrustumCulling);
    }
}

/// Walk newly-spawned mesh entities; if any ancestor is `LocalWizardBody`,
/// stamp `RenderLayers::layer(SELF_BODY_LAYER)`. Bevy does NOT auto-
/// propagate `RenderLayers` from parent to child, so without this every
/// mesh inside the gltf scene would default to layer 0 and re-appear in
/// the main camera (defeating the whole point of the layer filter).
///
/// The same descendant walk handles scene-swap: when `swap_local_body_scene`
/// replaces the SceneRoot handle, the new gltf spawns fresh meshes
/// without a `RenderLayers` component — `Added` fires for them and we
/// stamp the layer onto each.
pub fn propagate_self_body_render_layer(
    mut commands: Commands,
    new_meshes: Query<
        Entity,
        (
            Or<(Added<Mesh3d>, Added<SkinnedMesh>)>,
            Without<RenderLayers>,
        ),
    >,
    parents: Query<&ChildOf>,
    body_marker: Query<(), With<LocalWizardBody>>,
) {
    for entity in &new_meshes {
        let mut current = entity;
        loop {
            if body_marker.contains(current) {
                commands
                    .entity(entity)
                    .insert(RenderLayers::layer(SELF_BODY_LAYER));
                break;
            }
            match parents.get(current) {
                Ok(parent) => current = parent.0,
                Err(_) => break,
            }
        }
    }
}

pub fn attach_animation_graph(
    mut commands: Commands,
    wizard: Res<WizardAssets>,
    pending: Query<Entity, (With<AnimationPlayer>, Without<AnimationGraphHandle>)>,
    mut players: Query<&mut AnimationPlayer>,
) {
    if !wizard.ready() {
        return;
    }
    let Some(graph) = wizard.graph.clone() else {
        return;
    };
    for entity in &pending {
        let Ok(mut player) = players.get_mut(entity) else {
            continue;
        };
        // Start every locomotion clip muted-at-rest looping. The
        // per-frame blend system then sets weights based on velocity.
        // `play` is idempotent (uses `entry().or_default()`), so each
        // clip is started exactly once here.
        for node in [
            wizard.idle,
            wizard.walk_f,
            wizard.walk_b,
            wizard.walk_l,
            wizard.walk_r,
            wizard.falling,
            wizard.cast_idle,
            wizard.cast_walk_f,
            wizard.cast_walk_b,
            wizard.cast_walk_l,
            wizard.cast_walk_r,
        ]
        .into_iter()
        .flatten()
        {
            player.play(node).repeat().set_weight(0.0);
        }
        // Backwards walk reads too slow at its authored speed once the
        // overall MAX_SPEED dropped — bump the clip playback speed to
        // bring the foot cadence back in sync. Casting variant gets the
        // same scale.
        const WALK_BACKWARD_SPEED_SCALE: f32 = 1.15;
        for node in [wizard.walk_b, wizard.cast_walk_b].into_iter().flatten() {
            if let Some(active) = player.animation_mut(node) {
                active.set_speed(WALK_BACKWARD_SPEED_SCALE);
            }
        }
        // Seed idle at full weight so the wizard isn't a T-pose for the
        // single frame before the blend driver runs.
        if let Some(idle) = wizard.idle {
            player.play(idle).set_weight(1.0);
        }
        commands.entity(entity).insert(AnimationGraphHandle(graph.clone()));
    }
}

/// Compute per-clip blend weights.
///
/// - `airborne_blend` (0..1): routes weight between locomotion clips and
///   `falling`. 0 = fully grounded, 1 = fully airborne.
/// - `casting_blend` (0..1): cross-fades the locomotion side between the
///   plain clips (idle / walk_*) and the casting variants
///   (casting_idle / casting_walk_*). 0 = plain, 1 = casting.
///
/// Diagonal motion still blends two walk clips by dot-product against
/// the velocity direction; idle fills in as speed drops. All weights sum
/// to 1 across the clip set.
///
/// If a casting variant clip is missing from the GLTF, that direction's
/// casting weight is silently dropped and the plain clip stays visible;
/// this lets the system degrade gracefully if only some casting clips
/// are authored.
pub(crate) fn locomotion_blend(
    wizard: &WizardAssets,
    world_velocity: Vec3,
    yaw: f32,
    airborne_blend: f32,
    casting_blend: f32,
) -> Vec<(AnimationNodeIndex, f32)> {
    let mut out: Vec<(AnimationNodeIndex, f32)> = Vec::with_capacity(11);
    let push = |out: &mut Vec<_>, node: Option<AnimationNodeIndex>, w: f32| {
        if let Some(n) = node {
            out.push((n, w));
        }
    };

    let airborne_blend = airborne_blend.clamp(0.0, 1.0);
    let casting_blend = casting_blend.clamp(0.0, 1.0);
    let ground_factor = 1.0 - airborne_blend;
    let cast_factor = casting_blend;
    let plain_factor = 1.0 - casting_blend;
    push(&mut out, wizard.falling, airborne_blend);

    let planar = Vec3::new(world_velocity.x, 0.0, world_velocity.z);
    let speed = planar.length();

    if speed < WALK_MIN_SPEED {
        let idle_w = ground_factor;
        push(&mut out, wizard.idle, idle_w * plain_factor);
        push(&mut out, wizard.cast_idle, idle_w * cast_factor);
        push(&mut out, wizard.walk_f, 0.0);
        push(&mut out, wizard.walk_b, 0.0);
        push(&mut out, wizard.walk_l, 0.0);
        push(&mut out, wizard.walk_r, 0.0);
        push(&mut out, wizard.cast_walk_f, 0.0);
        push(&mut out, wizard.cast_walk_b, 0.0);
        push(&mut out, wizard.cast_walk_l, 0.0);
        push(&mut out, wizard.cast_walk_r, 0.0);
        return out;
    }

    let locomotion = (speed / LOCOMOTION_REF_SPEED).clamp(0.0, 1.0);
    let idle_share = ground_factor * (1.0 - locomotion);
    push(&mut out, wizard.idle, idle_share * plain_factor);
    push(&mut out, wizard.cast_idle, idle_share * cast_factor);

    // World velocity → local frame (player forward = -Z).
    let local = (Quat::from_axis_angle(Vec3::Y, -yaw) * planar) / speed;
    let forward = -local.z;
    let right = local.x;
    let walking = ground_factor * locomotion;
    let f_w = walking * forward.max(0.0);
    let b_w = walking * (-forward).max(0.0);
    let r_w = walking * right.max(0.0);
    let l_w = walking * (-right).max(0.0);

    push(&mut out, wizard.walk_f, f_w * plain_factor);
    push(&mut out, wizard.walk_b, b_w * plain_factor);
    push(&mut out, wizard.walk_r, r_w * plain_factor);
    push(&mut out, wizard.walk_l, l_w * plain_factor);
    push(&mut out, wizard.cast_walk_f, f_w * cast_factor);
    push(&mut out, wizard.cast_walk_b, b_w * cast_factor);
    push(&mut out, wizard.cast_walk_r, r_w * cast_factor);
    push(&mut out, wizard.cast_walk_l, l_w * cast_factor);
    out
}

/// Per-frame exponential follow toward the target airborne state.
/// `alpha` 0.25 reaches ~95% of a step input in ~12 frames (~200ms at
/// 60Hz) — fast enough that the jump still reads as a "pop" of falling
/// but slow enough to avoid the sub-frame on/off twitch.
pub(crate) fn step_airborne_blend(current: f32, grounded: bool) -> f32 {
    const ALPHA: f32 = 0.25;
    let target = if grounded { 0.0 } else { 1.0 };
    current + (target - current) * ALPHA
}

/// Per-frame exponential follow toward the target casting state.
/// `alpha` 0.2 reaches ~95% in ~14 frames (~230ms at 60Hz). A little
/// slower than the airborne ease so the cross-fade between locomotion
/// and casting variants feels deliberate.
pub(crate) fn step_casting_blend(current: f32, casting: bool) -> f32 {
    const ALPHA: f32 = 0.2;
    let target = if casting { 1.0 } else { 0.0 };
    current + (target - current) * ALPHA
}

/// Apply a locomotion blend to an `AnimationPlayer`. Each clip's weight
/// is set directly; clips not yet started are added (idempotent).
pub(crate) fn apply_locomotion_blend(
    player: &mut AnimationPlayer,
    wizard: &WizardAssets,
    world_velocity: Vec3,
    yaw: f32,
    airborne_blend: f32,
    casting_blend: f32,
) {
    for (node, weight) in
        locomotion_blend(wizard, world_velocity, yaw, airborne_blend, casting_blend)
    {
        player.play(node).repeat().set_weight(weight);
    }
}

/// Smoothly-eased airborne + casting factors for the local player.
/// Persisted across frames so jump start/land and cast wind-up/release
/// fade between clip sets instead of popping.
#[derive(Component, Default)]
pub struct LocalAnimBlend {
    pub airborne: f32,
    pub casting: f32,
}

pub fn drive_animation(
    wizard: Res<WizardAssets>,
    player: Single<
        (
            &LinearVelocity,
            &Facing,
            &crate::spells::engine::CastState,
            &mut LocalAnimBlend,
        ),
        (With<Player>, With<LocalPlayer>),
    >,
    mut anim: Query<(Entity, &mut AnimationPlayer)>,
    parents: Query<&ChildOf>,
    local_markers: Query<(), With<LocalPlayer>>,
) {
    if !wizard.ready() {
        return;
    }
    let (velocity, facing, cast_state, mut blend) = player.into_inner();

    // Casting animation plays while a cast is winding up (Charging — any
    // trigger with a non-None `ChargingDef`) OR actively in flight
    // (Channeling — Hold). Iris's Charging and convex lens's Channeling
    // both trip this; idle and Cooldown do not.
    let casting = cast_state.instances.values().any(|inst| {
        matches!(
            inst.phase,
            crate::spells::engine::CastPhase::Charging
                | crate::spells::engine::CastPhase::Channeling
        )
    });
    blend.airborne = step_airborne_blend(blend.airborne, facing.grounded);
    blend.casting = step_casting_blend(blend.casting, casting);

    for (anim_entity, mut anim_player) in &mut anim {
        // Only drive animation players that are descendants of the local
        // rig (LocalPlayer marker present on root + camera + lens + body).
        // Remote NetworkedPlayer wizards are driven independently by
        // `net::replication::drive_remote_animations`.
        let mut e = anim_entity;
        let mut owned_by_local = false;
        loop {
            if local_markers.get(e).is_ok() {
                owned_by_local = true;
                break;
            }
            let Ok(parent) = parents.get(e) else { break };
            e = parent.parent();
        }
        if !owned_by_local {
            continue;
        }

        apply_locomotion_blend(
            &mut anim_player,
            &wizard,
            velocity.0,
            facing.yaw,
            blend.airborne,
            blend.casting,
        );
    }
}
