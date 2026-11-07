#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文

MODDIR=${0%/*}
DATA=/data/adb/tempo

mkdir -p "$DATA/run" "$DATA/profile"
chmod 700 "$DATA/run"

# Bring every policy back to the values captured before the first write. The
# pristine copy taken at boot is authoritative; the control socket is used when
# the daemon is still alive so it can also release its own bookkeeping.
if [ -x "$MODDIR/bin/tempoctl" ] && [ -S "$DATA/run/control.sock" ]; then
    "$MODDIR/bin/tempoctl" --config "$DATA/config.toml" \
        --profile "$DATA/profile/babylon.toml" \
        restore --socket "$DATA/run/control.sock" >/dev/null 2>&1
fi

killall -9 Aegis 2>/dev/null
pkill -9 -f "bin/Aegis" 2>/dev/null

sh "$MODDIR/scripts/restore.sh" >> "$DATA/Aegis.log" 2>&1
sh "$MODDIR/scripts/charge_resume.sh" >> "$DATA/charge.log" 2>&1
sh "$MODDIR/scripts/shield.sh" restore >> "$DATA/shield.log" 2>&1
rm -rf "$DATA/orig"
