#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
#
# Captures the live policy limits before the first write of a boot.
# Hardware extrema are not a substitute for vendor/user limits: restoring them
# would silently remove an existing ceiling. The governor is captured live.

DATA=/data/adb/tempo
ORIG=$DATA/orig
ROOT=/sys/devices/system/cpu/cpufreq

boot_id=$(cat /proc/sys/kernel/random/boot_id 2>/dev/null) || exit 1
[ -n "$boot_id" ] || exit 1
if [ -f "$ORIG/boot_id" ] && [ "$(cat "$ORIG/boot_id" 2>/dev/null)" = "$boot_id" ]; then
    exit 0
fi

rm -rf "$ORIG"
mkdir -p "$ORIG" || exit 1

count=0
for dir in "$ROOT"/policy*; do
    [ -d "$dir" ] || continue
    name=${dir##*/}
    mkdir -p "$ORIG/$name" || exit 1
    min=$(cat "$dir/scaling_min_freq") || exit 1
    max=$(cat "$dir/scaling_max_freq") || exit 1
    governor=$(cat "$dir/scaling_governor") || exit 1
    [ "$min" -gt 0 ] && [ "$max" -ge "$min" ] && [ -n "$governor" ] || exit 1
    printf '%s\n' "$min" > "$ORIG/$name/min_freq" || exit 1
    printf '%s\n' "$max" > "$ORIG/$name/max_freq" || exit 1
    printf '%s\n' "$governor" > "$ORIG/$name/governor" || exit 1
    count=$((count + 1))
done

[ "$count" -gt 0 ] || exit 1
echo "$boot_id" > "$ORIG/boot_id" || exit 1
echo "aegis: captured pristine cpufreq state in $ORIG"
