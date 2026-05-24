#!/usr/bin/env bash
# Summarize a wisp-net-test session. Each replicated entity has a stable
# `net_id` (server-assigned), so correlations + divergence key on that.
#
# Output: JSON on stdout.

set -uo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <session_dir>" >&2
    exit 2
fi

session_dir="$1"
if [[ ! -d "$session_dir" ]]; then
    echo "no such dir: $session_dir" >&2
    exit 2
fi

shopt -s nullglob
files=("$session_dir"/*.jsonl)
shopt -u nullglob

if (( ${#files[@]} == 0 )); then
    echo "{\"session_dir\": \"$session_dir\", \"error\": \"no jsonl files\"}"
    exit 0
fi

# Build a single merged JSON array of every event, tagged with `src`.
# Writing to a temp file rather than a shell variable avoids ARG_MAX
# (--argjson on multi-MB payloads otherwise fails with "Argument list too
# long"). slurpfile then reads it back into one array binding.
tmp_all=$(mktemp)
trap 'rm -f "$tmp_all"' EXIT
{
    for f in "${files[@]}"; do
        base=$(basename "$f" .jsonl)
        jq -c --arg base "$base" '. + {src: (.src // $base)}' "$f" 2>/dev/null
    done | jq -s '.'
} > "$tmp_all"

# --slurpfile binds an *array of values* read from the file; our file
# already contains one big JSON array, so [0] indexes into it.
SLURP=(--slurpfile slurped "$tmp_all")
ALL_EXPR='$slurped[0]'

counts=$(jq -n "${SLURP[@]}" "
    ${ALL_EXPR}
    | group_by(.src)
    | map({
        key: .[0].src,
        value: (group_by(.kind) | map({key: .[0].kind, value: length}) | from_entries)
      })
    | from_entries
")

correlate() {
    local server_kind="$1"
    local arrival_kind="$2"
    jq -n "${SLURP[@]}" --arg sk "$server_kind" --arg ak "$arrival_kind" "
        (${ALL_EXPR} | map(select(.src == \"server\" and .kind == \$sk and .net_id != null))) as \$spawns
        | \$spawns | map(
            . as \$s
            | {
                spawned_at: \$s.t,
                net_id: \$s.net_id,
                pos: \$s.pos,
                client_id: \$s.client_id,
                slot: \$s.slot,
                receivers: (
                    (${ALL_EXPR} | map(select(.kind == \$ak and .src != \"server\" and .net_id == \$s.net_id)))
                    | group_by(.src)
                    | map(min_by(.t))
                    | map({src: .src, t: .t, latency_s: ((.t - \$s.t) * 10000 | round / 10000)})
                )
              }
        )
    "
}

players=$(correlate player_spawned replicated_player)
props=$(correlate prop_spawned replicated_prop)
lanterns=$(correlate lantern_spawned replicated_lantern)
portals=$(correlate portal_placed replicated_portal)

divergence=$(jq -n "${SLURP[@]}" "
    [\"player_position\",\"prop_position\",\"lantern_position\",\"portal_position\"] as \$kinds
    | (${ALL_EXPR} | map(select(.kind | IN(\$kinds[])) | select(.net_id != null and .pos != null)))
    | group_by(.net_id)
    | map(
        . as \$events
        # Per-entity, take each observer's MOST-RECENT sample. Compute the
        # position delta and the time-skew between the samples. A real
        # divergence is delta > tolerance AND time-skew is small. If the
        # time-skew is large, the entity was moving fast enough that the
        # delta is sampling-time noise, not a sync bug — flag it as
        # \"snapshot_skew\" so the harness can choose how to treat it.
        | (group_by(.src) | map(max_by(.t))) as \$last
        | (\$last | length) as \$n
        | if \$n < 2 then empty
          else
              (\$last | map(.t) | max) as \$t_max
              | (\$last | map(.t) | min) as \$t_min
              | (\$t_max - \$t_min) as \$t_skew
              | (
                  [
                      range(0; \$n-1) as \$i
                      | range(\$i+1; \$n) as \$j
                      | [
                          (\$last[\$i].pos[0] - \$last[\$j].pos[0] | fabs),
                          (\$last[\$i].pos[1] - \$last[\$j].pos[1] | fabs),
                          (\$last[\$i].pos[2] - \$last[\$j].pos[2] | fabs)
                        ] | max
                  ] | max
                ) as \$delta
              | (\$delta * 10000 | round / 10000) as \$delta_rounded
              | (\$t_skew * 10000 | round / 10000) as \$skew_rounded
              | {
                  net_id: \$events[0].net_id,
                  observers: (\$last | map({key: .src, value: .pos}) | from_entries),
                  t_skew_s: \$skew_rounded,
                  max_axis_delta: \$delta_rounded,
                  # Strict divergence: position delta exceeds tolerance AND
                  # the samples were taken close in time. A 50ms window is
                  # about 3 server ticks at 60Hz — close enough that any
                  # real sync drift should already be visible.
                  diverged: ((\$delta > 0.1) and (\$t_skew < 0.05)),
                  snapshot_skew: (\$t_skew >= 0.05)
              }
          end
      )
    | map({key: (.net_id | tostring), value: (. | del(.net_id))})
    | from_entries
")

sources=$(jq -n "${SLURP[@]}" "${ALL_EXPR} | map(.src) | unique")

# Cast chains: every server-side `cast_dispatched` event that carries a
# `caused_by_spell` is the child end of a parent → child link. We emit
# one entry per dispatch with the child + parent ids, chain depth, the
# captured charge propagated, and the wall-clock time. The harness uses
# this to assert e.g. "the fireball child dispatch happened after the
# fireball was thrown, with chain_depth 1."
cast_chains=$(jq -n "${SLURP[@]}" "
    ${ALL_EXPR}
    | map(select(.src == \"server\" and .kind == \"cast_dispatched\" and .caused_by_spell != null))
    | sort_by(.t)
    | map({
        parent_spell: .caused_by_spell,
        parent_cast:  .caused_by_cast,
        child_spell:  .spell,
        child_cast:   .cast,
        chain_depth:  .chain_depth,
        charge:       .charge,
        t:            .t
      })
")

# Damage events: every server-side `damage_applied`. Ordered by time
# so harness checks can assert e.g. "the last hp_after value for
# observer-2's net_id was 0 before the respawn."
damage_events=$(jq -n "${SLURP[@]}" "
    ${ALL_EXPR}
    | map(select(.src == \"server\" and .kind == \"damage_applied\"))
    | sort_by(.t)
    | map({
        target: .target,
        source: .source,
        amount: .amount,
        hp_after: .hp_after,
        t: .t
      })
")

# Deaths: combines server-side `entity_died` (non-player despawns) and
# `player_respawned` (player respawn). Useful for asserting that the
# fireball script actually killed someone, irrespective of entity class.
deaths=$(jq -n "${SLURP[@]}" "
    ${ALL_EXPR}
    | map(select(.src == \"server\" and (.kind == \"entity_died\" or .kind == \"player_respawned\")))
    | sort_by(.t)
    | map({
        kind: .kind,
        entity: .entity,
        killer: .killer,
        client_id: .client_id,
        pos: .pos,
        t: .t
      })
")

jq -n \
    --arg dir "$session_dir" \
    --argjson sources "$sources" \
    --argjson counts "$counts" \
    --argjson players "$players" \
    --argjson props "$props" \
    --argjson lanterns "$lanterns" \
    --argjson portals "$portals" \
    --argjson divergence "$divergence" \
    --argjson cast_chains "$cast_chains" \
    --argjson damage_events "$damage_events" \
    --argjson deaths "$deaths" '
{
    session_dir: $dir,
    sources: $sources,
    event_counts: $counts,
    players: $players,
    props: $props,
    lanterns: $lanterns,
    portals: $portals,
    position_divergence: $divergence,
    cast_chains: $cast_chains,
    damage_events: $damage_events,
    deaths: $deaths
}
'
