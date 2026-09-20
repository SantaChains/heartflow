#!/usr/bin/env bash
# Performance regression gate over the runtime's turn_overhead probe.
# Bash twin of scripts/bench-gate.ps1: runs `cargo test --release ...
# turn_overhead` and either saves the numbers as a machine-local baseline or
# compares the current numbers against one. Any metric more than the threshold
# (default 30%) slower exits 1, so a CI job can block silent regressions.
# Baselines are machine-specific: never commit one to the repo.
#
# Usage:
#   scripts/bench-gate.sh --save
#   scripts/bench-gate.sh
#   scripts/bench-gate.sh --baseline a.json --threshold 0.20
set -euo pipefail

save=0
threshold=0.30
baseline="${HEARTFLOW_BENCH_BASELINE:-${TMPDIR:-/tmp}/heartflow-perf-baseline.json}"

while [[ $# -gt 0 ]]; do
    case $1 in
        --save) save=1 ;;
        --baseline) baseline=$2; shift ;;
        --threshold) threshold=$2; shift ;;
        *) echo "unknown argument: $1 (usage: bench-gate.sh [--save] [--baseline path] [--threshold 0.30])" >&2; exit 2 ;;
    esac
    shift
done

cd "$(dirname "$0")/.."

echo 'running turn_overhead probe (release)...'
if ! output=$(cargo test --release -p heartflow-runtime --lib -- --ignored --nocapture turn_overhead 2>&1); then
    printf '%s\n' "$output"
    echo 'benchmark probe failed to run' >&2
    exit 1
fi

# Flatten the probe output into "group<TAB>metric<TAB>us" records. Metric rows
# are either "<name> <n> us" or a Debug Duration ("<n>ms", zero-width gap), so
# the number and unit are split off the right edge, then normalized to us.
current=$(printf '%s\n' "$output" | awk '
    /rounds=[0-9]+/ { g = $0; sub(/^.*rounds=/, "", g); sub(/[^0-9].*$/, "", g) }
    {
        line = $0
        sub(/^[ \t]+/, "", line); sub(/[ \t]+$/, "", line)
        if (line !~ /^[a-z_]+[ \t]+[0-9.]+[ \t]*(us|µs|ms|s)$/) next
        name = line; sub(/[ \t]+.*$/, "", name)
        rest = line; sub(/^[a-z_]+[ \t]+/, "", rest)
        num = rest; sub(/[ \t]*(us|µs|ms|s)$/, "", num)
        unit = rest; sub(/^[0-9.]+[ \t]*/, "", unit)
        mult = unit == "s" ? 1000000 : unit == "ms" ? 1000 : 1
        printf "%s\t%s\t%.1f\n", g, name, num * mult
    }
')
if [[ -z "$current" ]]; then
    echo 'no benchmark groups parsed; probe output changed?' >&2
    exit 1
fi

to_json() {
    awk -F'\t' '
        BEGIN { print "{" }
        $1 != prev {
            if (prev != "") print "\n  },"
            printf "  \"%s\": {", $1
            prev = $1; first = 1
        }
        {
            printf "%s\n    \"%s\": %s", first ? "" : ",", $2, $3
            first = 0
        }
        END { print "\n  }"; print "}" }
    '
}

from_json() {
    # Machine-written baseline (fixed shape) back into the TSV form above.
    awk '
        /^[ ]*"[0-9]+": \{[ \t]*$/ { g = $0; gsub(/[^0-9]/, "", g); next }
        /^[ ]*"[a-z_]+": / {
            line = $0; sub(/^[ \t]*"/, "", line)
            name = line; sub(/".*$/, "", name)
            value = line; sub(/^[^:]*:[ \t]*/, "", value); sub(/,?[ \t]*$/, "", value)
            printf "%s\t%s\t%s\n", g, name, value
        }
    ' "$1"
}

if [[ $save -eq 1 ]]; then
    to_json <<<"$current" >"$baseline"
    echo "baseline saved: $baseline"
    exit 0
fi

if [[ ! -f $baseline ]]; then
    echo "no baseline at $baseline - create one first with --save"
    exit 2
fi

# Compare: every baseline metric must still exist and stay within the
# threshold; a metric missing from the current run is a failure too.
if ! results=$(
    awk -v th="$threshold" '
        FNR == NR { base[$1 FS $2] = $3; order[++n] = $1 FS $2; next }
        {
            key = $1 FS $2; seen[key] = 1
            if (!(key in base)) { printf "rounds=%s/%s is new (no baseline value)\n", $1, $2; bad = 1; next }
            old = base[key]; new = $3
            if (new > old * (1 + th)) {
                pct = (new - old) / old * 100
                printf "rounds=%s/%s: %.1fus vs baseline %.1fus (+%.0f%%)\n", $1, $2, new, old, pct
                bad = 1
            }
        }
        END {
            for (i = 1; i <= n; i++)
                if (!(order[i] in seen)) {
                    split(order[i], parts, "\t")
                    printf "rounds=%s/%s missing from current run\n", parts[1], parts[2]
                    bad = 1
                }
            exit bad ? 1 : 0
        }' <(from_json "$baseline") <(printf '%s\n' "$current")
); then
    echo 'PERFORMANCE REGRESSION:'
    printf '%s\n' "$results"
    exit 1
fi
pct=$(awk -v t="$threshold" 'BEGIN { printf "%d", t * 100 }')
echo "PASS: no metric degraded beyond $pct% ($baseline)"
