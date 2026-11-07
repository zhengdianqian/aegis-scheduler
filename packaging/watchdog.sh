#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
#
# Failure guard for the scheduler daemon. Started by service.sh with the pid of
# the Aegis process. If the daemon disappears for any reason (crash, OOM, kill),
# the policies are restored from the pristine copy captured at boot and the
# watchdog exits. Nothing else runs on a timer; the only wakeup is this sleep.

DATA=/data/adb/tempo
MODDIR=${0%/*}
PID=$1

[ -n "$PID" ] || exit 0

while [ -d "/proc/$PID" ]; do
    sleep 60
done

echo "aegis: daemon $PID exited; restoring original cpufreq" >> "$DATA/Aegis.log"
sh "$MODDIR/restore.sh" >> "$DATA/Aegis.log" 2>&1
