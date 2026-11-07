#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
#
# Vendor scheduling shield.
#
# IMPORTANT: Qualcomm's `perfservice` and `perf-hal` are required by every
# Android app startup (`IPerfManager.setClientBinder`). Stopping or pausing
# them makes `bindApplication` hang and every app fail with
# "failed to complete startup". They must never be touched.
#
# Aegis therefore no longer stops any vendor HAL or daemon. The hardware
# ceiling is kept intact and the scheduler only publishes a floor, so a vendor
# boost hint can raise the floor for a moment but can never starve an app.
# `restore` is kept so a device that recorded an older shield state is repaired
# (services started, paused pids resumed) by `uninstall.sh` or by hand.
#
# Never touched: mi_thermald, android.thermal-hal, millet_core, perfservice,
# perf-hal-2-3, miuibooster.

DATA=/data/adb/tempo
STATE=$DATA/shield.state

mkdir -p "$DATA"
touch "$STATE"

apply() {
    # Intentionally empty: no vendor service is stopped or paused.
    echo "aegis-shield: vendor services left running"
}

restore() {
    [ -f "$STATE" ] || return 0
    while IFS= read -r entry; do
        case "$entry" in
            svc:*)
                service=${entry#svc:}
                current=$(getprop "init.svc.$service")
                [ "$current" = "stopped" ] && setprop "ctl.start" "$service"
                ;;
            pid:*)
                pid=${entry#pid:}
                pid=${pid%%:*}
                [ -d "/proc/$pid" ] && kill -CONT "$pid" 2>/dev/null
                ;;
        esac
    done < "$STATE"
    rm -f "$STATE"
    echo "aegis-shield: previous vendor state restored"
}

case "$1" in
    apply) apply ;;
    restore) restore ;;
    *) echo "usage: shield.sh apply|restore" >&2; exit 2 ;;
esac
