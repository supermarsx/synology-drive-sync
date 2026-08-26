#!/bin/sh
set -eu

package_name=synology-drive-sync
package_user=sdsync-package
package_group=sdsync-package
package_uid=23101
package_gid=23101
administrator_user=sdsync-admin
administrator_group=administrators
administrator_uid=23102
administrator_gid=23102
package_base=/var/packages/$package_name
physical_store=/volume3/@appstore/$package_name
physical_home=/volume3/@apphome/$package_name
physical_var=/volume3/@appdata/$package_name
authenticate_helper=/usr/syno/synoman/webman/modules/authenticate.cgi
fixture_cookie=id=sdsync-fixture-session
package_auth_marker=/tmp/sdsync-auth.$package_uid

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
for reserved_path in "$package_base" "$physical_store" "$physical_home" "$physical_var" \
    "$authenticate_helper" "$package_auth_marker"
do
    if [ -e "$reserved_path" ] || [ -L "$reserved_path" ]; then
        echo "refusing to replace existing fixture path: $reserved_path" >&2
        exit 73
    fi
done

fixture_root=$(mktemp -d /tmp/sdsync-real-supervisor.XXXXXX)
outer=$fixture_root/outer
cgi_summary=$fixture_root/cgi-summary
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
            "$physical_var/run/controller.ready" \
            "$cgi_summary"
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
addgroup -g "$administrator_gid" "$administrator_group" >/dev/null
adduser -D -H -u "$package_uid" -G "$package_group" "$package_user" >/dev/null
adduser -D -H -u "$administrator_uid" -G "$administrator_group" "$administrator_user" >/dev/null
administrator_groups=$(su "$administrator_user" -s /bin/sh -c 'id -G')
case " $administrator_groups " in
    *" $administrator_gid "*) ;;
    *)
        echo "fixture administrator is not a member of DSM's administrators group" >&2
        exit 77
        ;;
esac

mkdir -p "$(dirname -- "$authenticate_helper")"
cat > "$authenticate_helper" <<'EOF'
#!/bin/sh
set -eu
[ "${REQUEST_METHOD:-}" = GET ] || exit 1
[ "${QUERY_STRING:-}" = "" ] || exit 1
[ "${HTTP_COOKIE:-}" = id=sdsync-fixture-session ] || exit 1
[ "${HTTPS:-}" = on ] || exit 1
caller_uid=$(id -u)
[ "$caller_uid" = 23101 ] || exit 1
umask 077
auth_marker=/tmp/sdsync-auth.$caller_uid
[ ! -L "$auth_marker" ] || exit 1
printf '%s\n' authenticated >> "$auth_marker"
chmod 0600 "$auth_marker"
printf '%s\n' sdsync-admin
EOF
chmod 0755 "$authenticate_helper"
chown 0:0 "$authenticate_helper"
[ "$(stat -c '%u:%a:%h' "$authenticate_helper")" = 0:755:1 ] || {
    echo "mock DSM authentication helper is not a root-owned ordinary 0755 file" >&2
    exit 73
}

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

run_package_cgi() {
    su "$package_user" -s /bin/sh -c \
        "env -i \
        PATH=/usr/sbin:/usr/bin:/sbin:/bin \
        REQUEST_METHOD=GET \
        QUERY_STRING=action=csrf \
        HTTP_COOKIE=$fixture_cookie \
        HTTP_X_SDSYNC_REQUEST=1 \
        HTTPS=on \
        REMOTE_ADDR=127.0.0.1 \
        SERVER_ADDR=127.0.0.1 \
        SERVER_NAME=localhost \
        SERVER_PORT=5001 \
        $package_base/target/ui/api.cgi"
}

run_lifecycle start
run_lifecycle status

api_socket=$package_base/target/ui/api.sock
api_ready=$physical_var/run/api.ready
if ! { [ -S "$api_socket" ] && [ ! -L "$api_socket" ]; }; then
    echo "started package did not publish a real API socket" >&2
    exit 1
fi
[ "$(stat -c '%u:%a:%h' "$api_socket")" = "$package_uid:600:1" ] || {
    echo "started API socket is outside the package-owned 0600 contract" >&2
    exit 1
}
if ! { [ -f "$api_ready" ] && [ ! -L "$api_ready" ]; }; then
    echo "started package did not publish private API readiness" >&2
    exit 1
fi
[ "$(stat -c '%u:%a:%h' "$api_ready")" = "$package_uid:600:1" ] || {
    echo "API readiness is outside the package-owned 0600 contract" >&2
    exit 1
}
set +e
cgi_response=$(run_package_cgi)
cgi_status=$?
set -e
cgi_response=$(printf '%s' "$cgi_response" | tr -d '\r')
cgi_status_line=$(printf '%s\n' "$cgi_response" | sed -n '1p')
cgi_error_code=$(printf '%s\n' "$cgi_response" |
    sed -n 's/.*"code":"\([^"]*\)".*/\1/p' | sed -n '1p')
{
    printf 'exit=%s\n' "$cgi_status"
    printf 'status=%s\n' "$cgi_status_line"
    printf 'error_code=%s\n' "${cgi_error_code:-none}"
} > "$cgi_summary"
chmod 0600 "$cgi_summary"
if [ "$cgi_status" -ne 0 ] || [ "$cgi_status_line" != "Status: 200 OK" ]; then
    echo "installed CGI did not return HTTP 200 through the live package socket" >&2
    exit 1
fi
printf '%s\n' "$cgi_response" | grep -Fq '"schema":"sdsync.dsm-csrf.v1"' || {
    echo "installed CGI response is not the CSRF schema" >&2
    exit 1
}
csrf_token=$(printf '%s\n' "$cgi_response" |
    sed -n 's/.*"csrf_token":"\([^"]*\)".*/\1/p' | sed -n '1p')
printf '%s\n' "$csrf_token" | awk -F. '
    NF == 5 && $1 == "v1" && $2 ~ /^[0-9]+$/ && $3 ~ /^[0-9]+$/ &&
    length($4) == 32 && $4 ~ /^[0-9a-f]+$/ &&
    length($5) == 64 && $5 ~ /^[0-9a-f]+$/ { accepted = 1 }
    END { exit accepted ? 0 : 1 }
' || {
    echo "installed CGI returned a malformed CSRF token" >&2
    exit 1
}
if ! { [ -f "$package_auth_marker" ] && [ ! -L "$package_auth_marker" ] \
    && [ "$(stat -c '%u:%a:%h' "$package_auth_marker")" = "$package_uid:600:1" ] \
    && [ "$(wc -l < "$package_auth_marker" | tr -d ' ')" = 2 ]; }; then
    echo "DSM authentication helper was not executed twice by the package CGI/API identity" >&2
    exit 1
fi
csrf_key=$physical_var/control/csrf.key
if ! { [ -f "$csrf_key" ] && [ ! -L "$csrf_key" ] \
    && [ "$(stat -c '%u:%a:%h' "$csrf_key")" = "$package_uid:600:1" ]; }; then
    echo "CSRF key is outside the package-private contract" >&2
    exit 1
fi

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
echo "real Rust supervisor, package-UID CGI, and DSM shell lifecycle passed under BusyBox"
