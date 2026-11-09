#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
#
# Failure guard for the scheduler daemon. Started by service.sh with the pid of
# the Aegis process. If the daemon disappears for any reason (crash, OOM, kill),
# the policies are restored from the pristine copy captured at boot and the
# watchdog exits. This compatibility fallback checks process identity every 5s;
# the daemon has its own bounded safety and deadline timers.

DATA=/data/adb/tempo
MODDIR=${0%/*}
PID=$1

case "$PID" in ''|*[!0-9]*) exit 0 ;; esac

# /proc is available on all supported Android shells; no pidfd/wait extension.
# Compare starttime as well as PID so reuse cannot hide an exited daemon.
process_identity() {
    IFS= read -r stat < "/proc/$PID/stat" 2>/dev/null || return 1
    stat=${stat##*) }
    case "$stat" in Z\ *|X\ *) return 1 ;; esac
    set -- $stat
    [ "$#" -ge 20 ] || return 1
    shift 19
    printf '%s\n' "$1"
}

identity=$(process_identity)
while [ -n "$identity" ] && [ "$(process_identity)" = "$identity" ]; do
    sleep 5 || break
done

failed=0
# clean-exit confirms only the Rust writer, never legacy shield restoration.
if [ "$(cat "$DATA/run/clean-exit" 2>/dev/null)" != "$(printf 'aegis-clean-exit-v1\nwriter_restored=1')" ]; then
    echo "aegis: daemon $PID exited without confirming restore; restoring cpufreq bounds" >> "$DATA/Aegis.log"
    sh "$MODDIR/restore.sh" >> "$DATA/Aegis.log" 2>&1 || failed=1
fi
# Attempt independently even when cpufreq restore fails; preserve either error.
sh "$MODDIR/shield.sh" restore >> "$DATA/Aegis.log" 2>&1 || failed=1
exit "$failed"
