#!/system/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 默默的阿文
# Preparation is not activation or complete vendor scheduling takeover.
# No evidence establishes that an overlay is visible before the perf HAL reads
# these files. No boot hook, overlay, mount, stop, pause or permission changes.

DATA=/data/adb/tempo
STATE=$DATA/shield.state
STATUS=disabled_unverified_hal_timing
FINGERPRINT='Xiaomi/babylon/babylon:16/BP2A.250605.031.A3/OS3.0.304.0.WMVCNXM:user/release-keys'
HASH_CONFIG=e902d1905d6ce22bb62605855f73bc72553cc939ecf11d01ec9acc532c78f25d
HASH_SELECTION=57f93db729acbff95f5ab88bce908a433d02f3c72bc944e3c54d83f22f1ca87b
SOURCE=/vendor/etc/perf

hash_file() {
    digest=$(sha256sum "$1") || return 1
    digest=${digest%% *}
    [ "${#digest}" -eq 64 ] || return 1
    case "$digest" in *[!0-9a-f]*) return 1 ;; esac
    printf '%s\n' "$digest"
}

prepare() (
    # Explicit local preparation only. Never write to module/system or /vendor.
    [ "$(getprop ro.build.fingerprint)" = "$FINGERPRINT" ] || {
        echo 'aegis-shield: rejected fingerprint' >&2; exit 1;
    }
    [ "$(hash_file "$SOURCE/perfboostsconfig.xml")" = "$HASH_CONFIG" ] &&
        [ "$(hash_file "$SOURCE/perfboostselection.xml")" = "$HASH_SELECTION" ] || {
        echo 'aegis-shield: rejected source hashes' >&2; exit 1;
    }
    bin=$1
    [ -n "$bin" ] && [ -x "$bin" ] || {
        echo 'aegis-shield: prepare requires the local Aegis binary path' >&2; exit 2;
    }
    mkdir -p "$DATA/vendor-policy" || exit 1
    stage=$(mktemp -d "$DATA/vendor-policy/prepared.XXXXXXXX") || exit 1
    trap 'rm -rf "$stage"' 0
    trap 'exit 1' 1 2 15
    cp "$SOURCE/perfboostsconfig.xml" "$stage/source-perfboostsconfig.xml" &&
        cp "$SOURCE/perfboostselection.xml" "$stage/source-perfboostselection.xml" || exit 1
    # Verify the actual snapshots passed to the converter, not just the sources.
    [ "$(hash_file "$stage/source-perfboostsconfig.xml")" = "$HASH_CONFIG" ] &&
        [ "$(hash_file "$stage/source-perfboostselection.xml")" = "$HASH_SELECTION" ] || exit 1
    for name in perfboostsconfig.xml perfboostselection.xml; do
        "$bin" --vendor-policy-transform "$stage/source-$name" \
            --vendor-policy-output "$stage/$name" || exit 1
    done
    out_config=$(hash_file "$stage/perfboostsconfig.xml") &&
        out_selection=$(hash_file "$stage/perfboostselection.xml") || exit 1
    [ "$(getprop ro.build.fingerprint)" = "$FINGERPRINT" ] &&
        [ "$(hash_file "$SOURCE/perfboostsconfig.xml")" = "$HASH_CONFIG" ] &&
        [ "$(hash_file "$SOURCE/perfboostselection.xml")" = "$HASH_SELECTION" ] || exit 1
    {
        printf 'policy_version=1\nstatus=%s\nfingerprint=%s\n' "$STATUS" "$FINGERPRINT"
        printf 'source_dir=%s\n' "$SOURCE"
        printf 'source_sha256.perfboostsconfig.xml=%s\n' "$HASH_CONFIG"
        printf 'source_sha256.perfboostselection.xml=%s\n' "$HASH_SELECTION"
        printf 'output_sha256.perfboostsconfig.xml=%s\n' "$out_config"
        printf 'output_sha256.perfboostselection.xml=%s\n' "$out_selection"
        printf 'scope=Config:0x1080,0x1081;remove:0x40800000,0x40800100,0x40800200,0x40C00000\n'
        printf 'activation=forbidden;requires_verified_early_hal_read_timing_and_new_code_review\n'
    } > "$stage/manifest" || exit 1
    trap - 0 1 2 15
    echo "aegis-shield: prepared=$stage status=$STATUS; no overlay activated"
)

apply() {
    # Hard gate: neither a manifest nor a user flag can bypass missing evidence.
    echo "aegis-shield: $STATUS; no mounts, service changes or permission changes"
}

restore() {
    [ -f "$STATE" ] || {
        echo 'aegis-shield: no legacy state; reboot required if a legacy overlay was active'
        return 0
    }
    pending=0
    while IFS= read -r entry || [ -n "$entry" ]; do
        case "$entry" in
            svc:*)
                service=${entry#svc:}
                # Only known legacy vendor services may be restarted.
                case "$service" in
                    perfservice|perf-hal-2-3|miuibooster|mi_thermald|android.thermal-hal|millet_core)
                        current=$(getprop "init.svc.$service")
                        if [ "$current" = stopped ]; then
                            setprop ctl.start "$service" || pending=1
                            [ "$(getprop "init.svc.$service")" = running ] || pending=1
                        elif [ "$current" != running ]; then
                            pending=1
                        fi
                        ;;
                    *) pending=1 ;;
                esac
                ;;
            pid:*)
                # Old pid:number[:name] lacks boot ID/start time: PID reuse makes
                # any signal unsafe, even when the current process name matches.
                echo 'aegis-shield: legacy PID identity unverified; no signal sent; reboot required' >&2
                pending=1
                ;;
            '') ;;
            *) pending=1 ;;
        esac
    done < "$STATE"
    if [ "$pending" -ne 0 ]; then
        echo 'aegis-shield: legacy state retained; reboot required for safe recovery' >&2
        return 1
    fi
    rm -f "$STATE" || return 1
    echo 'aegis-shield: legacy services restored; reboot required if a legacy overlay was active'
}

case "$1" in
    prepare) prepare "$2" ;;
    apply) apply ;;
    restore) restore ;;
    *) echo 'usage: shield.sh prepare /path/to/Aegis | apply | restore' >&2; exit 2 ;;
esac
