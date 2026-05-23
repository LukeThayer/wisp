//! Cast engine: turns RON-defined `CastDef`s into runtime behavior.
//!
//! Pipeline each frame:
//! 1. `tick_cooldowns` decrements cooldown timers.
//! 2. `phase_advance` reads input states, transitions cast instances
//!    through Idle → Charging / Channeling → Releasing → Cooldown
//!    based on each cast's `TriggerDef` + `ChargingDef`.
//! 3. `tick_charge` advances charging accumulation (Time or Powered).
//! 4. `dispatch_casts` (exclusive world) iterates instances in
//!    Releasing / Channeling and runs delivery + effect.
//!
//! All four systems are gated by the loaded `SpellCatalog`; if it's empty,
//! the engine does nothing.

use std::collections::HashMap;

use bevy::ecs::system::SystemState;
use bevy::prelude::*;
use bevy_enhanced_input::prelude::*;

use crate::input::{Fire, FireSecondary, Jump, Pickup, ThrowLantern};
use crate::player::Player;
use crate::spells::catalog::SpellCatalog;
use crate::spells::data::{
    ActionRef, CastDef, CastId, ChargeSource, ChargingDef, ConditionDef, EffectDef,
    MarkerKind, OriginDef, OverchargeDef, PayloadDef, PowerSourceRef, TargetDef,
    TriggerDef,
};
use crate::spells::handlers::{Authority, CastContext, HandlerRegistry};
use crate::spells::markers::Lantern;
use crate::spells::{ActiveSpell, SpellId};

pub struct CastEnginePlugin;

/// System ordering anchors so spell modules can schedule their own systems
/// relative to the engine pipeline without depending on Bevy's topological
/// tiebreak. The classic case: iris's `apply_burst_on_release` must read
/// the `Releasing` phase + `captured_charge` *before* `dispatch_casts`
/// clears them on the same frame.
#[derive(SystemSet, Clone, Hash, Eq, PartialEq, Debug)]
pub enum CastEngineSet {
    /// `phase_advance` writes `CastPhase` transitions.
    PhaseAdvance,
    /// `dispatch_casts` consumes Releasing/Channeling phases and clears
    /// `captured_charge`. Spell-module hooks that need to see those values
    /// should `.before(CastEngineSet::Dispatch)`.
    Dispatch,
}

impl Plugin for CastEnginePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                tick_cooldowns,
                phase_advance.in_set(CastEngineSet::PhaseAdvance),
                tick_charge,
                dispatch_casts.in_set(CastEngineSet::Dispatch),
            )
                .chain(),
        );
    }
}

// --- Per-player runtime ----------------------------------------------------

/// Per-player map of cast id → CastInstance. Auto-populated on first use
/// of each cast; the engine consults `SpellCatalog` rather than walking
/// this map for "which casts exist."
#[derive(Component, Default, Debug)]
pub struct CastState {
    pub instances: HashMap<CastId, CastInstance>,
}

