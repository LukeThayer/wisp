//! Data types for the cast framework. These match the RON files in
//! `assets/spells/*.spell.ron` and `assets/bodies/*.body.ron` one-to-one.
//!
//! All types use closed enums for first-class concepts (no open strings for
//! enum variants); IDs (SpellId, CastId, BodyTemplateId, HandlerId,
//! BeamVisualId) are newtypes around String so authors can use friendly
//! names while keeping the framework's keying typed.

use bevy::prelude::*;
use serde::Deserialize;

pub use crate::spells::SpellId;

// --- Identifiers -----------------------------------------------------------

#[derive(Deserialize, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub struct CastId(pub String);

impl std::borrow::Borrow<str> for CastId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

#[derive(Deserialize, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub struct BodyTemplateId(pub String);

#[derive(Deserialize, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub struct BeamVisualId(pub String);

#[derive(Deserialize, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub struct HandlerId(pub String);

// --- Top-level: SpellDef + CastDef -----------------------------------------

#[derive(Asset, TypePath, Deserialize, Clone, Debug)]
pub struct SpellDef {
    pub id: SpellId,
    pub label: String,
    #[serde(default)]
    pub icon: Option<String>,
    pub casts: Vec<CastDef>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct CastDef {
    pub id: CastId,
    pub label: String,
    /// When true, this cast only fires if the player has this spell as their
    /// active slot. Default true — opt-out for passive abilities.
    #[serde(default = "default_true")]
    pub requires_active: bool,
    pub trigger: TriggerDef,
    #[serde(default)]
    pub conditions: Vec<ConditionDef>,
    #[serde(default)]
    pub cost: Vec<CostDef>,
    /// Unified charging model: wind-up gate (`Press` requires reaching
    /// `min_release` before release commits), release-charged scaling
    /// (`PressReleaseCharged` captures whatever charge was held), and
    /// `Hold` channel wind-up (promotes to Channeling at `max_charge`).
    /// Default `None` fires immediately on the trigger's natural edge.
    #[serde(default)]
    pub charging: ChargingDef,
    #[serde(default)]
    pub cooldown: f32,
    /// Where the cast emanates from in world space. Default `Wand`.
    #[serde(default)]
    pub origin: OriginDef,
    /// Where the cast aims / what it acts on. Default `Forward(infinity)`.
    #[serde(default)]
    pub target: TargetDef,
    /// The physical mechanism — projectile, hitscan, beam, custom handler.
    pub payload: PayloadDef,
    #[serde(default)]
    pub effect: EffectDef,
}

fn default_true() -> bool {
    true
}

// --- Triggers / actions -----------------------------------------------------

#[derive(Deserialize, Clone, Debug)]
pub enum TriggerDef {
    /// One-shot at the moment of press.
    Press { action: ActionRef },
    /// Ticks every frame while the action is held.
    Hold { action: ActionRef },
    /// Press begins charging; release fires with the captured charge.
    PressReleaseCharged { action: ActionRef },
}

impl TriggerDef {
    pub fn action(&self) -> ActionRef {
        match self {
            TriggerDef::Press { action } => *action,
            TriggerDef::Hold { action } => *action,
            TriggerDef::PressReleaseCharged { action } => *action,
        }
    }
}

/// Names the bei action this cast subscribes to. Closed enum so typos in RON
/// fail at load time rather than silently doing nothing.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActionRef {
    Fire,
    FireSecondary,
    ThrowLantern,
    Pickup,
    Jump,
}

// --- Charging ---------------------------------------------------------------

#[derive(Deserialize, Clone, Debug, Default)]
pub enum ChargingDef {
    #[default]
    None,
    /// Wind-up or release-charged accumulation. The interpretation depends
    /// on the cast's `trigger`:
    ///
    /// - **Press**: enters Charging on press; on release fires iff charge ≥
    ///   `min_release` (else fizzles to Idle). For a classic wind-up gate
    ///   ("must hold to full"), set `min_release == max_charge`.
    /// - **Hold**: enters Charging on press; promotes to Channeling when
    ///   charge ≥ `max_charge`. `min_release` is ignored. Release before
    ///   `max_charge` cancels to Idle.
    /// - **PressReleaseCharged**: enters Charging on press; on release
    ///   fires with `captured_charge = current charge` iff ≥ `min_release`.
    ///   Default `min_release` of 0 means any release fires.
    Charging {
        source: ChargeSource,
        /// Minimum charge required to commit on release. Below this:
        /// fizzle to Idle without firing. Default 0.
        #[serde(default)]
        min_release: f32,
        /// Soft cap. For Hold triggers, the threshold for promotion to
        /// Channeling. For other triggers, the natural full state.
        max_charge: f32,
        /// Charge lost per second when the source isn't providing (Powered
        /// case: source scalar is zero). Has no effect on Time-sourced
        /// charging today.
        #[serde(default)]
        decay_rate: f32,
        /// Opt-in: allows charge to exceed `max_charge` up to
        /// `overcharge.max`. The payload reads `captured_charge` and
        /// scales accordingly (e.g. bigger explosion radius). Default
        /// None — charge clamps at `max_charge`.
        #[serde(default)]
        overcharge: Option<OverchargeDef>,
    },
}

