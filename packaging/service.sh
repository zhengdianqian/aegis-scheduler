#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
#
# v3 boot sequence. Safety first:
#   1. recover from a previous boot that never reached the healthy state;
#   2. disable the old module id and refuse to compete with an active writer;
#   3. wait for boot_completed, the keyguard to be gone and 30 s of calm;
#   4. generate the device profile from sysfs (no hard-coded SoC);
#   5. capture the pristine governor/min/max;
#   6. start exactly one Aegis daemon, which itself stays read-only for 20 s;
#   7. only after the daemon survives 60 s is the boot guard cleared.
#
# If the daemon exits before the boot guard clears, the watchdog restores
# cpufreq and the marker disables this module on the next boot.

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

install_manager() {
    umask 077
    apk="$MODDIR/app/AegisManager.apk"
    stamp="$DATA/run/manager-installed.sha256"
    package=com.aegis.scheduler
    waited=0
    while [ "$(getprop sys.boot_completed)" != "1" ] && [ "$waited" -lt 300 ]; do
        waited=$((waited + 1))
        sleep 1
    done
    if [ "$(getprop sys.boot_completed)" != "1" ]; then
        log "manager install: boot timeout; will retry next start"
        return 1
    fi
    if [ ! -f "$apk" ]; then
        log "manager install: bundled APK missing; will retry next start"
        return 1
    fi
    digest=$(sha256sum "$apk") || {
        log "manager install: APK hash failed; will retry next start"
        return 1
    }
    digest=${digest%% *}
    installed_hash=
    [ ! -f "$stamp" ] || read -r installed_hash < "$stamp"
    installed_path=$(pm path "$package" 2>/dev/null)
    case "$installed_path" in
        package:*) [ "$installed_hash" != "$digest" ] || return 0 ;;
    esac

    stage=$(mktemp -d /data/local/tmp/aegis-manager.XXXXXX) || {
        log "manager install: staging directory failed; will retry next start"
        return 1
    }
    trap 'rm -rf "$stage"' EXIT
    trap 'exit 1' HUP INT TERM
    # Package Manager needs traversal/read access, not directory listing or writes.
    if ! cp "$apk" "$stage/AegisManager.apk" ||
        ! chmod 711 "$stage" || ! chmod 644 "$stage/AegisManager.apk"; then
        log "manager install: staging APK failed; will retry next start"
        return 1
    fi
    output=$(pm install -r "$stage/AegisManager.apk" 2>&1)
    result=$?
    log "manager install (exit $result): $output"
    if [ "$result" -ne 0 ]; then
        log "manager install failed; keeping existing app, will retry next start"
        return 1
    fi
    case "$output" in
        *Success*) ;;
        *) log "manager install: no success confirmation; will retry next start"; return 1 ;;
    esac
    installed_path=$(pm path "$package" 2>/dev/null)
    case "$installed_path" in
        package:*) ;;
        *) log "manager install: package not found after install; will retry next start"; return 1 ;;
    esac
    if ! printf '%s\n' "$digest" > "$stamp.tmp" || ! mv "$stamp.tmp" "$stamp"; then
        rm -f "$stamp.tmp"
        log "manager install: success hash not saved; will retry next start"
        return 1
    fi
    log "manager install: installed bundled APK successfully"
}

if [ "$1" = "--install-manager" ]; then
    install_manager
    exit $?
fi
# Independent of all scheduler guards, unlock checks and daemon lifetime.
nohup sh "$MODDIR/service.sh" --install-manager >> "$LOG" 2>&1 < /dev/null &

# --- 1. boot guard -----------------------------------------------------------
if [ -f "$ARMED" ]; then
    # A previous boot did not confirm a healthy start. Leave this boot's
    # kernel and vendor policy untouched.
    touch "$MODDIR/disable" || exit 1
    rm -f "$ARMED"
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

# Do not kill a manual test or race another instance's restore watchdog.
if pidof Aegis tempod >/dev/null 2>&1; then
    log "another scheduler instance is running; leaving cpufreq untouched"
    exit 0
fi

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

if ! sh "$MODDIR/scripts/profile-gen.sh" > "$PROFILE.tmp" 2>> "$LOG" || [ ! -s "$PROFILE.tmp" ]; then
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
if ! sh "$MODDIR/scripts/cpufreq_backup.sh" >> "$LOG" 2>&1; then
    log "cpufreq backup failed; refusing to start writer"
    exit 1
fi

# --- 6. start the single writer ---------------------------------------------
# The marker is written *before* the daemon may write anything. It is removed
# only after the daemon survived 60 s (20 s internal guard + margin).
rm -f "$DATA/run/clean-exit" || exit 1
date > "$ARMED" || exit 1
cd "$DATA" || exit 1
nohup "$BIN" \
    --config "$DATA/config.toml" \
    --profile "$PROFILE" \
    --run-dir "$DATA/run" \
    --log-file "$LOG" \
    >> "$LOG" 2>&1 &
pid=$!
# Cover the startup/healthy interval too: a crash must not leave a low ceiling.
nohup sh "$MODDIR/scripts/watchdog.sh" "$pid" >> "$LOG" 2>&1 &

sleep 5
if ! kill -0 "$pid" 2>/dev/null; then
    log "Aegis failed to start; watchdog handles restoration, boot guard kept"
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

# The failure watchdog remains active. Charging and vendor services are not
# modified by the scheduler.