#[derive(Clone, Debug, Default)]
pub struct CastInstance {
    pub phase: CastPhase,
    /// Current accumulated charge (0..max for Charging).
    pub charge: f32,
    /// Snapshot of `charge` at the moment of release (PressReleaseCharged).
    pub captured_charge: Option<f32>,
    pub cooldown_remaining: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CastPhase {
    #[default]
    Idle,
    /// Cast accumulating charge (any trigger × `ChargingDef::Charging`).
    /// Press/PressReleaseCharged exit via Releasing on release-with-charge;
    /// Hold exits via Channeling when charge ≥ max_charge.
    Charging,
    /// Hold-trigger cast in flight; fires effect every frame until release.
    Channeling,
    /// Single-shot dispatch frame. Press without charging arrives here
    /// directly with `captured_charge = Some(1.0)`. Press/PressReleaseCharged
    /// with charging arrive here on release with `captured_charge` =
    /// whatever charge they held. Transitions to Cooldown after dispatch.
    Releasing,
    /// Waiting for cooldown to expire.
    Cooldown,
}

/// Snapshot of which input actions are firing this frame, indexed by
/// `ActionRef`. Built once per player at the top of `phase_advance` so the
/// state machine can do trigger-matching without re-querying bei for every
/// cast.
#[derive(Default, Clone, Copy)]
struct ActionSnapshot {
    fire: bool,
    fire_secondary: bool,
    throw_lantern: bool,
    pickup: bool,
    jump: bool,
}

impl ActionSnapshot {
    fn held(&self, action: ActionRef) -> bool {
        match action {
            ActionRef::Fire => self.fire,
            ActionRef::FireSecondary => self.fire_secondary,
            ActionRef::ThrowLantern => self.throw_lantern,
            ActionRef::Pickup => self.pickup,
            ActionRef::Jump => self.jump,
        }
    }
}

/// Tracks previous-frame action state so we can detect press/release edges.
#[derive(Component, Default)]
pub struct PrevActionSnapshot {
    prev: ActionSnapshot,
}

// --- Systems ---------------------------------------------------------------

fn tick_cooldowns(time: Res<Time>, mut q: Query<&mut CastState>) {
    let dt = time.delta_secs();
    for mut state in &mut q {
        for instance in state.instances.values_mut() {
            if instance.phase != CastPhase::Cooldown {
                continue;
            }
            instance.cooldown_remaining -= dt;
            if instance.cooldown_remaining <= 0.0 {
                instance.cooldown_remaining = 0.0;
                instance.phase = CastPhase::Idle;
            }
        }
    }
}

fn tick_charge(
    time: Res<Time>,
    catalog: Res<SpellCatalog>,
    mut q: Query<(&mut CastState, Option<&crate::spells::convex_lens::LensPower>)>,
) {
    if catalog.is_empty() {
        return;
    }
    let dt = time.delta_secs();
    for (mut state, lens_power) in &mut q {
        for (_, _, cast) in iter_all_casts(&catalog) {
            let Some(instance) = state.instances.get_mut(&cast.id) else {
                continue;
            };
            if instance.phase != CastPhase::Charging {
                continue;
            }
            let ChargingDef::Charging {
                source,
                max_charge,
                decay_rate,
                overcharge,
                ..
            } = &cast.charging
            else {
                // Defensive: entered Charging with `ChargingDef::None`. Can
                // happen if a PressReleaseCharged trigger has no charging
                // def. Snap to Releasing with zero charge so the cast
                // resolves cleanly rather than hanging.
                instance.captured_charge = Some(0.0);
                instance.phase = CastPhase::Releasing;
                continue;
            };
            let hard_cap = overcharge
                .as_ref()
                .map(|o: &OverchargeDef| o.max)
                .unwrap_or(*max_charge);
            match source {
                ChargeSource::Time { duration } => {
                    let rate = max_charge / duration.max(1e-3);
                    instance.charge = (instance.charge + rate * dt).min(hard_cap);
                }
                ChargeSource::Powered { source } => {
                    let scalar = match source {
                        PowerSourceRef::LensPower => {
                            lens_power.map(|p| p.scalar).unwrap_or(0.0)
                        }
                    };
                    if scalar > 0.0 {
                        instance.charge = (instance.charge + scalar * dt).min(hard_cap);
                    } else {
                        instance.charge = (instance.charge - decay_rate * dt).max(0.0);
                    }
                }
            }
            // Hold triggers promote to Channeling once the bar reaches
            // max_charge (overcharge is meaningless for a continuous
            // channel — the player isn't "releasing" anything).
            if matches!(cast.trigger, TriggerDef::Hold { .. })
                && instance.charge >= *max_charge
            {
                instance.phase = CastPhase::Channeling;
                instance.charge = 0.0;
            }
        }
    }
}

fn phase_advance(
    catalog: Res<SpellCatalog>,
    // bei's `actions!()` macro spawns each `Action<A>` on a child entity of
    // the context owner, so we can't query them alongside the player in one
    // `Query` row. For single-player there's exactly one of each, so a
    // `Single` per type works. Multiplayer will need to traverse
    // `Actions<PlayerContext>` per player.
    fire: Option<Single<&Action<Fire>>>,
    fire_secondary: Option<Single<&Action<FireSecondary>>>,
    throw_lantern: Option<Single<&Action<ThrowLantern>>>,
    pickup: Option<Single<&Action<Pickup>>>,
    jump: Option<Single<&Action<Jump>>>,
    player: Option<
        Single<(&ActiveSpell, &mut CastState, &mut PrevActionSnapshot), With<Player>>,
    >,
) {
    if catalog.is_empty() {
        return;
    }
    let Some(player) = player else { return; };

    let now = ActionSnapshot {
        fire: fire.map(|s| **s.into_inner()).unwrap_or(false),
        fire_secondary: fire_secondary.map(|s| **s.into_inner()).unwrap_or(false),
        throw_lantern: throw_lantern.map(|s| **s.into_inner()).unwrap_or(false),
        pickup: pickup.map(|s| **s.into_inner()).unwrap_or(false),
        jump: jump.map(|s| **s.into_inner()).unwrap_or(false),
    };

    let (active, mut state, mut prev) = player.into_inner();

    {
        for (spell_id, _, cast) in iter_all_casts(&catalog) {
            if cast.requires_active && !active.0.eq(spell_id) {
                continue;
            }
            let action = cast.trigger.action();
            let held = now.held(action);
            let was_held = prev.prev.held(action);
            let pressed = held && !was_held;
            let released = !held && was_held;

            // Lazily insert the instance on first observation.
            let instance = state.instances.entry(cast.id.clone()).or_default();

            let has_charging = matches!(cast.charging, ChargingDef::Charging { .. });
            let min_release = match &cast.charging {
                ChargingDef::Charging { min_release, .. } => *min_release,
                ChargingDef::None => 0.0,
            };

            match (&cast.trigger, instance.phase) {
                // Press + Charging: press enters Charging; charge fills via
                // tick_charge. On release, fire iff charge ≥ min_release;
                // otherwise fizzle to Idle. For wind-up gates that must
                // reach full before they fire, set min_release == max_charge
                // in the spell's ChargingDef.
                (TriggerDef::Press { .. }, CastPhase::Idle) if pressed && has_charging => {
                    instance.phase = CastPhase::Charging;
                    instance.charge = 0.0;
                }
                (TriggerDef::Press { .. }, CastPhase::Charging) if released => {
                    if instance.charge >= min_release {
                        instance.captured_charge = Some(instance.charge);
                        instance.phase = CastPhase::Releasing;
                    } else {
                        instance.phase = CastPhase::Idle;
                    }
                    instance.charge = 0.0;
                }

                // Press without Charging: classic one-shot at press.
                // captured_charge = Some(1.0) so effects can still scale
                // uniformly by `captured_charge` without special-casing
                // "charging vs not."
                (TriggerDef::Press { .. }, CastPhase::Idle) if pressed => {
                    instance.captured_charge = Some(1.0);
                    instance.phase = CastPhase::Releasing;
                }

                // Hold + Charging: wind-up via Charging; tick_charge promotes
                // to Channeling at max_charge. Release during wind-up cancels.
                (TriggerDef::Hold { .. }, CastPhase::Idle) if held && has_charging => {
                    instance.phase = CastPhase::Charging;
                    instance.charge = 0.0;
                }
                (TriggerDef::Hold { .. }, CastPhase::Charging) if !held => {
                    instance.phase = CastPhase::Idle;
                    instance.charge = 0.0;
                }

                // Hold without Charging: enter Channeling immediately on press.
                (TriggerDef::Hold { .. }, CastPhase::Idle) if held => {
                    instance.phase = CastPhase::Channeling;
                }
                (TriggerDef::Hold { .. }, CastPhase::Channeling) if !held => {
                    instance.phase = if cast.cooldown > 0.0 {
                        instance.cooldown_remaining = cast.cooldown;
                        CastPhase::Cooldown
                    } else {
                        CastPhase::Idle
                    };
                }

                // PressReleaseCharged: enter Charging on press; release fires
                // with whatever charge was held, scaled iff ≥ min_release.
                // Default min_release of 0 preserves "any release fires".
                (TriggerDef::PressReleaseCharged { .. }, CastPhase::Idle) if pressed => {
                    instance.phase = CastPhase::Charging;
                    instance.charge = 0.0;
                }
                (TriggerDef::PressReleaseCharged { .. }, CastPhase::Charging) if released => {
                    if instance.charge >= min_release {
                        instance.captured_charge = Some(instance.charge);
                        instance.phase = CastPhase::Releasing;
                    } else {
                        instance.phase = CastPhase::Idle;
                        instance.charge = 0.0;
                    }
                }
                _ => {}
            }
        }

        prev.prev = now;
    }
}

/// Iterates `(spell_id, spell_def, cast_def)` over the entire catalog.
fn iter_all_casts(
    catalog: &SpellCatalog,
) -> impl Iterator<Item = (&SpellId, &crate::spells::data::SpellDef, &CastDef)> + '_ {
    catalog
        .iter()
        .flat_map(|(id, spell)| spell.casts.iter().map(move |c| (id, spell, c)))
}