#[derive(Deserialize, Clone, Debug)]
pub enum ChargeSource {
    /// Linear time-based fill. Charge ramps from 0 to `max_charge` over
    /// `duration` seconds. Overcharge zone (if any) fills at the same rate.
    Time { duration: f32 },
    /// Charge accumulates from a power source's scalar (e.g. LensPower) at
    /// `source.scalar * 1.0` per second. Decays at `decay_rate/s` when the
    /// source is dry.
    Powered { source: PowerSourceRef },
}

#[derive(Deserialize, Clone, Debug)]
pub struct OverchargeDef {
    /// Hard cap above `max_charge`. Charge keeps accumulating up to here.
    pub max: f32,
    // cost_per_sec: TODO — wire when CostDef has real variants. Today
    // CostDef is empty so a required field can't be expressed in RON;
    // payload scaling via `captured_charge` is the only overcharge knob.
}

/// Power sources that the `Powered` charge mode can read. Closed enum —
/// add variants here when adding new resource concepts.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PowerSourceRef {
    LensPower,
}

// --- Conditions -------------------------------------------------------------

#[derive(Deserialize, Clone, Debug)]
pub enum ConditionDef {
    /// Fail if the count of entities carrying `marker` is at or above `max`.
    MaxCount { marker: MarkerKind, max: u32 },
    /// Fail if the casting player's LensPower scalar is below the threshold.
    LensPowerAtLeast(f32),
}

// --- Costs (placeholder — populate as needed) -------------------------------

#[derive(Deserialize, Clone, Debug)]
pub enum CostDef {
    // Intentionally empty. Add variants only when a real cost mechanic lands
    // (mana, charges, consumable). Today, costs are expressed through
    // `ConditionDef::MaxCount` (e.g. lantern's 3-cap).
}

// --- Origin / Target / Payload ---------------------------------------------

#[derive(Deserialize, Clone, Debug, Default)]
pub enum OriginDef {
    /// Wand-tip anchor (PlayerRig.lens_anchor). The default for
    /// player-initiated casts.
    #[default]
    Wand,
    /// Caster's transform origin (player root position). Hook for
    /// future self-emanating casts; not yet load-bearing.
    Self_,
    /// A specific world point. Constructed by `ChildCastDispatch` when a
    /// parent spell's body triggers a child cast (e.g. fireball → explosion
    /// at impact). Not part of RON — `#[serde(skip)]` keeps it out of the
    /// parser; only Rust code can produce it.
    #[serde(skip)]
    Impact(Vec3),
}

#[derive(Deserialize, Clone, Debug)]
pub enum TargetDef {
    /// Aim along the wand-forward direction. `distance` caps the
    /// effective range (raycast max_range, projectile lifetime, etc.).
    /// Use `f32::INFINITY` for unbounded — the default.
    Forward { distance: f32 },
    /// Self-target — payload (if any) acts on the caster directly. Hook
    /// for future self-cast spells; not yet load-bearing.
    Self_,
    /// Area around the origin. Reserved hook for child casts (e.g. an
    /// explosion AoE). Built-in payloads ignore this today; consumed
    /// when `OriginDef::Impact` lands in task #5.
    Area { shape: AreaShape },
}

impl Default for TargetDef {
    fn default() -> Self {
        TargetDef::Forward { distance: f32::INFINITY }
    }
}

#[derive(Deserialize, Clone, Debug)]
pub enum AreaShape {
    Sphere { radius: f32 },
}

#[derive(Deserialize, Clone, Debug)]
pub enum PayloadDef {
    /// Spawn a physical body at origin, launched per `launch`. Server-
    /// authoritative — sends `SpawnBodyMessage`; the local caster sees
    /// their own projectile through the replication round-trip.
    /// `on_event` declares body-lifecycle hooks that dispatch child casts
    /// (e.g. `OnCollision` for an explosion). Server attaches the
    /// `BodyTriggers` component to the spawned entity; collisions feed
    /// `dispatch_child_cast`.
    SpawnBody {
        template: BodyTemplateId,
        launch: LaunchKind,
        #[serde(default)]
        on_event: Vec<BodyEventTrigger>,
    },
    /// One-shot raycast from origin along target.Forward. The first hit
    /// becomes the delivery target. `portal_aware` is reserved for
    /// future portal-bend logic; built-in path doesn't bend yet.
    Hitscan {
        #[serde(default)]
        portal_aware: bool,
    },
    /// Continuous per-frame raycast while the cast is Channeling.
    /// Built-in beam doesn't spawn a mesh — that lives in spell-specific
    /// custom handlers (convex_lens, iris) for now.
    Beam {
        #[serde(default)]
        portal_aware: bool,
        visual: BeamVisualId,
    },
    /// Defer to bespoke code registered under this handler id.
    Custom { handler: HandlerId },
}

