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
webman_route=/usr/syno/synoman/webman/3rdparty/$package_name
authenticate_helper=/usr/syno/synoman/webman/modules/authenticate.cgi
authenticate_target=/usr/syno/synoman/authenticate.cgi
fixture_cookie=id=sdsync-fixture-session
package_auth_marker=/tmp/sdsync-auth.$package_uid
user_service_marker=/tmp/sdsync-user-service.$package_uid
user_service_port=18080

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
    "$webman_route" "$authenticate_helper" "$authenticate_target" "$package_auth_marker" "$user_service_marker"
do
    if [ -e "$reserved_path" ] || [ -L "$reserved_path" ]; then
        echo "refusing to replace existing fixture path: $reserved_path" >&2
        exit 73
    fi
done

fixture_root=$(mktemp -d /tmp/sdsync-real-supervisor.XXXXXX)
outer=$fixture_root/outer
cgi_summary=$fixture_root/cgi-summary
fallback_cgi_summary=$fixture_root/fallback-cgi-summary
user_service_root=$fixture_root/user-service
user_service_pid=
lifecycle_log=$physical_var/log/lifecycle.log
lifecycle=$package_base/scripts/start-stop-status
mkdir -p "$outer"

dump_diagnostics() {
    result=$?
    trap - EXIT
    if [ -n "$user_service_pid" ]; then
        kill "$user_service_pid" >/dev/null 2>&1 || true
        wait "$user_service_pid" >/dev/null 2>&1 || true
    fi
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
            "$fallback_cgi_summary" \
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
if ! awk -F: '$1 == "system" { found = 1 } END { exit found ? 0 : 1 }' /etc/group; then
    addgroup -S system >/dev/null
fi
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
cat > "$authenticate_target" <<'EOF'
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
chmod 0750 "$authenticate_target"
chown 0:system "$authenticate_target"
ln -s ../../authenticate.cgi "$authenticate_helper"
system_gid=$(awk -F: '$1 == "system" { print $3; exit }' /etc/group)
[ -n "$system_gid" ] || {
    echo "fixture system group has no numeric gid" >&2
    exit 73
}
[ -L "$authenticate_helper" ] \
    && [ "$(readlink "$authenticate_helper")" = ../../authenticate.cgi ] \
    && [ "$(stat -c '%u' "$authenticate_helper")" = 0 ] \
    && [ "$(stat -c '%u:%g:%a:%h' "$authenticate_target")" = "0:$system_gid:750:1" ] || {
    echo "mock DSM authentication helper is not a root-owned symlink to root:system 0750" >&2
    exit 73
}
if su "$package_user" -s /bin/sh -c "test -x $authenticate_helper"; then
    echo "package identity unexpectedly has execute access to root:system authentication helper" >&2
    exit 73
fi

mkdir -p "$user_service_root"
cat > "$user_service_root/respond" <<'EOF'
#!/bin/sh
set -eu
IFS= read -r request_line || exit 1
request_line=$(printf '%s' "$request_line" | tr -d '\r')
[ "$request_line" = "GET /webapi/entry.cgi?api=SYNO.Core.Desktop.Initdata&version=1&method=get_user_service HTTP/1.1" ] || exit 1
cookie_seen=false
while IFS= read -r header
do
    header=$(printf '%s' "$header" | tr -d '\r')
    [ -n "$header" ] || break
    case $(printf '%s' "$header" | tr '[:upper:]' '[:lower:]') in
        'cookie: id=sdsync-fixture-session') cookie_seen=true ;;
    esac
