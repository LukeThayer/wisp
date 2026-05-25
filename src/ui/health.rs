//! Health HUD: bottom-left HP bar + damage / low-health vignette.
//! Both fed by the locally-owned `NetworkedPlayer`'s `NetworkedHealth`.
//!
//! Server is authoritative for hp (see `src/spells/damage.rs`); the client
//! never writes hp, only reads `NetworkedHealth` for display.
//!
//! Local player resolution mirrors `replication::hide_self_wizard_body`:
//! pull the lightyear `LocalId`, match it against `NetworkOwner` on the
//! replicated `NetworkedPlayer` entities. If no match yet (pre-spawn),
//! the bar hides.
//!
//! Vignette: a full-screen UI node with a RadialGradient that's
//! transparent in the inner ~50% of the viewport and red at the
//! corners. Alpha is driven by two intensity sources:
//! - **Damage flash**: each hp drop sets `flash_timer = FLASH_DURATION`,
//!   linearly decays back to 0. Brief spike independent of current hp.
//! - **Low-health pulse**: when `hp/max_hp <= LOW_HP_RATIO`, a
//!   sine-modulated baseline intensity rides under the flash so the
//!   edges keep breathing red until the player heals (respawns).

use bevy::prelude::*;
use bevy::ui::{
    BackgroundGradient, ColorStop, Gradient, RadialGradient, RadialGradientShape, UiPosition,
};
use lightyear::prelude::{LocalId, PeerId};
use lightyear::prelude::server::ClientOf;

use crate::net::protocol::{NetworkOwner, NetworkedHealth, NetworkedPlayer};
use crate::ui::hud::HudRoot;

pub struct HealthHudPlugin;

impl Plugin for HealthHudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostStartup, (spawn_health_bar, spawn_damage_vignette))
            .add_systems(Update, (update_health_bar, update_damage_vignette));
    }
}

#[derive(Component)]
struct HealthBar;

#[derive(Component)]
struct HealthBarFill;

#[derive(Component)]
struct HealthBarText;

/// Width and height of the bar (px). Bumped to 320×22 so the number
/// inside is comfortably legible and the bar reads as a primary HUD
/// element next to the new weapon panel.
pub const W: f32 = 320.0;
pub const H: f32 = 22.0;
pub const MARGIN: f32 = 20.0;

fn spawn_health_bar(
    mut commands: Commands,
    hud: Option<Single<Entity, With<HudRoot>>>,
) {
    let Some(hud) = hud else {
        // Server build doesn't run UiPlugin (no DefaultPlugins → no
        // window → no HUD root). HealthHudPlugin is added by the
        // shared UiPlugin so the bin choice already gates this; the
        // guard is here just for headless tests.
        return;
    };
    commands.entity(*hud).with_children(|root| {
        root.spawn((
            HealthBar,
            Name::new("HealthBar"),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(MARGIN),
                bottom: Val::Px(MARGIN),
                width: Val::Px(W),
                height: Val::Px(H),
                // Hidden until we resolve a local NetworkedPlayer with
                // NetworkedHealth. Avoids a flash of "0/0" on connect.
                display: Display::None,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(Color::srgba(0.08, 0.08, 0.08, 0.85)),
            children![(
                HealthBarFill,
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    top: Val::Px(0.0),
                    width: Val::Px(W),
                    height: Val::Px(H),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.85, 0.18, 0.18, 0.95)),
            )],
        ))
        .with_children(|bar| {
            bar.spawn((
                HealthBarText,
                Text::new("100 / 100"),
                TextFont {
                    font_size: 16.0,
                    ..default()
                },
                TextColor(Color::srgba(1.0, 1.0, 1.0, 0.95)),
                Node {
                    position_type: PositionType::Absolute,
                    ..default()
                },
                // Sit above the fill child so the number stays legible.
                ZIndex(1),
            ));
        });
    });
}

