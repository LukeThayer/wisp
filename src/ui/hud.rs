//! Heads-up display: crosshair, alignment indicator, and lumen counter.

use bevy::prelude::*;

use crate::magic::{IrisState, LensPower, IRIS_MAX_CHARGE, MAX_LANTERNS};
use crate::player::spells::{ActiveSpell, SpellId};

#[derive(Component)]
pub struct HudRoot;

#[derive(Component)]
pub struct LumenText;

/// Marker on the parent node of the alignment-box arms; hidden when no
/// lantern is in tether range.
#[derive(Component)]
pub struct AlignmentIndicator;

/// One of MAX_LANTERNS preallocated dots. The HUD pre-spawns the full set;
/// each frame the update system pairs the first N dots with the N in-range
/// sources reported by `LensPower` and hides the rest.
#[derive(Component)]
pub struct AlignmentDot {
    pub slot: usize,
}

/// Container around the iris charge bar (track + fill). Hidden when the
/// active spell isn't the Iris.
#[derive(Component)]
pub struct IrisChargeBar;

/// The fill node inside the iris charge bar — width is set proportional to
/// `IrisState::charge / IRIS_MAX_CHARGE` each frame.
#[derive(Component)]
pub struct IrisChargeFill;

const CROSSHAIR_THICKNESS: f32 = 2.0;
const CROSSHAIR_LENGTH: f32 = 12.0;
const CROSSHAIR_GAP: f32 = 4.0;

/// Half-extents (in pixels) of the alignment box. Asymmetric — taller than
/// wide — because the lens tolerates more vertical pitch error than yaw.
const ALIGN_BOX_HALF_W: f32 = 36.0;
const ALIGN_BOX_HALF_H: f32 = 84.0;
const ALIGN_BORDER: f32 = 1.5;
const ALIGN_DOT_SIZE: f32 = 6.0;

const LUMEN_TEXT_TOP: f32 = 96.0;

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
                .spawn(Node {
                    width: Val::Px(0.0),
                    height: Val::Px(0.0),
                    ..default()
                })
                .with_children(|center| {
                    spawn_crosshair(center);
                    spawn_alignment_indicator(center);
                    spawn_lumen_text(center);
                    spawn_iris_charge_bar(center);
                });
        });
}

fn spawn_crosshair(center: &mut ChildSpawnerCommands) {
    let arm = Color::srgba(1.0, 1.0, 1.0, 0.85);

    // Left
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
    // Right
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
    // Top
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
    // Bottom
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

fn spawn_alignment_indicator(center: &mut ChildSpawnerCommands) {
    let border = Color::srgba(0.85, 0.85, 0.85, 0.55);

    center
        .spawn((
            AlignmentIndicator,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(-ALIGN_BOX_HALF_W),
                top: Val::Px(-ALIGN_BOX_HALF_H),
                width: Val::Px(ALIGN_BOX_HALF_W * 2.0),
                height: Val::Px(ALIGN_BOX_HALF_H * 2.0),
                display: Display::None,
                ..default()
            },
        ))
        .with_children(|b| {
            // Top edge
            b.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    top: Val::Px(0.0),
                    width: Val::Px(ALIGN_BOX_HALF_W * 2.0),
                    height: Val::Px(ALIGN_BORDER),
                    ..default()
                },
                BackgroundColor(border),
            ));
            // Bottom edge
            b.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    top: Val::Px(ALIGN_BOX_HALF_H * 2.0 - ALIGN_BORDER),
                    width: Val::Px(ALIGN_BOX_HALF_W * 2.0),
                    height: Val::Px(ALIGN_BORDER),
                    ..default()
                },
                BackgroundColor(border),
            ));
            // Left edge
            b.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    top: Val::Px(0.0),
                    width: Val::Px(ALIGN_BORDER),
                    height: Val::Px(ALIGN_BOX_HALF_H * 2.0),
                    ..default()
                },
                BackgroundColor(border),
            ));
            // Right edge
            b.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(ALIGN_BOX_HALF_W * 2.0 - ALIGN_BORDER),
                    top: Val::Px(0.0),
                    width: Val::Px(ALIGN_BORDER),
                    height: Val::Px(ALIGN_BOX_HALF_H * 2.0),
                    ..default()
                },
                BackgroundColor(border),
            ));
        });

    // One dot per possible source. Hidden by default; the update system
    // pairs them with `LensPower::sources` each frame.
    for slot in 0..MAX_LANTERNS {
        center.spawn((
            AlignmentDot { slot },
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(-ALIGN_DOT_SIZE * 0.5),
                top: Val::Px(-ALIGN_DOT_SIZE * 0.5),
                width: Val::Px(ALIGN_DOT_SIZE),
                height: Val::Px(ALIGN_DOT_SIZE),
                display: Display::None,
                ..default()
            },
            BackgroundColor(Color::srgba(1.0, 0.85, 0.4, 0.9)),
        ));
    }
}

