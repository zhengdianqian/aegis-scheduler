#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
#
# Captures the live policy limits before the first write of a boot.
# Hardware extrema are not a substitute for vendor/user limits: restoring them
# would silently remove an existing ceiling. The governor is captured live.
#
# v5.2 净化：开机引导期间厂商会把 scaling_min_freq 临时抬到频段上半区
# （实测开机 ~70 s 时 min=硬件最高频）。把助推残留记为“原始下限”会在恢复时
# 把频率重新钉死在高位，因此超过频率跨度中点的下限一律按硬件最低频记录
# （上、下限区别对待：上限必须原样保留，热/电压保护不能丢）。

DATA=/data/adb/tempo
ORIG=$DATA/orig
ROOT=/sys/devices/system/cpu/cpufreq

# 读取原始下限并按跨度中点判定是否为开机助推残留，输出净化后的值。
sanitize_min() {
    live=$1
    dir=$2
    hwmin=$(cat "$dir/cpuinfo_min_freq" 2>/dev/null)
    hwmax=$(cat "$dir/cpuinfo_max_freq" 2>/dev/null)
    [ -n "$hwmin" ] && [ -n "$hwmax" ] || { printf '%s\n' "$live"; return; }
    [ "$hwmin" -le "$hwmax" ] 2>/dev/null || { printf '%s\n' "$live"; return; }
    span=$((hwmax - hwmin))
    if [ "$span" -gt 0 ] && [ "$live" -gt $((hwmin + span / 2)) ]; then
        printf '%s\n' "$hwmin"
    else
        printf '%s\n' "$live"
    fi
}

boot_id=$(cat /proc/sys/kernel/random/boot_id 2>/dev/null) || exit 1
[ -n "$boot_id" ] || exit 1
if [ -f "$ORIG/boot_id" ] && [ "$(cat "$ORIG/boot_id" 2>/dev/null)" = "$boot_id" ]; then
    # 同一次开机的既有备份：重新校验下限，净化历史助推残留（支持热升级自愈）。
    for dir in "$ROOT"/policy*; do
        [ -d "$dir" ] || continue
        name=${dir##*/}
        saved=$ORIG/$name/min_freq
        [ -f "$saved" ] || continue
        live=$(cat "$saved")
        cleaned=$(sanitize_min "$live" "$dir")
        if [ "$cleaned" != "$live" ]; then
            printf '%s\n' "$cleaned" > "$saved" || exit 1
            echo "aegis: sanitized boot-boost minimum for $name ($live -> $cleaned)"
        fi
    done
    exit 0
fi

rm -rf "$ORIG"
mkdir -p "$ORIG" || exit 1

count=0
for dir in "$ROOT"/policy*; do
    [ -d "$dir" ] || continue
    name=${dir##*/}
    mkdir -p "$ORIG/$name" || exit 1
    min=$(cat "$dir/scaling_min_freq") || exit 1
    max=$(cat "$dir/scaling_max_freq") || exit 1
    governor=$(cat "$dir/scaling_governor") || exit 1
    [ "$min" -gt 0 ] && [ "$max" -ge "$min" ] && [ -n "$governor" ] || exit 1
    min=$(sanitize_min "$min" "$dir")
    [ "$max" -ge "$min" ] || exit 1
    printf '%s\n' "$min" > "$ORIG/$name/min_freq" || exit 1
    printf '%s\n' "$max" > "$ORIG/$name/max_freq" || exit 1
    printf '%s\n' "$governor" > "$ORIG/$name/governor" || exit 1
    count=$((count + 1))
done

[ "$count" -gt 0 ] || exit 1
echo "$boot_id" > "$ORIG/boot_id" || exit 1
echo "aegis: captured pristine cpufreq state in $ORIG"
