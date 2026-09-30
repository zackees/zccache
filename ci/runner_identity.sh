#!/usr/bin/env bash
# Runner identity + contention probe for Perf Guard (#1861, #1807).
#
#   ci/runner_identity.sh begin   # print identity block, record steal/load baseline
#   ci/runner_identity.sh end     # print steal/load delta line since `begin`
#
# Observability only: run it as its own workflow step, never inside a timed
# benchmark command. Output is machine-parseable (parsed by
# ci/runner_identity.py); every key=value also goes to $GITHUB_STEP_SUMMARY.
# Linux only (the perf-guard matrix is ubuntu-24.04); elsewhere fields are
# "unknown" and the script still exits 0. It never fails the job.
set -u

state_file="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/runner-identity-baseline"

# Prints "<steal_ticks> <total_ticks>" from the aggregate cpu line of /proc/stat.
read_steal() {
  awk '/^cpu / { t = 0; for (i = 2; i <= NF; i++) t += $i; print ($9 + 0), t; exit }' \
    /proc/stat 2>/dev/null || echo "0 0"
}

# Prints "<load1> <load5>".
read_load() {
  awk '{ print $1, $2 }' /proc/loadavg 2>/dev/null || echo "unknown unknown"
}

summary() {
  [[ -n "${GITHUB_STEP_SUMMARY:-}" ]] || return 0
  printf '%s\n' "$1" >> "$GITHUB_STEP_SUMMARY" 2>/dev/null || true
}

mode="${1:-}"
case "$mode" in
  begin)
    cpu_model="$(awk -F': *' '/^model name/ { print $2; exit }' /proc/cpuinfo 2>/dev/null)"
    cpu_model="${cpu_model:-unknown}"
    cores="$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || echo unknown)"
    mem_kb="$(awk '/^MemTotal:/ { print $2; exit }' /proc/meminfo 2>/dev/null)"
    read -r steal total <<<"$(read_steal)"
    read -r load1 load5 <<<"$(read_load)"
    mkdir -p "$(dirname "$state_file")" 2>/dev/null || true
    printf '%s %s\n' "$steal" "$total" > "$state_file" 2>/dev/null || true

    lines=(
      "runner.cpu_model=$cpu_model"
      "runner.logical_cores=$cores"
      "runner.mem_total_kb=${mem_kb:-unknown}"
      "runner.kernel=$(uname -r 2>/dev/null || echo unknown)"
      "runner.image_os=${ImageOS:-unknown}"
      "runner.image_version=${ImageVersion:-unknown}"
      "runner.name=${RUNNER_NAME:-unknown}"
      "runner.arch=${RUNNER_ARCH:-$(uname -m 2>/dev/null || echo unknown)}"
      "runner.steal_ticks_begin=$steal"
      "runner.total_ticks_begin=$total"
      "runner.load1_begin=$load1"
      "runner.load5_begin=$load5"
    )
    echo "::group::runner-identity"
    echo "runner-identity-begin v=1"
    printf '%s\n' "${lines[@]}"
    echo "runner-identity-end"
    echo "::endgroup::"
    summary "### Runner identity (${GITHUB_JOB:-job} ${MATRIX_LABEL:-})"
    summary '```'
    for l in "${lines[@]}"; do summary "$l"; done
    summary '```'
    ;;
  end)
    read -r steal total <<<"$(read_steal)"
    read -r load1 load5 <<<"$(read_load)"
    steal0=0 total0=0
    if [[ -r "$state_file" ]]; then
      read -r steal0 total0 < "$state_file" || true
    fi
    d_steal=$((steal - steal0))
    d_total=$((total - total0))
    if ((d_total > 0)); then
      steal_pct="$(awk -v s="$d_steal" -v t="$d_total" 'BEGIN { printf "%.3f", 100 * s / t }')"
    else
      steal_pct="unknown"
    fi
    line="runner-identity-delta v=1 steal_ticks=$d_steal total_ticks=$d_total steal_pct=$steal_pct load1_end=$load1 load5_end=$load5"
    echo "$line"
    summary "### Runner contention (${GITHUB_JOB:-job} ${MATRIX_LABEL:-})"
    summary '```'
    summary "$line"
    summary '```'
    ;;
  *)
    echo "usage: $0 begin|end" >&2
    exit 2
    ;;
esac
exit 0
