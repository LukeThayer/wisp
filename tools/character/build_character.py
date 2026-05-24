"""Build a wisp-ready character glb by mesh mix-and-match.

Pre-requisite: you've already opened each FBX in Blender, dialed in
**Force Connect Children** + **Automatic Bone Orientation** (per the
asset creator's `BlenderArmature_ImportFBX.png`), and exported the
result to a `.glb` under `assets/sources/`. This script then assembles
one or more of those sources into the canonical wisp output:

    blender -b -P tools/character/build_character.py -- \\
        --anim-source assets/wizard.glb \\
        --source assets/sources/witch.glb \\
            --keep 'F_Witch_Top' --keep 'F_Witch_Bottom' --keep 'F_Witch_Cape' \\
            --keep 'F_Witch_Choker' --keep 'F_Witch_Headwear' --keep 'F_Witch_Staff' \\
            --keep 'F_Head' --keep 'F_eyes0' --keep 'F_eyebrows0' --keep 'F_mouth0' \\
        --out assets/witch.glb

To mix and match between sources, repeat `--source <glb>` followed by
the `--keep` globs that apply to *that* source. Each `--source` opens
its own --keep scope:

    blender -b -P tools/character/build_character.py -- \\
        --anim-source assets/wizard.glb \\
        --source assets/sources/mage.glb --keep 'M_Mage_Bottom' --keep 'M_Mage_Top' \\
        --source assets/sources/sorcerer.glb --keep 'M_Sorcerer_Headwear' \\
        --out assets/custom_mage.glb

Keep-glob conventions for Polysplit asset-pack characters:
  - Body skin meshes (`<G>_BottomBody`, `<G>_TopBody`) — OMIT. They
    overlap with the class outfit (z-fight on torso, double fingers).
  - Class outfit (`<Class>_Top`, `<Class>_Bottom`, accessories) —
    KEEP all. Top is sleeves/tunic, Bottom is legs/skirt; despite
    their bounding-box ranges hinting at overlap, they're designed
    to abut.
  - Face variants — keep only `*_0` (eyes0/eyebrows0/mouth0). Numbered
    variants 1–4 have UNTRANSFORMED bbox in the source (z in tens of
    thousands); they're broken in the FBX export.
  - Hair — skip unless you specifically want a hairstyle. Most
    `F_hair_*`/`M_hair_*` variants share the same broken-bbox problem.

Why graft and not retarget: both the source and the anim-source went
through Blender's identical gltf import/export cycle, so their bone
conventions are symmetric. (In contrast, porting straight from FBX —
the old `port_character.py` path — uses Blender's FBX importer for the
source and gltf importer for the anim source. These two pick different
bone rolls for the same bone, so the FBX mesh's skinning weights
rotate fingers around the wrong axis on wizard.glb's rig — the
"fingers fanning sideways" pathology that motivated this rewrite.)

The output is post-processed (see `force_opaque_materials_in_glb`) to
force every material's alphaMode to OPAQUE. The asset pack's Blender
materials default to alphaMode=BLEND because the RGB-recolor shader
leaves the Principled BSDF Alpha input linked even when nothing is
transparent. BLEND mode disables early-z and renders back-faces,
visible as "arms through dress" on the witch before this fix.
"""

import argparse
import fnmatch
import os
import sys

import bpy


def parse_args() -> argparse.Namespace:
    """Two-level CLI: top-level (--anim-source, --out) plus repeated
    --source/--keep groups. argparse can't natively express "each --keep
    binds to the most-recent --source," so we parse argv by hand.
    """
    argv = sys.argv
    argv = argv[argv.index("--") + 1 :] if "--" in argv else []
    anim_source = None
    out = None
    sources: list[tuple[str, list[str]]] = []
    current_source = None
    current_keep: list[str] = []

    def flush():
        if current_source is not None:
            sources.append((current_source, list(current_keep)))

    i = 0
    while i < len(argv):
        tok = argv[i]
        if tok == "--anim-source":
            anim_source = argv[i + 1]
            i += 2
        elif tok == "--out":
            out = argv[i + 1]
            i += 2
        elif tok == "--source":
            flush()
            current_source = argv[i + 1]
            current_keep = []
            i += 2
        elif tok == "--keep":
            if current_source is None:
                raise SystemExit(f"--keep {argv[i+1]!r} before any --source")
            current_keep.append(argv[i + 1])
            i += 2
        else:
            raise SystemExit(f"unknown arg {tok!r}")
    flush()

    if anim_source is None or out is None or not sources:
        raise SystemExit(
            "usage: --anim-source GLB --source GLB [--keep GLOB]... [--source GLB --keep GLOB...]... --out GLB"
        )
    return argparse.Namespace(anim_source=anim_source, out=out, sources=sources)