fn spawn_iris_charge_bar(center: &mut ChildSpawnerCommands) {
    const W: f32 = 120.0;
    const H: f32 = 6.0;
    center.spawn((
        IrisChargeBar,
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(-W * 0.5),
            top: Val::Px(124.0),
            width: Val::Px(W),
            height: Val::Px(H),
            display: Display::None,
            ..default()
        },
        BackgroundColor(Color::srgba(0.15, 0.15, 0.18, 0.7)),
        children![(
            IrisChargeFill,
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
}

fn spawn_lumen_text(center: &mut ChildSpawnerCommands) {
    center.spawn((
        LumenText,
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(-60.0),
            top: Val::Px(LUMEN_TEXT_TOP),
            width: Val::Px(120.0),
            justify_content: JustifyContent::Center,
            ..default()
        },
        children![(
            Text::new("0 lm"),
            TextFont {
                font_size: 16.0,
                ..default()
            },
            TextColor(Color::srgba(0.7, 0.7, 0.7, 0.85)),
        )],
    ));
}

pub fn update_lumen_text(
    power: Res<LensPower>,
    parents: Query<&Children, With<LumenText>>,
    mut texts: Query<(&mut Text, &mut TextColor)>,
) {
    for children in &parents {
        for child in children.iter() {
            if let Ok((mut text, mut color)) = texts.get_mut(child) {
                match (power.in_range, power.aligned) {
                    (false, _) => {
                        *text = Text::new("— lm");
                        color.0 = Color::srgba(0.5, 0.5, 0.5, 0.6);
                    }
                    (true, false) => {
                        *text = Text::new("misaligned");
                        color.0 = Color::srgba(1.0, 0.35, 0.35, 0.9);
                    }
                    (true, true) => {
                        let lm = power.lumens.round() as i32;
                        *text = Text::new(format!("{lm} lm"));
                        let warmth = power.scalar;
                        color.0 = Color::srgba(
                            1.0,
                            0.85 + 0.15 * warmth,
                            0.55 + 0.45 * (1.0 - warmth),
                            0.85,
                        );
                    }
                }
            }
        }
    }
}

pub fn update_iris_charge_bar(
    iris: Res<IrisState>,
    active_spell: Res<ActiveSpell>,
    mut bars: Query<&mut Node, (With<IrisChargeBar>, Without<IrisChargeFill>)>,
    mut fills: Query<(&mut Node, &mut BackgroundColor), With<IrisChargeFill>>,
) {
    const W: f32 = 120.0;
    let show = active_spell.0 == SpellId::Iris;
    for mut node in &mut bars {
        node.display = if show { Display::Flex } else { Display::None };
    }

    // During the burst flash, show what was captured at release so the player
    // sees the bar drain after firing rather than instantly snapping to empty.
    let displayed = if iris.burst_timer > 0.0 {
        iris.burst_charge
    } else {
        iris.charge
    };
    let frac = (displayed / IRIS_MAX_CHARGE).clamp(0.0, 1.0);
    let bursting = iris.burst_timer > 0.0;
    for (mut node, mut color) in &mut fills {
        node.width = Val::Px(W * frac);
        // Brighter as the bar fills; pure-bright during the burst.
        let warmth = if bursting { 1.0 } else { frac };
        color.0 = Color::srgba(
            1.0,
            0.75 + 0.25 * warmth,
            0.4 + 0.5 * warmth,
            0.95,
        );
    }
}

pub fn update_alignment_indicator(
    power: Res<LensPower>,
    mut indicator: Query<&mut Node, (With<AlignmentIndicator>, Without<AlignmentDot>)>,
    mut dots: Query<(&AlignmentDot, &mut Node, &mut BackgroundColor)>,
) {
    let show_box = if power.in_range { Display::Flex } else { Display::None };
    for mut node in &mut indicator {
        node.display = show_box;
    }

    for (dot, mut node, mut color) in &mut dots {
        let Some(source) = power.sources.get(dot.slot) else {
            node.display = Display::None;
            continue;
        };

        // Allow the dot to drift up to 1.6× the tolerance so the player can
        // see which way to swing the lens when misaligned.
        let yaw = source.yaw_offset.clamp(-1.6, 1.6);
        let pitch = source.pitch_offset.clamp(-1.6, 1.6);
        let dx = yaw * ALIGN_BOX_HALF_W;
        // Screen Y grows downward; pitch_offset is +up, so negate.
        let dy = -pitch * ALIGN_BOX_HALF_H;

        node.display = Display::Flex;
        node.left = Val::Px(-ALIGN_DOT_SIZE * 0.5 + dx);
        node.top = Val::Px(-ALIGN_DOT_SIZE * 0.5 + dy);

        let is_active = power.active_lantern == Some(source.entity);
        color.0 = match (source.aligned, is_active) {
            // Aligned + active: brightest yellow.
            (true, true) => Color::srgba(1.0, 0.9, 0.4, 0.95),
            // Aligned but not the chosen active: dimmer yellow.
            (true, false) => Color::srgba(0.95, 0.85, 0.45, 0.55),
            // Misaligned: red regardless of active status.
            (false, _) => Color::srgba(1.0, 0.35, 0.35, 0.9),
        };
    }
}