done
[ "$cookie_seen" = true ] || exit 1
umask 077
marker=/tmp/sdsync-user-service.23101
[ ! -L "$marker" ] || exit 1
printf '%s\n' authenticated >> "$marker"
chmod 0600 "$marker"
body='{"success":true,"data":{"Session":{"user":"sdsync-admin","is_admin":true},"UserSettings":{},"AppPrivilege":[],"ServiceStatus":{}}}'
printf 'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: %s\r\nConnection: close\r\n\r\n%s' "${#body}" "$body"
EOF
chmod 0755 "$user_service_root/respond"
nc -lk -s 127.0.0.1 -p "$user_service_port" -e "$user_service_root/respond" &
user_service_pid=$!
sleep 0.1
kill -0 "$user_service_pid" >/dev/null 2>&1 || {
    echo "mock DSM loopback user-service did not start" >&2
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
# DSM's installed @appstore payload is not a mutable runtime directory. Model
# that boundary explicitly so startup cannot regress to binding inside ui/.
chmod 0555 "$physical_store/ui"
ln -s "$physical_store" "$package_base/target"
ln -s "$physical_home" "$package_base/home"
ln -s "$physical_var" "$package_base/var"
mkdir -p "$(dirname -- "$webman_route")"
ln -s "$physical_store/ui" "$webman_route"
[ -L "$webman_route" ] && [ "$(readlink -f "$webman_route")" = "$physical_store/ui" ] || {
    echo "fixture dsmuidir route does not resolve to the installed package UI" >&2
    exit 73
}

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
    request_https=${cgi_https-on}
    request_port=${cgi_port-5001}
    su "$package_user" -s /bin/sh -c \
        "env -i \
        PATH=/usr/sbin:/usr/bin:/sbin:/bin \
        REQUEST_METHOD=GET \
        QUERY_STRING=action=csrf \
        CONTENT_LENGTH= \
        CONTENT_TYPE= \
        HTTP_TRANSFER_ENCODING= \
        HTTP_X_SDSYNC_CSRF= \
        HTTP_COOKIE=$fixture_cookie \
        HTTP_X_SDSYNC_REQUEST=1 \
        HTTPS=$request_https \
        REMOTE_ADDR=127.0.0.1 \
        SERVER_ADDR=127.0.0.1 \
        SERVER_NAME=localhost \
        SERVER_PORT=$request_port \
        $webman_route/api.cgi"
}

run_lifecycle start
run_lifecycle status

api_socket=$physical_var/run/api.sock
api_ready=$physical_var/run/api.ready
[ ! -e "$physical_store/ui/api.sock" ] && [ ! -L "$physical_store/ui/api.sock" ] || {
    echo "started package placed mutable API state inside the Webman UI payload" >&2
    exit 1
}
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

# A physical DSM can expose authenticate.cgi as root:system 0750. The package
# identity cannot execute that helper, so the CGI must authenticate through the
# fixed rootless DSM user-service on loopback and still reach the private relay.
set +e
fallback_cgi_response=$(cgi_https=off cgi_port=$user_service_port run_package_cgi)
fallback_cgi_status=$?
set -e
fallback_cgi_response=$(printf '%s' "$fallback_cgi_response" | tr -d '\r')
fallback_cgi_status_line=$(printf '%s\n' "$fallback_cgi_response" | sed -n '1p')
fallback_cgi_content_length=$(printf '%s\n' "$fallback_cgi_response" |
    sed -n 's/^Content-Length: \([0-9][0-9]*\)$/\1/p' | sed -n '1p')