// --- Dispatch (exclusive world) -------------------------------------------

/// Runs delivery + effect for casts in Active / Channeling / Releasing
/// phase. Exclusive world so custom handlers can use whatever queries they
/// need without parameter conflicts.
fn dispatch_casts(world: &mut World) {
    // Snapshot the work we need to do — copy out cast IDs + phases for each
    // player so we don't hold a borrow on World during handler execution.
    let mut work: Vec<DispatchWork> = Vec::new();
    {
        let mut q = world.query::<(Entity, &ActiveSpell, &CastState)>();
        let Some(catalog) = world.get_resource::<SpellCatalog>() else { return; };
        if catalog.is_empty() {
            return;
        }
        for (player, _active, state) in q.iter(world) {
            for (cast_id, instance) in state.instances.iter() {
                match instance.phase {
                    CastPhase::Channeling | CastPhase::Releasing => {
                        work.push(DispatchWork {
                            player,
                            cast_id: cast_id.clone(),
                            phase: instance.phase,
                            captured_charge: instance.captured_charge,
                        });
                    }
                    _ => {}
                }
            }
        }
    }

    for w in work {
        execute_one(world, w);
    }
}

struct DispatchWork {
    player: Entity,
    cast_id: CastId,
    phase: CastPhase,
    captured_charge: Option<f32>,
}

