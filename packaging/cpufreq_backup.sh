#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
#
# Captures the pristine hardware range of every cpufreq policy before the
# first write of a boot. The min/max reference comes from cpuinfo_min_freq and
# cpuinfo_max_freq, not from a live snapshot: a vendor boot boost or a stuck
# perf HAL hint can leave scaling_min_freq pinned at the maximum, and that must
# never be saved as "original". The governor is captured live.
#
# Aegis publishes a floor through scaling_min_freq and never lowers the
# hardware ceiling, so this restore range is the widest safe range.

DATA=/data/adb/tempo
ORIG=$DATA/orig
ROOT=/sys/devices/system/cpu/cpufreq

boot_id=$(cat /proc/sys/kernel/random/boot_id 2>/dev/null)
if [ -f "$ORIG/boot_id" ] && [ "$(cat "$ORIG/boot_id" 2>/dev/null)" = "$boot_id" ]; then
    exit 0
fi

rm -rf "$ORIG"
mkdir -p "$ORIG"

for dir in "$ROOT"/policy*; do
    [ -r "$dir/scaling_max_freq" ] || continue
    name=${dir##*/}
    mkdir -p "$ORIG/$name"
    cat "$dir/cpuinfo_min_freq" > "$ORIG/$name/min_freq" 2>/dev/null || \
        cat "$dir/scaling_min_freq" > "$ORIG/$name/min_freq" 2>/dev/null
    cat "$dir/cpuinfo_max_freq" > "$ORIG/$name/max_freq" 2>/dev/null || \
        cat "$dir/scaling_max_freq" > "$ORIG/$name/max_freq" 2>/dev/null
    cat "$dir/scaling_governor" > "$ORIG/$name/governor" 2>/dev/null
done

echo "$boot_id" > "$ORIG/boot_id"
echo "aegis: captured pristine cpufreq state in $ORIG"