def reset_scene() -> None:
    bpy.ops.wm.read_factory_settings(use_empty=True)


def find_armature(prior_names: set[str]) -> bpy.types.Object:
    new = [o for o in bpy.data.objects if o.type == "ARMATURE" and o.name not in prior_names]
    if len(new) != 1:
        raise RuntimeError(f"expected 1 new armature, found {len(new)}: {[o.name for o in new]}")
    return new[0]


def mesh_children(arm: bpy.types.Object) -> list[bpy.types.Object]:
    return [o for o in bpy.data.objects if o.type == "MESH" and o.parent is arm]


def graft_source(src_glb: str, keep_globs: list[str], anim_arm: bpy.types.Object) -> int:
    """Import `src_glb`, filter its meshes by `keep_globs`, reparent the
    kept ones onto `anim_arm`, and drop the rest (plus the source
    armature). Returns the number of meshes grafted.
    """
    pre = {o.name for o in bpy.data.objects}
    bpy.ops.import_scene.gltf(filepath=src_glb)
    src_arm = find_armature(pre)
    print(f"\nsource {src_glb!r}: armature {src_arm.name!r} ({len(src_arm.data.bones)} bones)")

    meshes = mesh_children(src_arm)
    kept = []
    for m in meshes:
        if any(fnmatch.fnmatchcase(m.name, g) for g in keep_globs):
            kept.append(m)
            print(f"  KEEP  {m.name}  verts={len(m.data.vertices)}")
        else:
            bpy.data.objects.remove(m, do_unlink=True)
    if not kept:
        raise RuntimeError(
            f"source {src_glb!r}: no meshes matched globs {keep_globs!r}; "
            f"available meshes: {[m.name for m in meshes]}"
        )

    # The source armature carries the same world transform as
    # anim_arm (both went through Blender's gltf import/export and
    # share Polysplit's 0.01 scale + axis-swap), so its bones sit at
    # the same world positions. Sanity-check by comparing one shared
    # bone's world head.
    anim_names = {b.name for b in anim_arm.data.bones}
    src_names = {b.name for b in src_arm.data.bones}
    shared = anim_names & src_names
    if not shared:
        raise RuntimeError(
            f"source {src_glb!r}: bone-name set disjoint from anim-source "
            f"({list(src_names)[:5]} vs {list(anim_names)[:5]}). Are they really the same rig?"
        )
    probe_bone = next(iter(shared))
    anim_world = (anim_arm.matrix_world @ anim_arm.data.bones[probe_bone].head_local)
    src_world = (src_arm.matrix_world @ src_arm.data.bones[probe_bone].head_local)
    delta = (anim_world - src_world).length
    print(f"  bone-position probe {probe_bone!r}: anim={tuple(round(v,3) for v in anim_world)} "
          f"src={tuple(round(v,3) for v in src_world)} delta={delta:.4f}m")
    if delta > 0.05:
        print(f"  WARN  rest-pose bone positions diverge by {delta:.4f}m — "
              "graft will visibly drift; consider re-exporting source from Blender "
              "with the same axis settings as wizard.glb (apply 0.01 scale, leave rotation).")

    # Rewire each kept mesh: ARMATURE modifier → anim_arm, parent →
    # anim_arm. Vertex-group bones must exist on anim_arm by name
    # (mesh skinning is keyed by name, not index).
    misses = 0
    for m in kept:
        arm_mod = next((mod for mod in m.modifiers if mod.type == "ARMATURE"), None)
        if arm_mod is None:
            arm_mod = m.modifiers.new(name="Armature", type="ARMATURE")
        arm_mod.object = anim_arm
        # Preserve the mesh's world transform when re-parenting (the
        # source armature's world is identical to anim_arm's, but be
        # defensive: matrix_parent_inverse should reproduce the mesh's
        # current world matrix under the new parent).
        m_world = m.matrix_world.copy()
        m.parent = anim_arm
        m.matrix_parent_inverse = anim_arm.matrix_world.inverted() @ m_world @ m.matrix_local.inverted()

        for vg in m.vertex_groups:
            if vg.name not in anim_names:
                misses += 1
                if misses <= 5:
                    print(f"    WARN  {m.name!r}: vertex_group {vg.name!r} has no match in anim armature")
    if misses > 5:
        print(f"    ...and {misses - 5} more missing vertex groups")

    # Drop the source armature now that the meshes are re-parented.
    bpy.data.objects.remove(src_arm, do_unlink=True)
    return len(kept)


