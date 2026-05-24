//! Customization panel: per-slot mesh pickers + 6 color-swatch buttons.
//!
//! Toggles on the `K` key. Five slot rows (Class, Hair, Eyes,
//! Eyebrows, Mouth) each have prev/next arrows that cycle through
//! [`PartSelection`]'s variant tables. Six color swatches (body
//! channels + object channels) cycle through `PRESETS`.
//!
//! All edits land in local resources first — `PartSelection` for mesh
//! choices, `CharacterColors` for tints. The recolor + parts systems
//! pick the change up via `Changed<>` filters and update the local
//! character. Replication of these resources back through the
//! server is handled by the `send_local_*` systems in
//! `crate::net::client`.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

use crate::input::InputMode;
use crate::player::parts::{PartSelection, Slot as PartSlot, SLOTS};
use crate::player::recolor::CharacterColors;

pub struct CustomizationPlugin;

impl Plugin for CustomizationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CustomizationOpen>()
            .init_resource::<SlotPresetIndex>()
            .init_resource::<OrbitCamera>()
            .add_systems(
                Update,
                (
                    toggle_panel_on_key,
                    handle_button_interactions,
                    refresh_swatch_visuals,
                    refresh_slot_labels,
                    orbit_with_ad_keys,
                ),
            );
    }
}

/// Camera-orbit angle while the customizer is open. The controller's
/// `apply_rotation` reads this to place the 3rd-person camera
/// `Quat::from_axis_angle(Y, yaw) * CAM_LOCAL` around the player.
/// Yaw=0 puts the camera directly behind; positive rotates clockwise
/// from above.
#[derive(Resource, Default)]
pub struct OrbitCamera {
    pub yaw: f32,
}

/// Tracks whether the panel is currently spawned.
#[derive(Resource, Default)]
struct CustomizationOpen {
    open: bool,
    root: Option<Entity>,
}

/// Per-channel index into [`PRESETS`]. Stored separately so cycling
/// remains stable across `CharacterColors` mutations (the preset
/// index is the authoritative state — `CharacterColors` is derived
/// from it).
#[derive(Resource, Default)]
struct SlotPresetIndex {
    body: [usize; 3],
    objects: [usize; 3],
}

/// Eight preset colors per swatch. Order matters — cycling advances
/// through them sequentially.
const PRESETS: [LinearRgba; 8] = [
    LinearRgba::rgb(0.85, 0.74, 0.62), // skin / cream
    LinearRgba::rgb(0.85, 0.15, 0.12), // crimson
    LinearRgba::rgb(0.95, 0.55, 0.10), // orange
    LinearRgba::rgb(0.92, 0.82, 0.20), // yellow
    LinearRgba::rgb(0.20, 0.65, 0.25), // forest green
    LinearRgba::rgb(0.18, 0.40, 0.85), // royal blue
    LinearRgba::rgb(0.55, 0.22, 0.70), // purple
    LinearRgba::rgb(0.10, 0.08, 0.12), // near-black
];

#[derive(Component)]
struct CustomizationRoot;

#[derive(Component, Clone, Copy)]
enum CustomButton {
    SlotPrev(PartSlot),
    SlotNext(PartSlot),
    BodySwatch(usize),
    ObjectSwatch(usize),
}

/// Tag on a slot's text node so `refresh_slot_labels` knows which
/// slot's current variant label to rewrite when `PartSelection`
/// changes.
#[derive(Component, Copy, Clone)]
struct SlotLabel(PartSlot);

#[derive(Component)]
struct SwatchVisual {
    channel: ColorChannel,
}

#[derive(Copy, Clone)]
enum ColorChannel {
    Body(usize),
    Objects(usize),
}

fn toggle_panel_on_key(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<CustomizationOpen>,
    mut mode: ResMut<InputMode>,
    mut cursor: Single<&mut CursorOptions, With<PrimaryWindow>>,
    selection: Res<PartSelection>,
    colors: Res<CharacterColors>,
) {
    if !keys.just_pressed(KeyCode::KeyK) {
        return;
    }
    if state.open {
        if let Some(root) = state.root.take() {
            if let Ok(mut e) = commands.get_entity(root) {
                e.despawn();
            }
        }
        state.open = false;
        *mode = InputMode::Player;
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    } else {
        state.root = Some(spawn_panel(&mut commands, &selection, &colors));
        state.open = true;
        *mode = InputMode::Customizing;
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }
}

fn spawn_panel(
    commands: &mut Commands,
    selection: &PartSelection,
    colors: &CharacterColors,
) -> Entity {
    commands
        .spawn((
            CustomizationRoot,
            Node {
                position_type: PositionType::Absolute,
                right: Val::Px(20.0),
                top: Val::Px(20.0),
                width: Val::Px(300.0),
                padding: UiRect::all(Val::Px(10.0)),
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(6.0),
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.78)),
        ))
        .with_children(|root| {
            root.spawn((
                Text::new("Customize  (K close, A/D orbit)"),
                TextFont { font_size: 13.0, ..default() },
                TextColor(Color::srgba(0.9, 0.9, 1.0, 1.0)),
            ));

            for &slot in SLOTS {
                spawn_slot_row(root, slot.display(), slot, selection.label(slot));
            }

            // Spacer.
            root.spawn(Node { height: Val::Px(6.0), ..default() });

            spawn_color_row(
                root,
                "Body",
                &colors.body,
                |i| ColorChannel::Body(i),
                |i| CustomButton::BodySwatch(i),
            );
            spawn_color_row(
                root,
                "Objects",
                &colors.objects,
                |i| ColorChannel::Objects(i),
                |i| CustomButton::ObjectSwatch(i),
            );
        })
        .id()
}

