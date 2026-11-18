#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
#
# Captures the live policy limits before the first write of a boot.
# The maximum is captured live: hardware extrema are not a substitute for a
# vendor/user ceiling, and restoring an extreme would silently remove it.
#
# v5.2 下限净化：备份可能抓在开机引导助推期（实测 min=硬件最高频）或外部
# 守护（powerkeeper）活动地板期（实测 min=672000 被固化）。"恢复/交还内核"
# 的语义是把下限还给硬件最低频——内核默认即如此，外部需要地板时会自行再抬；
# 上限必须原样保留（热/电压保护不能丢）。cpuinfo 不可读时保留实测值。

DATA=/data/adb/tempo
ORIG=$DATA/orig
ROOT=/sys/devices/system/cpu/cpufreq

# 输出该策略的恢复下限：cpuinfo_min_freq（不可读时退回实测值）。
sanitize_min() {
    live=$1
    dir=$2
    hwmin=$(cat "$dir/cpuinfo_min_freq" 2>/dev/null)
    if [ -n "$hwmin" ] && [ "$hwmin" -gt 0 ] 2>/dev/null; then
        printf '%s\n' "$hwmin"
    else
        printf '%s\n' "$live"
    fi
}

boot_id=$(cat /proc/sys/kernel/random/boot_id 2>/dev/null) || exit 1
[ -n "$boot_id" ] || exit 1
if [ -f "$ORIG/boot_id" ] && [ "$(cat "$ORIG/boot_id" 2>/dev/null)" = "$boot_id" ]; then
    # 同一次开机的既有备份：把下限统一净化为硬件最低频（热升级自愈）。
    for dir in "$ROOT"/policy*; do
        [ -d "$dir" ] || continue
        name=${dir##*/}
        saved=$ORIG/$name/min_freq
        [ -f "$saved" ] || continue
        live=$(cat "$saved")
        cleaned=$(sanitize_min "$live" "$dir")
        if [ "$cleaned" != "$live" ]; then
            printf '%s\n' "$cleaned" > "$saved" || exit 1
            echo "aegis: sanitized preserved minimum for $name ($live -> $cleaned)"
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
