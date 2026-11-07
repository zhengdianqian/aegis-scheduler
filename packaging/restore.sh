#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
#
# One-key restore. Writes back the pristine governor/min/max captured by
# cpufreq_backup.sh. Safe to run at any time, also from a terminal:
#   su 0 sh /data/adb/modules/aegis_scheduler/scripts/restore.sh

DATA=/data/adb/tempo
ORIG=$DATA/orig
ROOT=/sys/devices/system/cpu/cpufreq

if [ ! -d "$ORIG" ]; then
    echo "aegis: no pristine backup in $ORIG; nothing to restore"
    exit 0
fi

for dir in "$ORIG"/policy*; do
    [ -d "$dir" ] || continue
    name=${dir##*/}
    target=$ROOT/$name
    [ -d "$target" ] || continue
    # Maximum first, then minimum: an intermediate state must never be min>max.
    [ -f "$dir/max_freq" ] && cat "$dir/max_freq" > "$target/scaling_max_freq" 2>/dev/null
    [ -f "$dir/min_freq" ] && cat "$dir/min_freq" > "$target/scaling_min_freq" 2>/dev/null
    [ -f "$dir/governor" ] && cat "$dir/governor" > "$target/scaling_governor" 2>/dev/null
done

echo "aegis: cpufreq restored from $ORIG"