fn execute_one(world: &mut World, w: DispatchWork) {
    // Resolve the cast in the catalog. If the spell or cast vanished
    // (RON hot-reload removed it), abort cleanly.
    let (spell_id, cast) = {
        let catalog = world.resource::<SpellCatalog>();
        let mut found: Option<(SpellId, CastDef)> = None;
        for (id, spell) in catalog.iter() {
            if let Some(c) = spell.casts.iter().find(|c| c.id == w.cast_id) {
                found = Some((id.clone(), c.clone()));
                break;
            }
        }
        let Some(found) = found else { return; };
        found
    };

    // Conditions gate Releasing one-shots. Channeling is per-frame and
    // runs unconditionally (LensPowerAtLeast checks are inside the
    // beam/iris handlers, which look at the live LensPower component).
    if matches!(w.phase, CastPhase::Releasing)
        && !conditions_met(world, w.player, &cast.conditions)
    {
        clear_after_dispatch(world, w.player, &w.cast_id, &cast);
        return;
    }

    let ctx = CastContext {
        player: w.player,
        spell_id: spell_id.clone(),
        cast_id: w.cast_id.clone(),
        authority: Authority::Both,
        captured_charge: w.captured_charge,
        // User-initiated casts are always the chain root. `dispatch_child_cast`
        // is the only path that produces non-zero depths.
        chain_depth: 0,
        origin: cast.origin.clone(),
        target: cast.target.clone(),
    };

    // Delivery first; effect runs at the delivery's target.
    let delivery_target = run_payload(world, &ctx, &cast);
    run_effect(world, &ctx, &cast, delivery_target);

    if !matches!(w.phase, CastPhase::Channeling) {
        clear_after_dispatch(world, w.player, &w.cast_id, &cast);
    }
}

fn clear_after_dispatch(
    world: &mut World,
    player: Entity,
    cast_id: &CastId,
    cast: &CastDef,
) {
    let Ok(mut entity) = world.get_entity_mut(player) else { return; };
    let Some(mut state) = entity.get_mut::<CastState>() else { return; };
    let Some(instance) = state.instances.get_mut(cast_id) else { return; };
    if cast.cooldown > 0.0 {
        instance.cooldown_remaining = cast.cooldown;
        instance.phase = CastPhase::Cooldown;
    } else {
        instance.phase = CastPhase::Idle;
    }
    instance.captured_charge = None;
}

// --- Conditions ------------------------------------------------------------

fn conditions_met(world: &mut World, player: Entity, conditions: &[ConditionDef]) -> bool {
    for cond in conditions {
        match cond {
            ConditionDef::MaxCount { marker, max } => {
                if count_markers(world, *marker) >= *max as usize {
                    return false;
                }
            }
            ConditionDef::LensPowerAtLeast(threshold) => {
                let scalar = world
                    .get::<crate::spells::convex_lens::LensPower>(player)
                    .map(|p| p.scalar)
                    .unwrap_or(0.0);
                if scalar < *threshold {
                    return false;
                }
            }
        }
    }
    true
}

