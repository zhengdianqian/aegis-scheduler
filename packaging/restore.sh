#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
#
# One-key restore. Writes back the pristine min/max captured by
# cpufreq_backup.sh. Safe to run at any time, also from a terminal:
#   su 0 sh /data/adb/modules/aegis_scheduler/scripts/restore.sh

DATA=/data/adb/tempo
ORIG=$DATA/orig
ROOT=/sys/devices/system/cpu/cpufreq

boot_id=$(cat /proc/sys/kernel/random/boot_id) || exit 1
if [ -z "$boot_id" ] || [ "$(cat "$ORIG/boot_id" 2>/dev/null)" != "$boot_id" ]; then
    echo "aegis: no cpufreq backup for the current boot; nothing to restore"
    exit 0
fi

failed=0
# Never change governors, thermal services, or thermal limit nodes.
restore_node() {
    if [ "$(cat "$2" 2>/dev/null)" != "$1" ]; then
        printf '%s\n' "$1" > "$2" || return 1
    fi
    [ "$(cat "$2" 2>/dev/null)" = "$1" ]
}

for dir in "$ORIG"/policy*; do
    [ -d "$dir" ] || continue
    name=${dir##*/}
    target=$ROOT/$name
    min=$(cat "$dir/min_freq")
    max=$(cat "$dir/max_freq")
    current_min=$(cat "$target/scaling_min_freq")
    current_max=$(cat "$target/scaling_max_freq")
    if ! [ "$min" -gt 0 ] 2>/dev/null || ! [ "$max" -ge "$min" ] 2>/dev/null ||
       ! [ "$current_min" -gt 0 ] 2>/dev/null || ! [ "$current_max" -gt 0 ] 2>/dev/null; then
        echo "aegis: $name: incomplete backup or unavailable policy" >&2
        failed=1
        continue
    fi
    if [ "$current_min" -gt "$max" ]; then
        if ! restore_node "$min" "$target/scaling_min_freq"; then
            echo "aegis: $name: minimum restore rejected; stop this policy" >&2
            failed=1
            continue
        fi
    fi
    if ! restore_node "$max" "$target/scaling_max_freq"; then
        echo "aegis: $name: maximum restore rejected; stop this policy" >&2
        failed=1
        continue
    fi
    restore_node "$min" "$target/scaling_min_freq" || failed=1
    if [ "$(cat "$target/scaling_min_freq")" != "$min" ] ||
       [ "$(cat "$target/scaling_max_freq")" != "$max" ]; then
        echo "aegis: $name: restore readback did not match" >&2
        failed=1
    fi
done

[ "$failed" -eq 0 ] || exit 1
echo "aegis: cpufreq restored from $ORIG"