/// Resolve the local player's NetworkedPlayer entity via LocalId →
/// NetworkOwner, read its NetworkedHealth, and update the bar fill +
/// text. Hides when no match (pre-spawn, post-disconnect).
fn update_health_bar(
    local_ids: Query<&LocalId, Without<ClientOf>>,
    players: Query<(&NetworkOwner, &NetworkedHealth), With<NetworkedPlayer>>,
    mut bars: Query<&mut Node, (With<HealthBar>, Without<HealthBarFill>, Without<HealthBarText>)>,
    mut fills: Query<&mut Node, (With<HealthBarFill>, Without<HealthBar>, Without<HealthBarText>)>,
    mut texts: Query<&mut Text, With<HealthBarText>>,
) {
    let my_id = local_ids
        .iter()
        .next()
        .and_then(|local_id| match local_id.0 {
            PeerId::Netcode(id) | PeerId::Steam(id) | PeerId::Local(id) | PeerId::Entity(id) => {
                Some(id)
            }
            _ => None,
        });

    let health = my_id.and_then(|my_id| {
        players
            .iter()
            .find(|(owner, _)| owner.0 == my_id)
            .map(|(_, hp)| *hp)
    });

    let Some(health) = health else {
        for mut node in &mut bars {
            node.display = Display::None;
        }
        return;
    };

    for mut node in &mut bars {
        node.display = Display::Flex;
    }
    let ratio = if health.max_hp > 0.0 {
        (health.hp / health.max_hp).clamp(0.0, 1.0)
    } else {
        0.0
    };
    for mut node in &mut fills {
        node.width = Val::Px(W * ratio);
    }
    let hp_int = health.hp.round() as i32;
    let max_int = health.max_hp.round() as i32;
    for mut text in &mut texts {
        text.0 = format!("{hp_int} / {max_int}");
    }
}

// --- Damage / low-health vignette ------------------------------------------

#[derive(Component)]
struct DamageVignette {
    /// Seconds remaining on the per-hit damage flash. 0 = no flash.
    flash_timer: f32,
    /// Last observed hp; used to detect drops so we don't re-trigger
    /// the flash on every frame just because hp < max.
    prev_hp: Option<f32>,
}

/// Duration of a single damage flash. Short — it should read as a
/// punctuation, not an obscuring overlay.
const FLASH_DURATION: f32 = 0.45;
/// Peak alpha of the corner red color during a fresh flash.
const FLASH_PEAK_ALPHA: f32 = 0.85;

/// Threshold (fraction of max_hp) at which the persistent low-hp
/// vignette kicks in. 30% feels right — clearly "in danger" but not
/// flashing on every chip of damage.
const LOW_HP_RATIO: f32 = 0.30;
/// Baseline edge alpha at low hp (sine carrier center).
const LOW_HP_BASE_ALPHA: f32 = 0.30;
/// Sine amplitude around the baseline for the pulse.
const LOW_HP_PULSE_AMPL: f32 = 0.18;
/// Pulse frequency in Hz.
const LOW_HP_PULSE_HZ: f32 = 1.6;

/// Hue used for both flash and low-hp glow. Slightly orange-red so the
/// edges read as "blood" rather than emergency-red.
const VIGNETTE_R: f32 = 0.85;
const VIGNETTE_G: f32 = 0.08;
const VIGNETTE_B: f32 = 0.08;

fn spawn_damage_vignette(
    mut commands: Commands,
    hud: Option<Single<Entity, With<HudRoot>>>,
) {
    let Some(hud) = hud else { return; };
    commands.entity(*hud).with_children(|root| {
        root.spawn((
            DamageVignette { flash_timer: 0.0, prev_hp: None },
            Name::new("DamageVignette"),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                top: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                ..default()
            },
            // Initial transparent gradient — `update_damage_vignette`
            // overwrites the last stop's alpha each frame.
            BackgroundGradient(vec![Gradient::Radial(vignette_gradient(0.0))]),
            // Render above everything else in the HUD layer; the
            // crosshair + bars sit at default ZIndex(0).
            ZIndex(10),
            // Pointer-events: pass-through. The full-screen Node
            // would otherwise eat clicks intended for game-world UI
            // (radial menu, customization screen).
            Pickable::IGNORE,
        ));
    });
}