fn count_markers(world: &mut World, marker: MarkerKind) -> usize {
    match marker {
        MarkerKind::Lantern => world.query_filtered::<(), With<Lantern>>().iter(world).count(),
        MarkerKind::PortalTraveler => world
            .query_filtered::<(), With<crate::spells::portal::PortalTraveler>>()
            .iter(world)
            .count(),
    }
}

// --- Payload + Effect dispatch ---------------------------------------------

/// Result of running a payload — an entity (hit target for Hitscan/Beam,
/// spawned body for SpawnBody) plus optional direction (relevant for Beam
/// through portal where the impulse direction differs from the gaze).
#[derive(Default, Clone, Copy)]
pub struct DeliveryTarget {
    pub entity: Option<Entity>,
    pub direction: Option<Vec3>,
}

/// Extract the bounded distance from a target spec. `Forward { distance }`
/// returns its value; other variants fall back to f32::INFINITY. Used by
/// payloads that need a max raycast / projectile range.
fn target_distance(target: &TargetDef) -> f32 {
    match target {
        TargetDef::Forward { distance } => *distance,
        TargetDef::Self_ | TargetDef::Area { .. } => f32::INFINITY,
    }
}

fn run_payload(world: &mut World, ctx: &CastContext, cast: &CastDef) -> DeliveryTarget {
    // `deliveries::resolve_origin` handles all three OriginDef variants
    // (Wand reads the wand-tip; Self_ reads the caster transform;
    // Impact uses the explicit point). The payload functions below all
    // call into it, so the origin override from child-cast dispatch is
    // honored automatically.
    match &cast.payload {
        // `on_event` is resolved server-side from the catalog (via
        // SpawnBodyMessage.parent_cast); the client only needs to identify
        // the originating cast.
        PayloadDef::SpawnBody {
            template,
            launch,
            on_event: _,
        } => crate::spells::deliveries::spawn_body(
            world,
            ctx,
            template.clone(),
            launch.clone(),
        ),
        PayloadDef::Hitscan { portal_aware: _ } => {
            crate::spells::deliveries::hitscan(world, ctx, target_distance(&cast.target))
        }
        PayloadDef::Beam { portal_aware: _, visual: _ } => {
            crate::spells::deliveries::beam(world, ctx, target_distance(&cast.target))
        }
        PayloadDef::Custom { handler } => {
            let Some(handler_fn) = world.resource::<HandlerRegistry>().get(handler) else {
                warn!(
                    "Cast {:?} references unregistered handler {:?}",
                    ctx.cast_id.0, handler.0
                );
                return DeliveryTarget::default();
            };
            handler_fn(world, ctx);
            DeliveryTarget::default()
        }
    }
}

fn run_effect(
    world: &mut World,
    ctx: &CastContext,
    cast: &CastDef,
    target: DeliveryTarget,
) {
    match &cast.effect {
        EffectDef::None => {}
        EffectDef::Impulse { magnitude } => {
            crate::spells::effects::impulse(world, ctx, target, *magnitude);
        }
        EffectDef::ImpulseTick { magnitude_per_sec } => {
            crate::spells::effects::impulse_tick(world, ctx, target, *magnitude_per_sec);
        }
        EffectDef::Despawn => {
            crate::spells::effects::despawn(world, target);
        }
        EffectDef::DespawnNearbyMatching { marker, radius } => {
            crate::spells::effects::despawn_nearby_matching(world, ctx, *marker, *radius);
        }
        EffectDef::Custom { handler } => {
            let Some(handler_fn) = world
                .resource::<HandlerRegistry>()
                .get(handler)
            else {
                warn!("Cast {:?} effect references unregistered handler {:?}", ctx.cast_id.0, handler.0);
                return;
            };
            handler_fn(world, ctx);
        }
    }
}

// --- Cancel on spell change ------------------------------------------------

/// When the player switches active spell, snap any in-flight Charging /
/// Channeling instances for the previous spell back to Idle so half-charged
/// state doesn't bleed across spells.
///
/// Runs on `ActiveSpell` change; needs to know which casts belong to which
/// spell (catalog) to decide which to cancel.
pub fn cancel_on_spell_change(
    catalog: Res<SpellCatalog>,
    mut players: Query<(&ActiveSpell, &mut CastState), Changed<ActiveSpell>>,
) {
    if catalog.is_empty() {
        return;
    }
    for (active, mut state) in &mut players {
        for (spell_id, spell) in catalog.iter() {
            // Only cancel casts on SPELLS that aren't currently active. The
            // newly-active spell's casts stay in whatever state they were in
            // (which should be Idle for a freshly-activated spell).
            if active.0.eq(spell_id) {
                continue;
            }
            for cast in &spell.casts {
                if let Some(instance) = state.instances.get_mut(&cast.id) {
                    if matches!(
                        instance.phase,
                        CastPhase::Charging | CastPhase::Channeling | CastPhase::Releasing
                    ) {
                        instance.phase = CastPhase::Idle;
                        instance.charge = 0.0;
                        instance.captured_charge = None;
                    }
                }
            }
        }
    }
}

