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
        --profile "$DATA/profile/device.toml" \
        restore --socket "$DATA/run/control.sock" >/dev/null 2>&1
fi

for pid in $(pidof Aegis 2>/dev/null); do
    [ "$(readlink "/proc/$pid/exe")" = "$MODDIR/bin/Aegis" ] || continue
    kill -TERM "$pid" 2>/dev/null
    for second in 1 2 3; do
        kill -0 "$pid" 2>/dev/null || break
        sleep 1
    done
    if [ "$(readlink "/proc/$pid/exe")" = "$MODDIR/bin/Aegis" ]; then
        kill -KILL "$pid" 2>/dev/null || exit 1
    fi
done

if [ "$(cat "$DATA/run/clean-exit" 2>/dev/null)" != "$(printf 'aegis-clean-exit-v1\nwriter_restored=1')" ]; then
    sh "$MODDIR/scripts/restore.sh" >> "$DATA/Aegis.log" 2>&1 || exit 1
fi
rm -f "$MODDIR/control.armed"