/// Body-lifecycle hook that triggers a child cast when the spawned body
/// reaches a given state. Attached to the body server-side via
/// `BodyTriggers`. Cleanup: triggers fire once then despawn the body.
#[derive(Deserialize, Clone, Debug)]
pub enum BodyEventTrigger {
    /// First avian `CollisionStart` involving this body fires the trigger.
    OnCollision { triggers: TriggerSpec },
    /// `secs` seconds after spawn the trigger fires regardless of state.
    /// Useful for fuses and timed bombs.
    OnTimeout { secs: f32, triggers: TriggerSpec },
}

/// Names which spell + cast to dispatch when a body trigger fires. Both
/// IDs are required — `SpellId` keys the catalog entry; `CastId` keys the
/// specific cast within that spell.
#[derive(Deserialize, Clone, Debug)]
pub struct TriggerSpec {
    pub spell_id: SpellId,
    pub cast_id: CastId,
}

#[derive(Deserialize, Clone, Debug)]
pub enum LaunchKind {
    /// `forward` along the wand-forward axis, `up` along world up.
    Throw { forward: f32, up: f32 },
    /// Drop at the origin, no extra velocity.
    Drop,
    /// Held / carried (no launch).
    Held,
}

// --- Effect -----------------------------------------------------------------

#[derive(Deserialize, Clone, Debug, Default)]
pub enum EffectDef {
    #[default]
    None,
    /// Single impulse at the delivered target. If the cast has a
    /// `captured_charge` value, magnitude is auto-scaled by it.
    Impulse { magnitude: f32 },
    /// Continuous impulse per second while channeling.
    ImpulseTick { magnitude_per_sec: f32 },
    /// Despawn whatever the delivery hit.
    Despawn,
    /// Despawn any entity within `radius` of the caster that carries `marker`.
    DespawnNearbyMatching { marker: MarkerKind, radius: f32 },
    /// Defer to bespoke code registered under this handler id.
    Custom { handler: HandlerId },
}

// --- Markers ----------------------------------------------------------------

/// First-class identity tags that bodies can carry, that conditions can query,
/// and that effects can match on. Closed enum so typos fail at load.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkerKind {
    Lantern,
    PortalTraveler,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(path: &str) -> SpellDef {
        let s = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        ron::from_str::<SpellDef>(&s).unwrap_or_else(|e| panic!("parse {path}: {e}"))
    }

    #[test]
    fn iris_parses_to_powered_charging() {
        let spell = parse("assets/spells/iris.spell.ron");
        assert_eq!(spell.id.0, "iris");
        assert_eq!(spell.casts.len(), 1);
        let cast = &spell.casts[0];
        match &cast.charging {
            ChargingDef::Charging {
                source,
                max_charge,
                decay_rate,
                min_release,
                overcharge,
            } => {
                assert!(matches!(
                    source,
                    ChargeSource::Powered { source: PowerSourceRef::LensPower }
                ));
                assert_eq!(*max_charge, 3.0);
                assert_eq!(*decay_rate, 2.0);
                assert_eq!(*min_release, 0.0);
                assert!(overcharge.is_none());
            }
            other => panic!("expected Charging, got {other:?}"),
        }
        assert!(matches!(cast.trigger, TriggerDef::PressReleaseCharged { .. }));
    }

    #[test]
    fn stone_toss_parses_to_time_charging() {
        let spell = parse("assets/spells/stone_toss.spell.ron");
        let cast = &spell.casts[0];
        match &cast.charging {
            ChargingDef::Charging {
                source,
                max_charge,
                min_release,
                ..
            } => {
                assert!(matches!(source, ChargeSource::Time { duration } if (*duration - 0.5).abs() < 1e-6));
                assert_eq!(*max_charge, 1.0);
                assert_eq!(*min_release, 1.0);
            }
            other => panic!("expected Charging, got {other:?}"),
        }
        // Origin/Target default to Wand / Forward(infinity) when omitted.
        assert!(matches!(cast.origin, OriginDef::Wand));
        assert!(matches!(
            cast.target,
            TargetDef::Forward { distance } if distance.is_infinite()
        ));
        assert!(matches!(cast.payload, PayloadDef::SpawnBody { .. }));
    }

    #[test]
    fn slam_pulse_parses_to_hitscan_with_forward_distance() {
        let spell = parse("assets/spells/slam_pulse.spell.ron");
        let cast = &spell.casts[0];
        assert!(matches!(
            cast.target,
            TargetDef::Forward { distance } if (distance - 8.0).abs() < 1e-6
        ));
        assert!(matches!(cast.payload, PayloadDef::Hitscan { .. }));
    }
}