// --- Child-cast dispatch ---------------------------------------------------

/// Context for a server-dispatched child cast — one triggered by world
/// state (collision, timer, status proc) rather than user input.
/// Skips the normal cast pipeline: no PreCast validation, no Cost, no
/// Cooldown, no Charging. Goes straight to payload + effect.
///
/// The fields cover everything the cast pipeline can't infer when there's
/// no triggering player input:
/// - `origin` is the runtime world location, overriding the cast's
///   authored `OriginDef` (which would otherwise be `Wand`, useless for a
///   collision-triggered explosion).
/// - `original_caster` is propagated down the chain for damage / kill
///   attribution. `None` if the original caster has disconnected; the
///   payload + effect become orphan but still run.
/// - `captured_charge` propagates the parent cast's `captured_charge`
///   value so effects scale uniformly down the chain (e.g. a fully-charged
///   fireball produces a bigger explosion).
/// - `chain_depth` tracks how deep we are in parent → child → grandchild.
///   Hard cap of [`MAX_CHILD_CAST_CHAIN_DEPTH`] prevents cycle-bombs.
pub struct ChildCastContext {
    pub origin: OriginDef,
    pub original_caster: Option<Entity>,
    pub captured_charge: Option<f32>,
    pub chain_depth: u8,
    /// The (spell, cast) that caused this child dispatch — for trace
    /// correlation. Populated by `BodyTriggers` when a body's lifecycle
    /// fires the child; `None` for direct test dispatches.
    pub caused_by: Option<crate::spells::data::TriggerSpec>,
}

/// Hard cap on parent→child→… chain depth. Exceeding this aborts the
/// dispatch with a warn (debug build also panics to catch authoring
/// mistakes). Picked conservatively — chains > 4 are almost certainly a
/// cycle (e.g. a body's on_collision triggers a spell that spawns the
/// same body).
pub const MAX_CHILD_CAST_CHAIN_DEPTH: u8 = 4;

