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

# Detect actual policies at install time; service.sh refreshes after reboot.
if sh "$MODDIR/scripts/profile-gen.sh" > /data/adb/tempo/profile/device.toml.install && \
   [ -s /data/adb/tempo/profile/device.toml.install ]; then
    mv /data/adb/tempo/profile/device.toml.install /data/adb/tempo/profile/device.toml
    chmod 644 /data/adb/tempo/profile/device.toml
    ui_print "- 已识别设备 CPU 簇；默认不施加省电频率上限。"
else
    rm -f /data/adb/tempo/profile/device.toml.install
    ui_print "- 安装阶段未能识别 CPU 簇，将在启动时重新识别。"
fi

ui_print "- 请重启设备：开机完成后将自动安装或更新 Aegis 管理器。"
