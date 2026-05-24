"""Port a Polysplit / Mixamo-compatible character FBX onto the wisp animation
rig + actions and write a wisp-ready .glb.

Approach: the asset-pack characters and the existing `wizard.glb` share a
skeleton (Polysplit's `rootSkeleton`). The FBX has meshes-only (no
actions); the source glb has actions but a different mesh. We graft the
new meshes onto the source's armature so the existing animations apply
unchanged.

Usage (run from anywhere — Blender provides the bpy environment):

    blender -b -P tools/character/port_character.py -- \\
        --fbx /path/to/M_Sorcerer.fbx \\
        --anim-source assets/wizard.glb \\
        --out      assets/sorcerer.glb \\
        --keep 'M_Head' --keep 'M_eyes0' --keep 'M_eyebrows0' \\
        --keep 'M_mouth0' --keep 'M_BottomBody' --keep 'M_TopBody' \\
        --keep 'M_Sorcerer_*'

`--keep` is a glob pattern (fnmatch) and may be repeated. A mesh from the
FBX is kept iff at least one `--keep` matches its name. If `--keep` is
omitted, defaults to ['M_Head', 'M_BottomBody', 'M_TopBody', 'M_eyes0',
'M_eyebrows0', 'M_mouth0', '<FbxBaseName>_*'] — works for any
'M_<Class>' / 'F_<Class>' character in the BasicHeroes pack.

The output glb keeps **every** action from the anim source (the user
chose "keep everything" — leaves room for future spell-specific clips).
"""

import argparse
import fnmatch
import os
import sys

import bpy


# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

def parse_args() -> argparse.Namespace:
    # Blender's own argv ends at `--`; ours starts after it.
    argv = sys.argv
    argv = argv[argv.index("--") + 1 :] if "--" in argv else []

    p = argparse.ArgumentParser(description="Port a Polysplit character to a wisp-ready glb.")
    p.add_argument("--fbx", required=True, help="Source FBX (Polysplit character).")
    p.add_argument("--anim-source", required=True, help="glb containing the target armature + actions.")
    p.add_argument("--out", required=True, help="Output glb path.")
    p.add_argument(
        "--keep",
        action="append",
        default=None,
        metavar="GLOB",
        help="Mesh-name glob to keep from the FBX. May be repeated.",
    )
    return p.parse_args(argv)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def reset_scene() -> None:
    """Start from a totally empty Blender scene."""
    bpy.ops.wm.read_factory_settings(use_empty=True)


def find_unique_armature(prior_names: set[str]) -> bpy.types.Object:
    """Return the single new armature added to the scene since `prior_names`."""
    new = [o for o in bpy.data.objects if o.type == "ARMATURE" and o.name not in prior_names]
    if len(new) != 1:
        raise RuntimeError(
            f"expected exactly one new armature, found {len(new)}: {[o.name for o in new]}"
        )
    return new[0]


def mesh_children(armature: bpy.types.Object) -> list[bpy.types.Object]:
    return [o for o in bpy.data.objects if o.type == "MESH" and o.parent is armature]


def default_keep_globs(fbx_path: str) -> list[str]:
    """Sensible defaults if user didn't supply --keep.

    Robed-mage archetypes (sorcerer, mage, witch, warlock) have outfits
    that fully cover the body and carry all the skinning weights (incl.
    fingers). Keeping the base body meshes (`M_BottomBody`/`M_TopBody`)
    alongside them double-renders every limb in the same world space —
    visible as z-fighting on the torso and "duplicated bones" on the
    fingers. Default to outfit-only; for armored archetypes where the
    body is the visible skin, add `--keep 'M_TopBody' --keep 'M_BottomBody'`
    explicitly.
    """
    base = os.path.splitext(os.path.basename(fbx_path))[0]  # e.g. 'M_Sorcerer'
    return [
        "M_Head",
        "F_Head",
        "M_eyes0",
        "F_eyes0",
        "M_eyebrows0",
        "F_eyebrows0",
        "M_mouth0",
        "F_mouth0",
        f"{base}_*",
    ]


# ---------------------------------------------------------------------------
# Main pipeline
# ---------------------------------------------------------------------------