/// Dispatch a child cast. Returns `true` if the cast was resolved and
/// run; `false` if the spell/cast id wasn't in the catalog or the chain
/// depth exceeded the cap.
///
/// The dispatch overrides the cast's authored `OriginDef` with
/// `cctx.origin` so the payload runs from the runtime location (e.g.
/// collision point), not the original caster's wand.
pub fn dispatch_child_cast(
    world: &mut World,
    spell_id: SpellId,
    cast_id: CastId,
    cctx: ChildCastContext,
) -> bool {
    if cctx.chain_depth > MAX_CHILD_CAST_CHAIN_DEPTH {
        debug_assert!(
            false,
            "child cast chain depth {} exceeded max {} for spell {:?}/cast {:?}",
            cctx.chain_depth, MAX_CHILD_CAST_CHAIN_DEPTH, spell_id.0, cast_id.0
        );
        warn!(
            "child cast chain depth {} > {} for {:?}/{:?}; aborting (likely cycle)",
            cctx.chain_depth, MAX_CHILD_CAST_CHAIN_DEPTH, spell_id.0, cast_id.0
        );
        return false;
    }

    // Resolve the cast in the catalog. Clone it so we can override the
    // authored origin with the child's runtime origin without mutating
    // the catalog.
    let mut effective_cast: CastDef = {
        let Some(catalog) = world.get_resource::<SpellCatalog>() else {
            return false;
        };
        let Some(spell) = catalog.get(&spell_id) else {
            warn!("child cast: unknown spell {:?}", spell_id.0);
            return false;
        };
        let Some(cast) = spell.casts.iter().find(|c| c.id == cast_id) else {
            warn!(
                "child cast: spell {:?} has no cast {:?}",
                spell_id.0, cast_id.0
            );
            return false;
        };
        cast.clone()
    };
    effective_cast.origin = cctx.origin.clone();

    let ctx = CastContext {
        // For attribution: original caster propagates down the chain.
        // None → Entity::PLACEHOLDER, which queries against will reject;
        // payloads that need a caster will skip gracefully.
        player: cctx.original_caster.unwrap_or(Entity::PLACEHOLDER),
        spell_id: spell_id.clone(),
        cast_id: cast_id.clone(),
        authority: Authority::Both,
        captured_charge: cctx.captured_charge,
        chain_depth: cctx.chain_depth,
        // Effective origin / target reflect any override from cctx
        // (which already mutated effective_cast.origin above).
        origin: effective_cast.origin.clone(),
        target: effective_cast.target.clone(),
    };

    // Trace the dispatch before running it so the harness can correlate
    // parent → child timing even if the payload itself panics.
    // NOTE: `cast_kind` (not `kind`) to avoid colliding with trace::event's
    // top-level `kind` field — the extra-object merge would otherwise
    // shadow the event name.
    let (caused_by_spell, caused_by_cast) = match &cctx.caused_by {
        Some(spec) => (Some(spec.spell_id.0.clone()), Some(spec.cast_id.0.clone())),
        None => (None, None),
    };
    crate::trace::event(
        "cast_dispatched",
        serde_json::json!({
            "spell": spell_id.0,
            "cast":  cast_id.0,
            "cast_kind":  "child",
            "chain_depth": cctx.chain_depth,
            "charge": cctx.captured_charge,
            "caused_by_spell": caused_by_spell,
            "caused_by_cast":  caused_by_cast,
        }),
    );

    // Note: we deliberately do NOT consult `effective_cast.conditions`,
    // `effective_cast.cost`, or `effective_cast.cooldown`. Child casts are
    // mechanical — the user (parent) already paid.
    let delivery_target = run_payload(world, &ctx, &effective_cast);
    run_effect(world, &ctx, &effective_cast, delivery_target);
    true
}

// --- Bootstrap: add PrevActionSnapshot to players when CastState appears ---

pub fn add_action_snapshot_to_players(
    mut commands: Commands,
    q: Query<Entity, (With<Player>, With<CastState>, Without<PrevActionSnapshot>)>,
) {
    for entity in &q {
        commands.entity(entity).insert(PrevActionSnapshot::default());
    }
}

// Suppress dead-code warnings for the SystemState placeholder we don't yet need.
#[allow(dead_code)]
fn _suppress_systemstate_warning(_world: &mut World) {
    let _: SystemState<()>;
}

#[cfg(test)]
mod child_cast_tests {
    use super::*;
    use crate::spells::catalog::{BodyCatalog, SpellCatalog};
    use crate::spells::data::{
        ActionRef, AreaShape, CastDef, ChargingDef, HandlerId, PayloadDef, SpellDef,
        TargetDef, TriggerDef,
    };
    use crate::spells::handlers::HandlerRegistry;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    /// Snapshot of the things a probe handler captured from CastContext.
    /// Wider than `Option<f32>` so tests can assert origin/target/depth
    /// propagation too.
    #[derive(Default, Clone)]
    struct ProbeSnapshot {
        captured_charge: Option<f32>,
        origin: Option<OriginDef>,
        target: Option<TargetDef>,
        chain_depth: Option<u8>,
    }

    fn probe_spell(
        spell_id: &str,
        cast_id: &str,
        handler: &str,
        target: TargetDef,
    ) -> SpellDef {
        SpellDef {
            id: SpellId::new(spell_id),
            label: "probe".into(),
            icon: None,
            casts: vec![CastDef {
                id: CastId(cast_id.into()),
                label: "probe".into(),
                requires_active: false,
                trigger: TriggerDef::Press {
                    action: ActionRef::Fire,
                },
                conditions: vec![],
                cost: vec![],
                charging: ChargingDef::None,
                cooldown: 0.0,
                origin: OriginDef::Wand,
                target,
                payload: PayloadDef::Custom {
                    handler: HandlerId(handler.into()),
                },
                effect: EffectDef::None,
            }],
        }
    }

    fn minimal_world() -> (App, Arc<AtomicU32>, Arc<Mutex<ProbeSnapshot>>) {
        minimal_world_with_target(TargetDef::default())
    }