fallback_cgi_body=$(printf '%s\n' "$fallback_cgi_response" | sed '1,/^$/d')
{
    printf 'exit=%s\n' "$fallback_cgi_status"
    printf 'status=%s\n' "$fallback_cgi_status_line"
    printf '%s\n' "$fallback_cgi_response"
} > "$fallback_cgi_summary"
chmod 0600 "$fallback_cgi_summary"
[ "$fallback_cgi_status" -eq 0 ] \
    && [ "$fallback_cgi_status_line" = "Status: 200 OK" ] || {
    echo "rootless DSM authentication fallback did not produce a successful GET envelope" >&2
    exit 1
}
[ -n "$fallback_cgi_body" ] \
    && printf '%s\n' "$fallback_cgi_content_length" | grep -Eq '^[1-9][0-9]*$' \
    && [ "$(printf '%s' "$fallback_cgi_body" | wc -c | tr -d ' ')" = "$fallback_cgi_content_length" ] || {
    echo "rootless DSM authentication fallback body or Content-Length is empty or inconsistent" >&2
    exit 1
}
printf '%s\n' "$fallback_cgi_response" | grep -Fq '"schema":"sdsync.dsm-csrf.v1"' || {
    echo "rootless DSM authentication fallback did not reach the CSRF API" >&2
    exit 1
}
[ ! -e "$package_auth_marker" ] && [ ! -L "$package_auth_marker" ] || {
    echo "non-executable DSM authentication helper was unexpectedly invoked" >&2
    exit 1
}
if ! { [ -f "$user_service_marker" ] && [ ! -L "$user_service_marker" ] \
    && [ "$(stat -c '%u:%a:%h' "$user_service_marker")" = "0:600:1" ] \
    && [ "$(wc -l < "$user_service_marker" | tr -d ' ')" = 1 ]; }; then
    echo "fixed loopback DSM user-service was not invoked exactly once" >&2
    exit 1
fi
if grep -Fq '"stage":"dsm_authentication"' "$physical_var/log/api.log"; then
    echo "successful rootless authentication fallback emitted a failure diagnostic" >&2
    exit 1
fi

# Make only the synthetic helper callable for the successful relay. Requiring
# one helper marker and one unchanged user-service marker proves the executable
# DSM helper remains primary and the daemon repeats neither authentication path.
kill "$user_service_pid"
wait "$user_service_pid" >/dev/null 2>&1 || true
user_service_pid=
chmod 0755 "$authenticate_target"
[ -L "$authenticate_helper" ] \
    && [ "$(stat -c '%u:%g:%a:%h' "$authenticate_target")" = "0:$system_gid:755:1" ] || {
    echo "mock DSM authentication helper did not enter the explicit callable test phase" >&2
    exit 73
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
    && [ "$(wc -l < "$package_auth_marker" | tr -d ' ')" = 1 ]; }; then
    echo "DSM authentication helper was not executed exactly once by the package CGI" >&2
    exit 1
fi
if [ "$(wc -l < "$user_service_marker" | tr -d ' ')" != 1 ]; then
    echo "executable DSM helper unexpectedly retried the loopback user-service" >&2
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

# A resolved, executable CGI with no live backend reports service unavailability
# inside a nonempty HTTP-200 GET envelope. That is intentionally distinct from
# a missing Webman route (HTTP 404 generated outside the package CGI).
set +e
stopped_cgi_response=$(run_package_cgi)
stopped_cgi_status=$?
set -e
stopped_cgi_response=$(printf '%s' "$stopped_cgi_response" | tr -d '\r')
[ "$stopped_cgi_status" -eq 0 ] || {
    echo "stopped installed CGI did not emit a structured HTTP response" >&2
    exit 1
}
[ "$(printf '%s\n' "$stopped_cgi_response" | sed -n '1p')" = "Status: 200 OK" ] || {
    echo "stopped installed CGI did not distinguish backend unavailability from route absence" >&2
    exit 1
}
printf '%s\n' "$stopped_cgi_response" | grep -Fq '"code":"service_unavailable"' || {
    echo "stopped installed CGI response is not the service-unavailable schema" >&2
    exit 1
}
printf '%s\n' "$stopped_cgi_response" | grep -Fq '"status":503' \
    && printf '%s\n' "$stopped_cgi_response" | grep -Fq '"stage":"bridge_connect"' || {
    echo "stopped installed CGI response lost semantic status or bridge stage" >&2
    exit 1
}

trap - EXIT
echo "real Rust supervisor, package-UID CGI, and DSM shell lifecycle passed under BusyBox"