def port(args: argparse.Namespace) -> None:
    reset_scene()

    # Import the anim source first so its armature gets the unsuffixed
    # name. We're going to keep this armature.
    pre = {o.name for o in bpy.data.objects}
    bpy.ops.import_scene.gltf(filepath=args.anim_source)
    anim_arm = find_unique_armature(pre)
    print(f"anim-source armature: {anim_arm.name!r} ({len(anim_arm.data.bones)} bones)")

    # Drop the anim source's own meshes — we're replacing them.
    for m in mesh_children(anim_arm):
        bpy.data.objects.remove(m, do_unlink=True)

    # Import the FBX. Blender suffixes its armature with `.001` because it
    # collides with `rootSkeleton`. We capture it explicitly.
    #
    # FBX-importer flags here come directly from the asset creator's
    # documentation (BlenderArmature_ImportFBX.png in the pack): they
    # ship the FBX out of Maya knowing Blender's default importer
    # mis-orients the bones, and the fix on the import side is the
    # combination of **Force Connect Children** + **Automatic Bone
    # Orientation**. Without either, Blender uses the FBX-stored "node
    # axis" for each bone, which leaves finger bones oriented along a
    # global axis instead of along their child direction. Animation
    # curves stored relative to the bone's local frame then rotate
    # around the wrong axis at runtime — exactly the "fingers fanning
    # sideways instead of curling" pathology.
    pre = {o.name for o in bpy.data.objects}
    bpy.ops.import_scene.fbx(
        filepath=args.fbx,
        use_anim=False,
        force_connect_children=True,
        automatic_bone_orientation=True,
    )
    fbx_arm = find_unique_armature(pre)
    print(f"fbx armature: {fbx_arm.name!r} ({len(fbx_arm.data.bones)} bones)")

    # Drop any empty scene-root nodes the FBX importer left behind
    # (the asset pack ships `BaseMale` / `M_<Class>` group nodes that
    # have no mesh/skin/children — they're container empties from Unity
    # that just clutter the exported glTF).
    for obj in list(bpy.data.objects):
        if obj.name in pre:
            continue
        if obj.type == "EMPTY" and not obj.children:
            bpy.data.objects.remove(obj, do_unlink=True)

    fbx_meshes = mesh_children(fbx_arm)
    print(f"fbx meshes: {len(fbx_meshes)}")

    # Filter to the keep set.
    keep_globs = args.keep or default_keep_globs(args.fbx)
    print(f"keep globs: {keep_globs}")
    kept, dropped = [], []
    for m in fbx_meshes:
        if any(fnmatch.fnmatchcase(m.name, g) for g in keep_globs):
            kept.append(m)
        else:
            dropped.append(m)
    print(f"keeping {len(kept)} meshes, dropping {len(dropped)}")
    for m in kept:
        print(f"  KEEP  {m.name}  verts={len(m.data.vertices)}")
    for m in dropped:
        bpy.data.objects.remove(m, do_unlink=True)

    if not kept:
        raise RuntimeError("no meshes survived the keep filter — refusing to write an empty character")

    # Bake the FBX armature's world transform (Unity FBX exports at scale
    # 0.01 with an axis-swap rotation) into the bones AND the meshes so
    # vertex positions end up in meters/axis-corrected before we move
    # them to the anim-source rig. Doing this on the armature with all
    # children selected (rather than per-mesh) handles a quirk in the
    # asset pack: meshes like `M_BottomBody` / `M_TopBody` have the FBX
    # parent-transform baked into their `matrix_local` (identity world)
    # while others inherit it via `matrix_world` from the armature. A
    # per-mesh unparent+apply path catches only the second group. The
    # armature-level apply normalizes both.
    bpy.context.view_layer.update()
    bpy.ops.object.select_all(action="DESELECT")
    fbx_arm.select_set(True)
    for child in mesh_children(fbx_arm):
        # Single-user any shared mesh data; the apply otherwise no-ops on
        # multi-user data. (The asset pack reuses M_BottomBody / M_TopBody
        # across every character variant.)
        if child.data.users > 1:
            child.data = child.data.copy()
        child.select_set(True)
    bpy.context.view_layer.objects.active = fbx_arm
    bpy.ops.object.transform_apply(location=True, rotation=True, scale=True)
    # The FBX armature is now at identity world with bones+meshes in
    # meters/axis-corrected coordinates. We KEEP this armature — its
    # bone rolls match the FBX mesh's bind data, and the asset
    # creator's import flags ensure orientations match what their
    # source export expected.

    # The anim-source armature came in with a 0.01 scale + axis-swap
    # baked into its matrix_world (Bevy reads this correctly because
    # the glTF skin transforms compose with the armature node's
    # transform). For retargeting, both armatures need to live in the
    # SAME world frame — otherwise Copy Transforms (world space)
    # captures the scale/rotation difference as per-bone pose offsets,
    # which is exactly the "more broken" regression. Apply
    # anim_arm's world transform so both armatures sit at identity.
    bpy.ops.object.select_all(action="DESELECT")
    anim_arm.select_set(True)
    bpy.context.view_layer.objects.active = anim_arm
    bpy.ops.object.transform_apply(location=True, rotation=True, scale=True)

    # ---- Retarget the anim-source actions onto the FBX armature ----
    #
    # We cannot reuse anim_arm directly: Blender's gltf importer picks
    # different bone rolls (X-axis along -Z) than its FBX importer
    # (X-axis along +Y), so reusing anim_arm's bones with the FBX's
    # mesh skinning data rotates fingers ~90° (the "fanning sideways"
    # bug). Instead we keep FBX bones and bake-retarget each
    # anim-source action via Copy Transforms constraints — the
    # constraint copies world-space pose and `nla.bake` records it as
    # fcurves in the FBX armature's bone-local frame, automatically
    # absorbing the roll difference.
    #
    # Bone naming: Polysplit's FBX and the anim-source glb use
    # identical bone names (rootSkeleton + named L_/R_ joints), so the
    # constraint target lookup is a direct name match.
    fbx_bone_names = {b.name for b in fbx_arm.data.bones}
    anim_bone_names = {b.name for b in anim_arm.data.bones}
    shared = fbx_bone_names & anim_bone_names
    print(f"\nretarget: {len(shared)}/{len(anim_bone_names)} bones share names with FBX rig")
    missing = sorted(anim_bone_names - fbx_bone_names)
    if missing:
        print(f"  bones in anim_arm not in fbx_arm (will be unanimated): {missing[:10]}")

    # Add Copy Transforms constraints. World-space target+owner means
    # the constraint reads the source bone's posed world matrix and
    # makes the FBX bone match — regardless of either bone's roll.
    for fbx_bone in fbx_arm.pose.bones:
        if fbx_bone.name not in shared:
            continue
        # Clear any existing constraint we may have added on a previous
        # pass (defensive — re-running the script shouldn't accumulate).
        for c in list(fbx_bone.constraints):
            if c.name == "Retarget":
                fbx_bone.constraints.remove(c)
        c = fbx_bone.constraints.new(type="COPY_TRANSFORMS")
        c.name = "Retarget"
        c.target = anim_arm
        c.subtarget = fbx_bone.name

    # Bake each anim-source action onto the FBX armature.
    if anim_arm.animation_data is None:
        anim_arm.animation_data_create()
    if fbx_arm.animation_data is None:
        fbx_arm.animation_data_create()
    saved_actions = list(bpy.data.actions)
    print(f"\nretarget: baking {len(saved_actions)} actions...")
    baked_pairs = []  # (src_action, new_name)
    for src in saved_actions:
        # Rename the source first so the baked action can take the
        # canonical name (Blender appends a suffix on collision).
        canonical = src.name
        src.name = f"_src_{canonical}"

        anim_arm.animation_data.action = src
        f_start = int(src.frame_range[0])
        f_end = int(src.frame_range[1])
        bpy.context.scene.frame_start = f_start
        bpy.context.scene.frame_end = f_end
        bpy.context.scene.frame_set(f_start)

        # Activate fbx_arm + select all of its pose bones (bake reads
        # the persistent selection state, not the call-site mode).
        bpy.ops.object.select_all(action="DESELECT")
        fbx_arm.select_set(True)
        bpy.context.view_layer.objects.active = fbx_arm
        bpy.ops.object.mode_set(mode="POSE")
        bpy.ops.pose.select_all(action="SELECT")

        # Bake the constrained pose into a fresh action.
        bpy.ops.nla.bake(
            frame_start=f_start,
            frame_end=f_end,
            step=1,
            only_selected=True,
            visual_keying=True,
            clear_constraints=False,
            clear_parents=False,
            use_current_action=False,
            bake_types={"POSE"},
        )
        bpy.ops.object.mode_set(mode="OBJECT")

        baked = fbx_arm.animation_data.action
        if baked is None:
            print(f"  WARN  bake for {canonical!r} produced no action")
            continue
        baked.name = canonical
        baked.use_fake_user = True
        baked_pairs.append((src, canonical))
        print(f"  baked {canonical!r}  frames={f_start}-{f_end}  fcurves={len(baked.fcurves)}")

    # Strip the now-stale source actions so the exporter only sees the
    # FBX-retargeted ones.
    for src, _ in baked_pairs:
        bpy.data.actions.remove(src, do_unlink=True)

    # Remove the retarget constraints — we don't want them serialized.
    for fbx_bone in fbx_arm.pose.bones:
        for c in list(fbx_bone.constraints):
            if c.name == "Retarget":
                fbx_bone.constraints.remove(c)

    # Drop anim_arm now that its actions are baked onto fbx_arm.
    bpy.data.objects.remove(anim_arm, do_unlink=True)

    # Detach any leftover animation_data action on fbx_arm — leave it
    # NLA-orphaned so the exporter walks bpy.data.actions cleanly.
    if fbx_arm.animation_data is not None:
        fbx_arm.animation_data.action = None

    # Report the state we're about to export.
    print(f"\nexport state:")
    print(f"  armature: {fbx_arm.name!r}, bones={len(fbx_arm.data.bones)}")
    print(f"  meshes: {[m.name for m in mesh_children(fbx_arm)]}")
    print(f"  actions: {[a.name for a in bpy.data.actions]}")

    # Export. `export_animation_mode='ACTIONS'` exports every Action in
    # bpy.data.actions as a separate glTF animation with the action's
    # name — which is what wisp reads (gltf.named_animations).
    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    bpy.ops.export_scene.gltf(
        filepath=args.out,
        export_format="GLB",
        export_animations=True,
        export_animation_mode="ACTIONS",
        export_skins=True,
        export_apply=False,  # don't modify-apply armature/scale
    )
    print(f"\nwrote {args.out}")


if __name__ == "__main__":
    args = parse_args()
    port(args)