    fn minimal_world_with_target(
        target: TargetDef,
    ) -> (App, Arc<AtomicU32>, Arc<Mutex<ProbeSnapshot>>) {
        let mut app = App::new();
        app.init_resource::<HandlerRegistry>();
        app.init_resource::<SpellCatalog>();
        app.init_resource::<BodyCatalog>();

        let calls = Arc::new(AtomicU32::new(0));
        let snapshot = Arc::new(Mutex::new(ProbeSnapshot::default()));
        let calls_h = calls.clone();
        let snapshot_h = snapshot.clone();

        let mut registry = app.world_mut().resource_mut::<HandlerRegistry>();
        registry.register(
            HandlerId("test.probe".to_string()),
            move |_world: &mut World, ctx: &CastContext| {
                calls_h.fetch_add(1, Ordering::SeqCst);
                let mut s = snapshot_h.lock().unwrap();
                s.captured_charge = ctx.captured_charge;
                s.origin = Some(ctx.origin.clone());
                s.target = Some(ctx.target.clone());
                s.chain_depth = Some(ctx.chain_depth);
            },
        );

        let spell = probe_spell("test_spell", "test_spell.probe", "test.probe", target);
        app.world_mut()
            .resource_mut::<SpellCatalog>()
            .insert_for_test(spell);
        (app, calls, snapshot)
    }

    #[test]
    fn dispatch_child_cast_runs_custom_handler() {
        let (mut app, calls, captured) = minimal_world();
        let dispatched = dispatch_child_cast(
            app.world_mut(),
            SpellId::new("test_spell"),
            CastId("test_spell.probe".into()),
            ChildCastContext {
                origin: OriginDef::Impact(Vec3::new(1.0, 2.0, 3.0)),
                original_caster: None,
                captured_charge: Some(2.5),
                chain_depth: 0,
                caused_by: None,
            },
        );
        assert!(dispatched, "expected dispatch_child_cast to succeed");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let snap = captured.lock().unwrap();
        assert_eq!(snap.captured_charge, Some(2.5));
        assert!(matches!(
            snap.origin,
            Some(OriginDef::Impact(p)) if p == Vec3::new(1.0, 2.0, 3.0)
        ));
        assert_eq!(snap.chain_depth, Some(0));
    }

    #[test]
    fn dispatch_child_cast_propagates_area_target_to_handler() {
        let (mut app, _calls, snapshot) =
            minimal_world_with_target(TargetDef::Area {
                shape: AreaShape::Sphere { radius: 3.5 },
            });
        let dispatched = dispatch_child_cast(
            app.world_mut(),
            SpellId::new("test_spell"),
            CastId("test_spell.probe".into()),
            ChildCastContext {
                origin: OriginDef::Impact(Vec3::ZERO),
                original_caster: None,
                captured_charge: None,
                chain_depth: 2,
                caused_by: None,
            },
        );
        assert!(dispatched);
        let snap = snapshot.lock().unwrap();
        assert_eq!(snap.chain_depth, Some(2));
        assert!(matches!(
            snap.target,
            Some(TargetDef::Area {
                shape: AreaShape::Sphere { radius }
            }) if (radius - 3.5).abs() < 1e-6
        ));
    }

    #[test]
    fn dispatch_child_cast_returns_false_when_chain_too_deep() {
        let (mut app, calls, _) = minimal_world();
        // Cap is 4; passing 5 must abort. Use catch_unwind to swallow the
        // debug_assert! panic — release builds skip the assert and the
        // function just returns false.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dispatch_child_cast(
                app.world_mut(),
                SpellId::new("test_spell"),
                CastId("test_spell.probe".into()),
                ChildCastContext {
                    origin: OriginDef::Wand,
                    original_caster: None,
                    captured_charge: None,
                    chain_depth: 5,
                    caused_by: None,
                },
            )
        }));
        // Either path is acceptable: panic (debug) or `false` return (release).
        match result {
            Ok(dispatched) => assert!(!dispatched, "expected abort"),
            Err(_) => { /* debug-build panic from debug_assert */ }
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "handler must not run when depth exceeded"
        );
    }

    #[test]
    fn dispatch_child_cast_returns_false_for_unknown_spell() {
        let (mut app, calls, _) = minimal_world();
        let dispatched = dispatch_child_cast(
            app.world_mut(),
            SpellId::new("nonexistent_spell"),
            CastId("nonexistent_spell.cast".into()),
            ChildCastContext {
                origin: OriginDef::Wand,
                original_caster: None,
                captured_charge: None,
                chain_depth: 0,
                caused_by: None,
            },
        );
        assert!(!dispatched);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
