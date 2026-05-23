//! HUD scaffolding: just the crosshair and a `HudCenter` anchor node that
//! per-spell HUD overlays attach to. Spell-specific UI (lumen text, alignment
//! indicator, iris charge bar) lives in each spell's module so the HUD layer
//! itself stays spell-agnostic.

use bevy::prelude::*;

#[derive(Component)]
pub struct HudRoot;

/// Zero-size node centered on the viewport. Per-spell HUD overlays add their
/// nodes as children of this anchor, positioning themselves with absolute
/// offsets from the screen center.
#[derive(Component)]
pub struct HudCenter;

const CROSSHAIR_THICKNESS: f32 = 2.0;
const CROSSHAIR_LENGTH: f32 = 12.0;
const CROSSHAIR_GAP: f32 = 4.0;

pub fn spawn_hud(mut commands: Commands) {
    commands
        .spawn((
            Name::new("Hud"),
            HudRoot,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                top: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            Pickable::IGNORE,
        ))
        .with_children(|parent| {
            parent
                .spawn((
                    HudCenter,
                    Node {
                        width: Val::Px(0.0),
                        height: Val::Px(0.0),
                        ..default()
                    },
                ))
                .with_children(spawn_crosshair);
        });
}

fn spawn_crosshair(center: &mut ChildSpawnerCommands) {
    let arm = Color::srgba(1.0, 1.0, 1.0, 0.85);

    center.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(-(CROSSHAIR_GAP + CROSSHAIR_LENGTH)),
            top: Val::Px(-CROSSHAIR_THICKNESS * 0.5),
            width: Val::Px(CROSSHAIR_LENGTH),
            height: Val::Px(CROSSHAIR_THICKNESS),
            ..default()
        },
        BackgroundColor(arm),
    ));
    center.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(CROSSHAIR_GAP),
            top: Val::Px(-CROSSHAIR_THICKNESS * 0.5),
            width: Val::Px(CROSSHAIR_LENGTH),
            height: Val::Px(CROSSHAIR_THICKNESS),
            ..default()
        },
        BackgroundColor(arm),
    ));
    center.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(-CROSSHAIR_THICKNESS * 0.5),
            top: Val::Px(-(CROSSHAIR_GAP + CROSSHAIR_LENGTH)),
            width: Val::Px(CROSSHAIR_THICKNESS),
            height: Val::Px(CROSSHAIR_LENGTH),
            ..default()
        },
        BackgroundColor(arm),
    ));
    center.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(-CROSSHAIR_THICKNESS * 0.5),
            top: Val::Px(CROSSHAIR_GAP),
            width: Val::Px(CROSSHAIR_THICKNESS),
            height: Val::Px(CROSSHAIR_LENGTH),
            ..default()
        },
        BackgroundColor(arm),
    ));
}