def main() -> None:
    args = parse_args()
    reset_scene()

    # Import the anim source first — its armature is the canonical
    # rig that the kept meshes from every --source will be grafted
    # onto.
    pre = {o.name for o in bpy.data.objects}
    bpy.ops.import_scene.gltf(filepath=args.anim_source)
    anim_arm = find_armature(pre)
    print(f"anim-source {args.anim_source!r}: armature {anim_arm.name!r} ({len(anim_arm.data.bones)} bones)")

    # Drop the anim source's own meshes (we're going to replace them).
    for m in mesh_children(anim_arm):
        bpy.data.objects.remove(m, do_unlink=True)

    total = 0
    for src_glb, keep_globs in args.sources:
        total += graft_source(src_glb, keep_globs, anim_arm)

    # Mark every action as fake-user so the exporter walks them all.
    for act in bpy.data.actions:
        act.use_fake_user = True

    print(f"\nexport state:")
    print(f"  armature: {anim_arm.name!r} ({len(anim_arm.data.bones)} bones)")
    print(f"  meshes:   {[m.name for m in mesh_children(anim_arm)]} (total {total})")
    print(f"  actions:  {[a.name for a in bpy.data.actions]}")

    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    bpy.ops.export_scene.gltf(
        filepath=args.out,
        export_format="GLB",
        export_animations=True,
        export_animation_mode="ACTIONS",
        export_skins=True,
        export_apply=False,
    )
    print(f"\nwrote {args.out}")
    force_opaque_materials_in_glb(args.out)


def force_opaque_materials_in_glb(glb_path: str) -> None:
    """Rewrite the glb's JSON chunk so every material has
    alphaMode='OPAQUE' and no alphaCutoff. Done as a post-process
    because Blender 4.4 derives alphaMode from
    `surface_render_method` + Principled BSDF Alpha-input wiring, and
    the asset pack's RGB-recolor shader leaves the Alpha input linked
    to a texture even when the character is supposed to be opaque.
    That produces alphaMode=BLEND on export, which makes the renderer
    alpha-blend the mesh, skip early-z, and render back-faces — visible
    as "see arms through dress" on the witch.
    """
    import json
    import struct
    with open(glb_path, "rb") as f:
        data = f.read()

    if data[:4] != b"glTF":
        raise RuntimeError(f"{glb_path}: not a glb (no 'glTF' magic)")
    version, total_len = struct.unpack_from("<II", data, 4)
    assert version == 2 and total_len == len(data)

    # JSON chunk: starts at byte 12. Layout: u32 length, u32 type='JSON', bytes.
    json_len, json_type = struct.unpack_from("<II", data, 12)
    assert json_type == 0x4E4F534A, f"unexpected chunk0 type {json_type:#x}"
    json_bytes = data[20 : 20 + json_len]
    j = json.loads(json_bytes)

    changed = 0
    for mat in j.get("materials", []):
        if mat.get("alphaMode") and mat["alphaMode"] != "OPAQUE":
            mat["alphaMode"] = "OPAQUE"
            mat.pop("alphaCutoff", None)
            changed += 1
        elif "alphaCutoff" in mat:
            mat.pop("alphaCutoff", None)
            changed += 1
    if changed == 0:
        print(f"  alphaMode: all {len(j.get('materials', []))} material(s) already OPAQUE")
        return

    new_json = json.dumps(j, separators=(",", ":")).encode("utf-8")
    # JSON chunk must be 4-byte aligned; pad with spaces (0x20).
    pad = (-len(new_json)) % 4
    new_json += b" " * pad

    bin_chunk = data[20 + json_len :]  # the rest is the BIN chunk header + payload
    new_total = 12 + 8 + len(new_json) + len(bin_chunk)

    with open(glb_path, "wb") as f:
        f.write(b"glTF")
        f.write(struct.pack("<II", 2, new_total))
        f.write(struct.pack("<II", len(new_json), 0x4E4F534A))
        f.write(new_json)
        f.write(bin_chunk)
    print(f"  alphaMode: forced {changed} material(s) to OPAQUE (post-export rewrite)")


if __name__ == "__main__":
    main()
