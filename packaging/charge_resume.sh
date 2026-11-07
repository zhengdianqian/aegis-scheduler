#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
#
# Resume charging. Called on module start and on removal so a stopped charger
# can never outlive the module. Vendor thermal and charge limits are untouched;
# only the suspend switch Aegis may have set is cleared.

for node in \
    /sys/class/qcom-battery/input_suspend \
    /sys/class/power_supply/battery/input_suspend \
    /sys/class/power_supply/battery/battery_input_suspend; do
    [ -w "$node" ] && echo 0 > "$node"
done

for node in \
    /sys/class/power_supply/battery/charging_enabled \
    /sys/class/power_supply/battery/battery_charging_enabled \
    /sys/class/power_supply/battery/charge_enabled; do
    [ -w "$node" ] && echo 1 > "$node"
done
