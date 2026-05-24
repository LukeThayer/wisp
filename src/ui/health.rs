//! Health HUD: bottom-left HP bar fed by the locally-owned
//! `NetworkedPlayer`'s `NetworkedHealth`.
//!
//! Server is authoritative for hp (see `src/spells/damage.rs`); the client
//! never writes hp, only reads `NetworkedHealth` for display. The bar is
//! anchored to `HudRoot` (full-screen) with absolute bottom-left
//! positioning so it sits outside the cast HUD's centered cluster.
//!
//! Local player resolution mirrors `replication::hide_self_wizard_body`:
//! pull the lightyear `LocalId`, match it against `NetworkOwner` on the
//! replicated `NetworkedPlayer` entities. If no match yet (pre-spawn),
//! the bar hides.

use bevy::prelude::*;
use lightyear::prelude::{LocalId, PeerId};
use lightyear::prelude::server::ClientOf;

use crate::net::protocol::{NetworkOwner, NetworkedHealth, NetworkedPlayer};
use crate::ui::hud::HudRoot;

pub struct HealthHudPlugin;

impl Plugin for HealthHudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostStartup, spawn_health_bar)
            .add_systems(Update, update_health_bar);
    }
}

#[derive(Component)]
struct HealthBar;

#[derive(Component)]
struct HealthBarFill;

#[derive(Component)]
struct HealthBarText;

/// Width and height of the bar (px). 200×14 sits comfortably above the
/// bottom-left corner without crowding the crosshair cluster at center.
const W: f32 = 200.0;
const H: f32 = 14.0;
const MARGIN: f32 = 20.0;

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
                    font_size: 12.0,
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