/// Build a radial gradient with a transparent core that fades to red
/// at the corners. `alpha` controls the corner intensity (0 = no
/// vignette, 1 = saturated red). Stops are picked so the inner ~50%
/// stays fully clear — keeps the crosshair, prop pile, and HUD bars
/// unobscured.
fn vignette_gradient(alpha: f32) -> RadialGradient {
    let clear = Color::srgba(VIGNETTE_R, VIGNETTE_G, VIGNETTE_B, 0.0);
    let red = Color::srgba(VIGNETTE_R, VIGNETTE_G, VIGNETTE_B, alpha.clamp(0.0, 1.0));
    RadialGradient::new(
        UiPosition::CENTER,
        // FarthestCorner gives a stable shape regardless of aspect
        // ratio — the gradient always reaches alpha at the corner.
        RadialGradientShape::FarthestCorner,
        vec![
            ColorStop::percent(clear, 0.0),
            ColorStop::percent(clear, 50.0),
            ColorStop::percent(red, 100.0),
        ],
    )
}

fn update_damage_vignette(
    time: Res<Time>,
    local_ids: Query<&LocalId, Without<ClientOf>>,
    players: Query<(&NetworkOwner, &NetworkedHealth), With<NetworkedPlayer>>,
    mut vignettes: Query<(&mut DamageVignette, &mut BackgroundGradient)>,
) {
    let dt = time.delta_secs();
    let elapsed = time.elapsed_secs();

    let my_id = local_ids
        .iter()
        .next()
        .and_then(|local_id| match local_id.0 {
            PeerId::Netcode(id) | PeerId::Steam(id) | PeerId::Local(id) | PeerId::Entity(id) => {
                Some(id)
            }
            _ => None,
        });
    let health: Option<NetworkedHealth> = my_id.and_then(|my_id| {
        players
            .iter()
            .find(|(owner, _)| owner.0 == my_id)
            .map(|(_, hp)| *hp)
    });

    for (mut state, mut bg) in &mut vignettes {
        // Resolve current/prev hp so we can detect drops. On the very
        // first observed value, seed prev_hp without triggering a flash.
        let alpha = if let Some(h) = health {
            let prev = state.prev_hp.unwrap_or(h.hp);
            // Damage = strictly positive drop in hp. We deliberately
            // ignore upward jumps (respawn refill) — those reset the
            // baseline silently.
            if h.hp + 0.001 < prev {
                state.flash_timer = FLASH_DURATION;
            }
            state.prev_hp = Some(h.hp);

            // Decay the flash timer toward zero.
            state.flash_timer = (state.flash_timer - dt).max(0.0);
            let flash_progress = state.flash_timer / FLASH_DURATION;
            let flash_alpha = flash_progress * FLASH_PEAK_ALPHA;

            // Low-hp baseline pulse.
            let ratio = if h.max_hp > 0.0 {
                (h.hp / h.max_hp).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let low_alpha = if ratio > 0.0 && ratio <= LOW_HP_RATIO {
                let phase = (elapsed * core::f32::consts::TAU * LOW_HP_PULSE_HZ).sin();
                (LOW_HP_BASE_ALPHA + LOW_HP_PULSE_AMPL * phase).max(0.0)
            } else {
                0.0
            };

            // Combine: flash sits on top of the low-hp baseline. We
            // sum-and-clamp rather than max so a fresh hit during
            // low-hp still feels louder than the pulse alone.
            (flash_alpha + low_alpha).clamp(0.0, 1.0)
        } else {
            // No local player resolved yet → reset everything and
            // hide the vignette.
            state.flash_timer = 0.0;
            state.prev_hp = None;
            0.0
        };

        // Overwrite the gradient. Single-stop edit would be cheaper,
        // but a full rebuild keeps the code linear and matches the
        // spawn-time builder.
        bg.0 = vec![Gradient::Radial(vignette_gradient(alpha))];
    }
}
