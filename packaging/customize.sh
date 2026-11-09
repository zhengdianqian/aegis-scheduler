#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文

MODDIR=${MODPATH:-${0%/*}}

# The old module id must never coexist with Aegis: disable it first so its
# service.sh cannot launch a second cpufreq writer after the next boot.
if [ -d /data/adb/modules/tempo_sched ]; then
    touch /data/adb/modules/tempo_sched/disable
fi

mkdir -p /data/adb/tempo/run /data/adb/tempo/profile
chmod 700 /data/adb/tempo/run
chmod 755 "$MODDIR/bin/Aegis" "$MODDIR/bin/tempoctl" 2>/dev/null
chmod 755 "$MODDIR"/*.sh "$MODDIR"/scripts/*.sh 2>/dev/null

ui_print "- 请重启设备：开机完成后将自动安装或更新 Aegis 管理器。"
ui_print "- 安装失败会保留原应用并在下次启动重试，详情见 /data/adb/tempo/Aegis.log。"
