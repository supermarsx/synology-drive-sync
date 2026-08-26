#!/bin/sh
set -eu

package_name=synology-drive-sync
package_user=sdsync-package
package_group=sdsync-package
package_uid=23101
package_gid=23101
http_uid=23102
http_gid=23102
package_base=/var/packages/$package_name
physical_store=/volume3/@appstore/$package_name
physical_home=/volume3/@apphome/$package_name
physical_var=/volume3/@appdata/$package_name

usage() {
    echo "usage: $0 /absolute/path/to/synology-drive-sync-<version>-x86_64.spk" >&2
    exit 64
}

[ "$#" -eq 1 ] || usage
spk=$1
[ "${spk#/}" != "$spk" ] || usage
[ -f "$spk" ] && [ ! -L "$spk" ] || {
    echo "real-supervisor fixture is not a regular SPK: $spk" >&2
    exit 66
}
[ "$(id -u)" -eq 0 ] || {
    echo "real-supervisor fixture must run as root inside its disposable container" >&2
    exit 77
}
[ -f /.dockerenv ] || {
    echo "refusing to create DSM FHS fixtures outside a disposable container" >&2
    exit 77
}
for reserved_path in "$package_base" "$physical_store" "$physical_home" "$physical_var"; do
    if [ -e "$reserved_path" ] || [ -L "$reserved_path" ]; then
        echo "refusing to replace existing fixture path: $reserved_path" >&2
        exit 73
    fi
done

fixture_root=$(mktemp -d /tmp/sdsync-real-supervisor.XXXXXX)
outer=$fixture_root/outer
lifecycle_log=$physical_var/log/lifecycle.log
lifecycle=$package_base/scripts/start-stop-status
mkdir -p "$outer"

dump_diagnostics() {
    result=$?
    trap - EXIT
    if [ "$result" -ne 0 ]; then
        echo "real DSM supervisor lifecycle failed with status $result" >&2
        for diagnostic in \
            "$lifecycle_log" \
            "$physical_var/log/api.log" \
            "$physical_var/log/controller.log" \
            "$physical_var/run/api.pid" \
            "$physical_var/run/api.bound" \
            "$physical_var/run/api.ready" \
            "$physical_var/run/controller.pid" \
            "$physical_var/run/controller.starting" \
            "$physical_var/run/controller.ready"
        do
            if [ -f "$diagnostic" ] && [ ! -L "$diagnostic" ]; then
                echo "--- $diagnostic" >&2
                sed -n '1,240p' "$diagnostic" >&2 || true
            fi
        done
        echo "--- processes" >&2
        ps >&2 || true
    fi
    exit "$result"
}
trap dump_diagnostics EXIT

tar -xf "$spk" -C "$outer"
[ -f "$outer/package.tgz" ] && [ ! -L "$outer/package.tgz" ] || {
    echo "SPK does not contain a regular package.tgz" >&2
    exit 65
}
[ -d "$outer/scripts" ] && [ ! -L "$outer/scripts" ] || {
    echo "SPK does not contain a regular scripts directory" >&2
    exit 65
}

addgroup -g "$package_gid" "$package_group" >/dev/null
addgroup -g "$http_gid" http >/dev/null
adduser -D -H -u "$package_uid" -G "$package_group" "$package_user" >/dev/null
adduser -D -H -u "$http_uid" -G http http >/dev/null
addgroup "$package_user" http >/dev/null
package_groups=$(su "$package_user" -s /bin/sh -c 'id -G')
case " $package_groups " in
    *" $http_gid "*) ;;
    *)
        echo "package identity is not a supplementary member of DSM's http group" >&2
        exit 77
        ;;
esac

mkdir -p "$physical_store" "$physical_home" "$physical_var" "$package_base"
tar -xzf "$outer/package.tgz" -C "$physical_store"
cp -R "$outer/scripts" "$package_base/scripts"
chmod 0755 "$package_base/scripts" "$lifecycle"
chmod 0755 "$physical_store"
chmod 0700 "$physical_home" "$physical_var"
chown -R "$package_uid:$package_gid" "$physical_store" "$physical_home" "$physical_var" \
    "$package_base/scripts"
ln -s "$physical_store" "$package_base/target"
ln -s "$physical_home" "$package_base/home"
ln -s "$physical_var" "$package_base/var"

run_lifecycle() {
    lifecycle_action=$1
    su "$package_user" -s /bin/sh -c \
        "env -i \
        PATH=/usr/sbin:/usr/bin:/sbin:/bin \
        HOME=$physical_home \
        SYNOPKG_PKGNAME=$package_name \
        SYNOPKG_PKGDEST=$physical_store \
        SYNOPKG_PKGHOME=$physical_home \
        SYNOPKG_PKGVAR=$physical_var \
        SYNOPKG_DSM_VERSION_MAJOR=7 \
        SYNOPKG_TEMP_LOGFILE=$lifecycle_log \
        SDSYNC_DSM_START_TIMEOUT=15 \
        SDSYNC_DSM_STOP_TIMEOUT=15 \
        $lifecycle $lifecycle_action"
}

run_lifecycle start
run_lifecycle status
run_lifecycle stop

set +e
run_lifecycle status
stopped_status=$?
set -e
[ "$stopped_status" -eq 3 ] || {
    echo "stopped lifecycle returned $stopped_status instead of 3" >&2
    exit 1
}

trap - EXIT
echo "real Rust supervisor and DSM shell lifecycle passed start/status/stop under BusyBox"