fn spawn_slot_row(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    slot: PartSlot,
    current: &str,
) {
    parent
        .spawn(Node {
            flex_direction: FlexDirection::Row,
            column_gap: Val::Px(8.0),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|row| {
            // Slot label (e.g. "Class").
            row.spawn((
                Node { width: Val::Px(64.0), ..default() },
                children![(
                    Text::new(label.to_string()),
                    TextFont { font_size: 13.0, ..default() },
                    TextColor(Color::srgba(0.85, 0.85, 0.9, 1.0)),
                )],
            ));
            // Prev button.
            spawn_text_button(row, "<", CustomButton::SlotPrev(slot));
            // Variant name (auto-updates via SlotLabel).
            row.spawn((
                SlotLabel(slot),
                Node {
                    flex_grow: 1.0,
                    justify_content: JustifyContent::Center,
                    ..default()
                },
                children![(
                    Text::new(current.to_string()),
                    TextFont { font_size: 13.0, ..default() },
                    TextColor(Color::WHITE),
                )],
            ));
            // Next button.
            spawn_text_button(row, ">", CustomButton::SlotNext(slot));
        });
}

fn spawn_text_button(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    kind: CustomButton,
) {
    parent.spawn((
        Button,
        kind,
        Node {
            width: Val::Px(24.0),
            height: Val::Px(24.0),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        },
        BackgroundColor(Color::srgba(0.2, 0.2, 0.25, 1.0)),
        children![(
            Text::new(label.to_string()),
            TextFont { font_size: 14.0, ..default() },
            TextColor(Color::WHITE),
        )],
    ));
}

fn spawn_color_row(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    tints: &[LinearRgba; 3],
    channel: impl Fn(usize) -> ColorChannel,
    button_kind: impl Fn(usize) -> CustomButton,
) {
    parent
        .spawn(Node {
            flex_direction: FlexDirection::Row,
            column_gap: Val::Px(8.0),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                Node { width: Val::Px(64.0), ..default() },
                children![(
                    Text::new(label.to_string()),
                    TextFont { font_size: 13.0, ..default() },
                    TextColor(Color::srgba(0.85, 0.85, 0.9, 1.0)),
                )],
            ));
            for i in 0..3 {
                row.spawn((
                    Button,
                    button_kind(i),
                    SwatchVisual { channel: channel(i) },
                    Node {
                        width: Val::Px(28.0),
                        height: Val::Px(28.0),
                        ..default()
                    },
                    BackgroundColor(Color::from(tints[i])),
                ));
            }
        });
}

fn handle_button_interactions(
    mut interactions: Query<
        (&Interaction, &CustomButton),
        (Changed<Interaction>, With<Button>),
    >,
    mut selection: ResMut<PartSelection>,
    mut colors: ResMut<CharacterColors>,
    mut presets: ResMut<SlotPresetIndex>,
) {
    for (interaction, button) in &mut interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match *button {
            CustomButton::SlotPrev(slot) => selection.advance(slot, -1),
            CustomButton::SlotNext(slot) => selection.advance(slot, 1),
            CustomButton::BodySwatch(i) => {
                presets.body[i] = (presets.body[i] + 1) % PRESETS.len();
                colors.body[i] = PRESETS[presets.body[i]];
            }
            CustomButton::ObjectSwatch(i) => {
                presets.objects[i] = (presets.objects[i] + 1) % PRESETS.len();
                colors.objects[i] = PRESETS[presets.objects[i]];
            }
        }
    }
}

fn refresh_slot_labels(
    selection: Res<PartSelection>,
    labels: Query<(&SlotLabel, &Children)>,
    mut texts: Query<&mut Text>,
) {
    if !selection.is_changed() {
        return;
    }
    for (label, children) in &labels {
        for child in children.iter() {
            if let Ok(mut text) = texts.get_mut(child) {
                text.0 = selection.label(label.0).to_string();
            }
        }
    }
}

fn refresh_swatch_visuals(
    colors: Res<CharacterColors>,
    mut swatches: Query<(&SwatchVisual, &mut BackgroundColor)>,
) {
    if !colors.is_changed() {
        return;
    }
    for (swatch, mut bg) in &mut swatches {
        let tint = match swatch.channel {
            ColorChannel::Body(i) => colors.body[i],
            ColorChannel::Objects(i) => colors.objects[i],
        };
        bg.0 = Color::from(tint);
    }
}

/// A/D orbits the customization-mode camera around the player.
/// Reads keyboard directly because PlayerContext is inactive while
/// customizing — BEI events for Movement aren't firing.
fn orbit_with_ad_keys(
    mode: Res<InputMode>,
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    mut orbit: ResMut<OrbitCamera>,
) {
    if *mode != InputMode::Customizing {
        return;
    }
    const ORBIT_SPEED: f32 = 2.2;
    let dt = time.delta_secs();
    if keys.pressed(KeyCode::KeyA) {
        orbit.yaw -= ORBIT_SPEED * dt;
    }
    if keys.pressed(KeyCode::KeyD) {
        orbit.yaw += ORBIT_SPEED * dt;
    }
}
