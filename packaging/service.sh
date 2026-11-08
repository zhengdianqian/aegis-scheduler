#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
#
# v3 boot sequence. Safety first:
#   1. recover from a previous boot that never reached the healthy state;
#   2. migrate away from the old module id and stop any legacy writer;
#   3. wait for boot_completed, the keyguard to be gone and 30 s of calm;
#   4. generate the device profile from sysfs (no hard-coded SoC);
#   5. capture the pristine governor/min/max;
#   6. start exactly one Aegis daemon, which itself stays read-only for 20 s;
#   7. only after the daemon survives 60 s is the boot guard cleared.
#
# No cpufreq node is written by the daemon before step 7. That is the
# mechanism that makes "a control that did not finish booting" impossible to
# repeat: if control.armed is still present at the next boot, this module
# disables itself before touching anything.

MODDIR=${0%/*}
DATA=/data/adb/tempo
LOG="$DATA/Aegis.log"
BIN="$MODDIR/bin/Aegis"
PROFILE="$DATA/profile/device.toml"
ARMED="$MODDIR/control.armed"

mkdir -p "$DATA/run" "$DATA/profile" "$DATA/orig"
chmod 700 "$DATA/run"

log() {
    echo "aegis: $*" >> "$LOG"
}

# --- 1. boot guard -----------------------------------------------------------
if [ -f "$ARMED" ]; then
    # A previous boot armed the control but never confirmed a healthy start.
    # Drop any legacy v2 floor mask, restore the pristine range and disable
    # ourselves so the next boot is completely stock.
    for d in /sys/devices/system/cpu/cpufreq/policy*; do
        [ -e "$d/scaling_min_freq" ] || continue
        if mount | grep -q " $d/scaling_min_freq "; then
            umount "$d/scaling_min_freq" 2>/dev/null
        fi
    done
    rm -f "$ARMED"
    touch "$MODDIR/disable"
    log "boot guard: control.armed found; module disabled, cpufreq untouched"
    exit 0
fi

# --- 2. migration -----------------------------------------------------------
for old in /data/adb/modules/tempo_sched; do
    if [ -d "$old" ] && [ ! -f "$old/disable" ]; then
        touch "$old/disable"
        log "disabled previous module $old"
    fi
done

# A stale manual start must never share the policies with the service instance.
killall Aegis 2>/dev/null
pkill -f "bin/Aegis" 2>/dev/null
sleep 1
killall -9 Aegis 2>/dev/null
pkill -9 -f "bin/Aegis" 2>/dev/null

# --- 3. boot + unlock gate ---------------------------------------------------
waited=0
while [ "$(getprop sys.boot_completed)" != "1" ] && [ "$waited" -lt 180 ]; do
    waited=$((waited + 1))
    sleep 1
done
if [ "$(getprop sys.boot_completed)" != "1" ]; then
    log "boot_completed not set after ${waited}s; leaving cpufreq untouched"
    exit 0
fi

waited=0
while [ "$waited" -lt 120 ]; do
    if ! dumpsys window 2>/dev/null | grep -qE 'isKeyguardShowing=true|mDreamingLockscreen=true'; then
        break
    fi
    waited=$((waited + 1))
    sleep 1
done
if [ "$waited" -ge 120 ]; then
    log "keyguard still up after ${waited}s; leaving cpufreq untouched"
    exit 0
fi
sleep 30

# --- 4. generic device profile ----------------------------------------------
if [ ! -f "$DATA/config.toml" ]; then
    cp "$MODDIR/config/config.toml" "$DATA/config.toml"
fi

sh "$MODDIR/scripts/profile-gen.sh" > "$PROFILE.tmp" 2>> "$LOG"
if [ ! -s "$PROFILE.tmp" ]; then
    rm -f "$PROFILE.tmp"
    log "device profile generation failed; leaving cpufreq untouched"
    exit 0
fi
mv "$PROFILE.tmp" "$PROFILE"
chmod 644 "$PROFILE"

chmod 755 "$BIN" "$MODDIR/bin/tempoctl" 2>/dev/null

if ! "$BIN" --check --config "$DATA/config.toml" --profile "$PROFILE" > /dev/null 2>&1; then
    if [ -f "$DATA/config.toml.bak" ] && \
        "$BIN" --check --config "$DATA/config.toml.bak" --profile "$PROFILE" > /dev/null 2>&1; then
        cp "$DATA/config.toml.bak" "$DATA/config.toml"
        log "config rejected, restored last known good copy"
    else
        cp "$MODDIR/config/config.toml" "$DATA/config.toml"
        log "config rejected, restored module default"
    fi
fi
if ! "$BIN" --check --config "$DATA/config.toml" --profile "$PROFILE" > /dev/null 2>&1; then
    log "configuration still invalid; leaving cpufreq untouched"
    exit 0
fi

# --- 5. pristine backup (before any write) -----------------------------------
sh "$MODDIR/scripts/cpufreq_backup.sh" >> "$LOG" 2>&1

# --- 6. start the single writer ---------------------------------------------
# The marker is written *before* the daemon may write anything. It is removed
# only after the daemon survived 60 s (20 s internal guard + margin).
date > "$ARMED" 2>/dev/null || touch "$ARMED"
cd "$DATA" || exit 1
nohup "$BIN" \
    --config "$DATA/config.toml" \
    --profile "$PROFILE" \
    --run-dir "$DATA/run" \
    --log-file "$LOG" \
    >> "$LOG" 2>&1 &
pid=$!

sleep 5
if ! kill -0 "$pid" 2>/dev/null; then
    log "Aegis failed to start; restoring original cpufreq"
    sh "$MODDIR/scripts/restore.sh" >> "$LOG" 2>&1
    rm -f "$ARMED"
    exit 0
fi
log "Aegis started (pid $pid)"

# --- 7. healthy state --------------------------------------------------------
sleep 60
if kill -0 "$pid" 2>/dev/null; then
    rm -f "$ARMED"
    log "boot guard cleared (boot completed, unlocked, daemon alive 60 s)"
else
    log "boot guard kept: daemon exited early; next boot will disable the module"
    exit 0
fi

# --- 8. failure watchdog -----------------------------------------------------
nohup sh "$MODDIR/scripts/watchdog.sh" "$pid" >> "$LOG" 2>&1 &

# --- 9. charging resume and vendor shield -----------------------------------
sh "$MODDIR/scripts/charge_resume.sh" >> "$DATA/charge.log" 2>&1
sh "$MODDIR/scripts/shield.sh" apply >> "$DATA/shield.log" 2>&1
