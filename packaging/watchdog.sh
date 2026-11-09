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
    sleep 2
done

if [ "$(cat "$DATA/run/clean-exit" 2>/dev/null)" = "$(printf 'aegis-clean-exit-v1\nwriter_restored=1')" ]; then
    exit 0
fi

# 主动双边控制异常退出后恢复本次启动备份，不修改温控服务或限制节点。
echo "aegis: daemon $PID exited without confirming restore; restoring cpufreq bounds" >> "$DATA/Aegis.log"
sh "$MODDIR/restore.sh" >> "$DATA/Aegis.log" 2>&1 || exit 1
