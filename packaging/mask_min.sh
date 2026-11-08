#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
#
# Pin every policy's scaling_min_freq to the hardware minimum by bind-mounting
# a regular file over the sysfs attribute. Vendor PerfHAL keeps writing the
# attribute, but its writes land in the mask file, so it can no longer raise
# the kernel floor between two of our writes. Scheduling then relies on
# scaling_max_freq, FAS and the governor's natural ramp.
#
# Idempotent: run again after a reboot or from a terminal.
#   su 0 sh /data/adb/modules/aegis_scheduler/scripts/mask_min.sh

DATA=/data/adb/tempo
MASK=$DATA/mask
ROOT=/sys/devices/system/cpu/cpufreq
LOG="$DATA/Aegis.log"

mkdir -p "$MASK"
chmod 700 "$MASK"

log() {
    echo "aegis: $*" >> "$LOG"
}

for dir in "$ROOT"/policy*; do
    node=$dir/scaling_min_freq
    [ -e "$node" ] || continue
    name=${dir##*/}
    if mount | grep -q " $node "; then
        continue
    fi
    hw=$(cat "$dir/cpuinfo_min_freq" 2>/dev/null)
    [ -n "$hw" ] || continue
    printf '%s' "$hw" > "$MASK/$name"
    chmod 666 "$MASK/$name"
    # Real floor first, then hide the node. Writing the value after the mount
    # would only edit the mask file.
    echo "$hw" > "$node" 2>/dev/null || {
        log "mask: $node not writable"
        continue
    }
    if mount --bind "$MASK/$name" "$node" 2>/dev/null; then
        log "mask: pinned $name to $hw"
    else
        log "mask: bind failed on $node"
    fi
done
