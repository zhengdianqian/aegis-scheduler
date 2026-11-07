#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
#
# Boot sequence, safety first:
#   1. migrate the previous module id and stop any legacy writer;
#   2. wait for `sys.boot_completed` (max 180 s) and let the system settle;
#   3. capture the pristine governor/min/max of every policy;
#   4. start exactly one Aegis daemon;
#   5. if it dies immediately, restore the captured values;
#   6. only after the daemon is up, apply the vendor shield.
#
# No cpufreq node is written before step 4. Forcing a low ceiling while system
# services are still starting is what used to hang the boot.

MODDIR=${0%/*}
DATA=/data/adb/tempo
LOG="$DATA/Aegis.log"
BIN="$MODDIR/bin/Aegis"

mkdir -p "$DATA/run" "$DATA/profile" "$DATA/orig"
chmod 700 "$DATA/run"

log() {
    echo "aegis: $*" >> "$LOG"
}

# --- 1. migration -----------------------------------------------------------
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

# --- 2. boot gate -----------------------------------------------------------
waited=0
while [ "$(getprop sys.boot_completed)" != "1" ] && [ "$waited" -lt 180 ]; do
    waited=$((waited + 1))
    sleep 1
done
if [ "$(getprop sys.boot_completed)" != "1" ]; then
    log "boot_completed not set after ${waited}s; leaving cpufreq untouched"
    exit 0
fi

# Let vendor boot boosts and the first framework storms drain. Writes only
# start after this window; the daemon repeats the same gate internally.
sleep 10

if [ ! -f "$DATA/config.toml" ]; then
    cp "$MODDIR/config/config.toml" "$DATA/config.toml"
fi
if [ ! -f "$DATA/profile/babylon.toml" ]; then
    cp "$MODDIR/config/babylon.toml" "$DATA/profile/babylon.toml"
fi

chmod 755 "$BIN" "$MODDIR/bin/tempoctl" 2>/dev/null

# Never boot with an invalid or half-written configuration.
if ! "$BIN" --check --config "$DATA/config.toml" \
        --profile "$DATA/profile/babylon.toml" > /dev/null 2>&1; then
    if [ -f "$DATA/config.toml.bak" ] && \
        "$BIN" --check --config "$DATA/config.toml.bak" \
            --profile "$DATA/profile/babylon.toml" > /dev/null 2>&1; then
        cp "$DATA/config.toml.bak" "$DATA/config.toml"
        log "config rejected, restored last known good copy"
    else
        cp "$MODDIR/config/config.toml" "$DATA/config.toml"
        log "config rejected, restored module default"
    fi
fi

# --- 3. pristine backup (once per boot, before any write) --------------------
sh "$MODDIR/scripts/cpufreq_backup.sh" >> "$LOG" 2>&1

# --- 4. start the single writer ---------------------------------------------
cd "$DATA" || exit 1
nohup "$BIN" \
    --config "$DATA/config.toml" \
    --profile "$DATA/profile/babylon.toml" \
    --run-dir "$DATA/run" \
    --log-file "$LOG" \
    >> "$LOG" 2>&1 &
pid=$!

sleep 5
if ! kill -0 "$pid" 2>/dev/null; then
    log "Aegis failed to start; restoring original cpufreq"
    sh "$MODDIR/scripts/restore.sh" >> "$LOG" 2>&1
    exit 0
fi
log "Aegis started (pid $pid)"

# --- 5. failure watchdog -----------------------------------------------------
# A sleeping watchdog costs one wake per minute and guarantees the policies are
# restored even when the daemon is killed (OOM, crash, manual kill) and its own
# Drop guard never runs.
nohup sh "$MODDIR/scripts/watchdog.sh" "$pid" >> "$LOG" 2>&1 &

# --- 6. charging resume and vendor shield, after boot is stable --------------
sh "$MODDIR/scripts/charge_resume.sh" >> "$DATA/charge.log" 2>&1
sh "$MODDIR/scripts/shield.sh" apply >> "$DATA/shield.log" 2>&1
