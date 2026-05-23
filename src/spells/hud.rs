//! Shared per-cast HUD widgets. Generic so any data-driven spell with a
//! `ChargingDef::Charging` gets visual feedback "for free" — no Rust code
//! per spell.

use bevy::prelude::*;

use crate::player::LocalPlayer;
use crate::spells::catalog::SpellCatalog;
use crate::spells::data::ChargingDef;
use crate::spells::engine::CastState;
use crate::spells::ActiveSpell;
use crate::ui::hud::HudCenter;

pub struct CastHudPlugin;

impl Plugin for CastHudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostStartup, spawn_charge_bar)
            .add_systems(Update, update_charge_bar);
    }
}

#[derive(Component)]
struct ChargeBar;

#[derive(Component)]
struct ChargeBarFill;

const W: f32 = 120.0;
const H: f32 = 6.0;
/// Vertical position relative to crosshair center. Below the lumen text,
/// where iris's bespoke bar used to live.
const TOP_PX: f32 = 124.0;

fn spawn_charge_bar(mut commands: Commands, center: Single<Entity, With<HudCenter>>) {
    commands.entity(*center).with_children(|c| {
        c.spawn((
            ChargeBar,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(-W * 0.5),
                top: Val::Px(TOP_PX),
                width: Val::Px(W),
                height: Val::Px(H),
                display: Display::None,
                ..default()
            },
            BackgroundColor(Color::srgba(0.15, 0.15, 0.18, 0.7)),
            children![(
                ChargeBarFill,
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    top: Val::Px(0.0),
                    width: Val::Px(0.0),
                    height: Val::Px(H),
                    ..default()
                },
                BackgroundColor(Color::srgba(1.0, 0.8, 0.4, 0.9)),
            )],
        ));
    });
}

/// Find the first cast on the local player's active spell that has
/// `ChargingDef::Charging`, and render its charge ratio. Hide otherwise.
/// `full` lights up the bar when charge ≥ min_release — i.e. when
/// releasing would actually fire the cast (vs. fizzle).
fn update_charge_bar(
    catalog: Res<SpellCatalog>,
    player: Option<Single<(&ActiveSpell, &CastState), With<LocalPlayer>>>,
    mut bars: Query<&mut Node, (With<ChargeBar>, Without<ChargeBarFill>)>,
    mut fills: Query<(&mut Node, &mut BackgroundColor), With<ChargeBarFill>>,
) {
    let mut display: Option<(f32, bool)> = None; // (ratio, full)

    if let Some(player) = player {
        let (active, cast_state) = *player;
        if let Some(spell) = catalog.get(&active.0) {
            for cast in &spell.casts {
                let ChargingDef::Charging {
                    max_charge,
                    min_release,
                    overcharge,
                    ..
                } = &cast.charging
                else {
                    continue;
                };
                // Bar maxes at the hard cap so overcharge is visualized as
                // "beyond full." Below min_release, the bar reads "not
                // ready yet" via the `full` flag.
                let display_max = overcharge
                    .as_ref()
                    .map(|o| o.max)
                    .unwrap_or(*max_charge);
                let charge = cast_state
                    .instances
                    .get(cast.id.0.as_str())
                    .map(|i| i.charge)
                    .unwrap_or(0.0);
                if charge > 0.0 {
                    let ratio = (charge / display_max.max(1e-6)).clamp(0.0, 1.0);
                    display = Some((ratio, charge >= *min_release));
                    break;
                }
                // Even at 0 charge, if the cast is charged-capable on the
                // active spell, show the empty bar so the player knows
                // it exists. First charged cast wins.
                if display.is_none() {
                    display = Some((0.0, false));
                }
            }
        }
    }

    let Some((ratio, full)) = display else {
        for mut node in &mut bars {
            node.display = Display::None;
        }
        return;
    };

    for mut node in &mut bars {
        node.display = Display::Flex;
    }
    for (mut node, mut color) in &mut fills {
        node.width = Val::Px(W * ratio);
        let warmth = if full { 1.0 } else { ratio };
        color.0 = Color::srgba(1.0, 0.75 + 0.25 * warmth, 0.4 + 0.5 * warmth, 0.95);
    }
}
