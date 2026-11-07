#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 默默的阿文
# Tempo device profile generator. Read-only: probes sysfs and prints TOML.

set -eu

platform=$(getprop ro.board.platform 2>/dev/null || true)
[ -n "$platform" ] || platform=unknown
sdk=$(getprop ro.build.version.sdk 2>/dev/null || true)
[ -n "$sdk" ] || sdk=0

work="${TMPDIR:-/data/local/tmp}/tempo-profile.$$"
rm -rf "$work"
mkdir -p "$work"
trap 'rm -rf "$work"' EXIT HUP INT TERM

: > "$work/records"
: > "$work/caps"

for dir in /sys/devices/system/cpu/cpufreq/policy*; do
    [ -d "$dir" ] || continue
    idx=${dir##*policy}
    [ -n "$idx" ] || continue

    cpus=$(cat "$dir/related_cpus" 2>/dev/null || true)
    [ -n "$cpus" ] || cpus=$(cat "$dir/affected_cpus" 2>/dev/null || true)
    [ -n "$cpus" ] || continue

    set -- $cpus
    first=$1
    ncpu=$#
    cpu_list=$(echo "$cpus" | tr ' ' ',')

    min=$(cat "$dir/cpuinfo_min_freq" 2>/dev/null || true)
    [ -n "$min" ] || min=$(cat "$dir/scaling_min_freq" 2>/dev/null || true)
    max=$(cat "$dir/cpuinfo_max_freq" 2>/dev/null || true)
    [ -n "$max" ] || max=$(cat "$dir/scaling_max_freq" 2>/dev/null || true)
    [ -n "$min" ] && [ -n "$max" ] || continue

    freqs=""
    if [ -f "$dir/scaling_available_frequencies" ]; then
        freqs=$(cat "$dir/scaling_available_frequencies" 2>/dev/null || true)
    fi
    if [ -z "$freqs" ] && [ -f "$dir/stats/time_in_state" ]; then
        freqs=$(awk 'NR > 1 { print $1 }' "$dir/stats/time_in_state")
    fi
    if [ -z "$freqs" ] && [ -f "$dir/scaling_available_freqs" ]; then
        freqs=$(cat "$dir/scaling_available_freqs" 2>/dev/null || true)
    fi

    freq_list=$(
        {
            printf '%s\n' $freqs
            echo "$min"
            echo "$max"
        } | awk 'NF' | sort -n | uniq
    )
    freq_fmt=$(echo "$freq_list" | awk 'BEGIN { ORS = "" } { if (NR > 1) printf ", "; printf "%s", $0 }')

    cap=""
    if [ -f "/sys/devices/system/cpu/cpu$first/cpu_capacity" ]; then
        cap=$(cat "/sys/devices/system/cpu/cpu$first/cpu_capacity" 2>/dev/null || true)
    fi
    if [ -n "$cap" ]; then
        cap_src=sysfs
    else
        cap=$((max * ncpu))
        cap_src=estimated
    fi

    printf '%s|%s|%s|%s|%s|%s|%s\n' \
        "$idx" "$cpu_list" "$min" "$max" "$cap" "$cap_src" "$freq_fmt" >> "$work/records"
    printf '%s %s\n' "$cap" "$idx" >> "$work/caps"
done

if [ ! -s "$work/records" ]; then
    echo "tempo-profile-gen: no cpufreq policies found" >&2
    exit 4
fi

count=$(awk 'END { print NR }' "$work/records")
sort -n "$work/caps" > "$work/caps_sorted"
low_index=$(head -n 1 "$work/caps_sorted" | awk '{ print $2 }')
high_index=$(tail -n 1 "$work/caps_sorted" | awk '{ print $2 }')
high_capacity=$(tail -n 1 "$work/caps_sorted" | awk '{ print $1 }')

prime_index=""
if [ "$count" -ge 3 ]; then
    second_capacity=$(tail -n 2 "$work/caps_sorted" | head -n 1 | awk '{ print $1 }')
    if [ "$high_capacity" -ge $((second_capacity * 12 / 10)) ]; then
        prime_index=$high_index
    fi
fi

printf 'schema = 1\nplatform = "%s"\nsdk = %s\n\n' "$platform" "$sdk"

sort -t '|' -k 1,1n "$work/records" | while IFS='|' read -r idx cpu_list min max capacity cap_src freqs; do
    role=balanced
    if [ "$idx" = "$low_index" ]; then
        role=efficiency
    fi
    if [ -n "$prime_index" ] && [ "$idx" = "$prime_index" ]; then
        role=prime
    fi

    printf '[[cluster]]\n'
    printf 'cpus = [%s]\n' "$cpu_list"
    printf 'role = "%s"\n' "$role"
    printf 'min_khz = %s\n' "$min"
    printf 'max_khz = %s\n' "$max"
    printf 'capacity = %s\n' "$capacity"
    printf 'freq_khz = [%s]\n' "$freqs"
    printf 'capacity_source = "%s"\n\n' "$cap_src"
done
