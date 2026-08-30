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
authenticate_target=/usr/syno/synoman/webman/authenticate.cgi
authenticate_helper_parent=$(dirname -- "$authenticate_helper")
fixture_cookie=id=sdsync-fixture-session
fixture_synology_token=sdsync-fixture%2B%2F%3D
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
            "$physical_var/log/audit.log" \
            "$physical_var/log/activity.log" \
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
        echo "--- private runtime metadata" >&2
        for diagnostic_path in \
            "$physical_var/log" \
            "$physical_var/state/audit-outbox" \
            "$physical_var/control/requests" \
            "$physical_var/control/processing" \
            "$physical_var/control/responses" \
            "$physical_var/control/staging"
        do
            if [ -e "$diagnostic_path" ] && [ ! -L "$diagnostic_path" ]; then
                stat -c '%n %u:%g %a %h' "$diagnostic_path" >&2 || true
            fi
        done
        for diagnostic_record in \
            "$physical_var/state/audit-outbox"/*.event \
            "$physical_var/control/requests"/*.json \
            "$physical_var/control/processing"/*.json \
            "$physical_var/control/responses"/*.json
        do
            if [ -f "$diagnostic_record" ] && [ ! -L "$diagnostic_record" ]; then
                echo "--- $diagnostic_record" >&2
                stat -c '%n %u:%g %a %h' "$diagnostic_record" >&2 || true
                sed -n '1,40p' "$diagnostic_record" >&2 || true
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
[ "${QUERY_STRING:-}" = "SynoToken=sdsync-fixture%2B%2F%3D" ] || exit 1
[ "${HTTP_COOKIE:-}" = id=sdsync-fixture-session ] || exit 1
[ "${HTTP_X_SYNO_TOKEN:-}" = sdsync-fixture%2B%2F%3D ] || exit 1
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
ln -s ../authenticate.cgi "$authenticate_helper"
# The fallback decision must be based on the package identity's execute probe,
# not on userspace trust assumptions about a helper it cannot execute. Make the
# helper parent deliberately incompatible with the direct-helper metadata rules
# while preserving the documented DSM root:system 0750 EACCES condition.
chmod 0775 "$authenticate_helper_parent"
system_gid=$(awk -F: '$1 == "system" { print $3; exit }' /etc/group)
[ -n "$system_gid" ] || {
    echo "fixture system group has no numeric gid" >&2
    exit 73
}
[ -L "$authenticate_helper" ] \
    && [ "$(readlink "$authenticate_helper")" = ../authenticate.cgi ] \
    && [ "$(stat -c '%u' "$authenticate_helper")" = 0 ] \
    && [ "$(stat -c '%u:%a' "$authenticate_helper_parent")" = "0:775" ] \
    && [ "$(stat -c '%u:%g:%a:%h' "$authenticate_target")" = "0:$system_gid:750:1" ] || {
    echo "mock DSM authentication helper did not enter the metadata-unsafe EACCES phase" >&2
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
case $request_line in *SynoToken*|*sdsync-fixture%2B%2F%3D*) exit 1 ;; esac
cookie_seen=false
token_seen=false
while IFS= read -r header
do
    header=$(printf '%s' "$header" | tr -d '\r')
    [ -n "$header" ] || break
    case $(printf '%s' "$header" | tr '[:upper:]' '[:lower:]') in
        'cookie: id=sdsync-fixture-session') cookie_seen=true ;;
        'x-syno-token: sdsync-fixture%2b%2f%3d') token_seen=true ;;
    esac
done
[ "$cookie_seen" = true ] || exit 1
[ "$token_seen" = true ] || exit 1
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

run_package_cgi_get() {
    request_query=$1
    case $request_query in
        ''|*[!A-Za-z0-9_.=\&-]*)
            echo "unsafe fixture CGI query" >&2
            return 64
            ;;
    esac
    request_https=${cgi_https-on}
    request_port=${cgi_port-5001}
    su "$package_user" -s /bin/sh -c \
        "env -i \
        PATH=/usr/sbin:/usr/bin:/sbin:/bin \
        REQUEST_METHOD=GET \
        QUERY_STRING='$request_query' \
        CONTENT_LENGTH= \
        CONTENT_TYPE= \
        HTTP_TRANSFER_ENCODING= \
        HTTP_X_SDSYNC_CSRF= \
        HTTP_COOKIE=$fixture_cookie \
        HTTP_X_SYNO_TOKEN=$fixture_synology_token \
        HTTP_X_SDSYNC_REQUEST=1 \
        HTTPS=$request_https \
        REMOTE_ADDR=127.0.0.1 \
        SERVER_ADDR=127.0.0.1 \
        SERVER_NAME=localhost \
        SERVER_PORT=$request_port \
        $webman_route/api.cgi"
}

run_package_cgi() {
    run_package_cgi_get action=csrf
}

run_package_cgi_post() {
    request_body=$1
    request_csrf=$2
    case $request_csrf in
        ''|*[!A-Za-z0-9._-]*)
            echo "unsafe fixture CSRF token" >&2
            return 64
            ;;
    esac
    request_length=$(printf '%s' "$request_body" | wc -c | tr -d ' ')
    case $request_length in ''|*[!0-9]*) return 73 ;; esac
    request_https=${cgi_https-on}
    request_port=${cgi_port-5001}
    printf '%s' "$request_body" | su "$package_user" -s /bin/sh -c \
        "env -i \
        PATH=/usr/sbin:/usr/bin:/sbin:/bin \
        REQUEST_METHOD=POST \
        QUERY_STRING= \
        CONTENT_LENGTH=$request_length \
        CONTENT_TYPE=application/json \
        HTTP_TRANSFER_ENCODING= \
        HTTP_X_SDSYNC_CSRF='$request_csrf' \
        HTTP_COOKIE=$fixture_cookie \
        HTTP_X_SYNO_TOKEN=$fixture_synology_token \
        HTTP_X_SDSYNC_REQUEST=1 \
        HTTPS=$request_https \
        REMOTE_ADDR=127.0.0.1 \
        SERVER_ADDR=127.0.0.1 \
        SERVER_NAME=localhost \
        SERVER_PORT=$request_port \
        $webman_route/api.cgi"
}

require_cgi_json_response() {
    response_label=$1
    response_exit=$2
    response_expected_status=$3
    response_expected_schema=$4
    response_payload=$5
    response_status_line=$(printf '%s\n' "$response_payload" | sed -n '1p')
    response_content_length=$(printf '%s\n' "$response_payload" |
        sed -n 's/^Content-Length: \([0-9][0-9]*\)$/\1/p' | sed -n '1p')
    response_body=$(printf '%s\n' "$response_payload" | sed '1,/^$/d')
    [ "$response_exit" -eq 0 ] \
        && [ "$response_status_line" = "Status: $response_expected_status" ] || {
        echo "$response_label did not return Status: $response_expected_status" >&2
        return 1
    }
    [ -n "$response_body" ] \
        && printf '%s\n' "$response_content_length" | grep -Eq '^[1-9][0-9]*$' \
        && [ "$(printf '%s' "$response_body" | wc -c | tr -d ' ')" = "$response_content_length" ] || {
        echo "$response_label body or Content-Length is empty or inconsistent" >&2
        return 1
    }
    printf '%s\n' "$response_body" | grep -Fq "\"schema\":\"$response_expected_schema\"" || {
        echo "$response_label returned an unexpected schema" >&2
        return 1
    }
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

fallback_csrf_token=$(printf '%s\n' "$fallback_cgi_body" |
    sed -n 's/.*"csrf_token":"\([^"]*\)".*/\1/p' | sed -n '1p')
printf '%s\n' "$fallback_csrf_token" | awk -F. '
    NF == 5 && $1 == "v1" && $2 ~ /^[0-9]+$/ && $3 ~ /^[0-9]+$/ &&
    length($4) == 32 && $4 ~ /^[0-9a-f]+$/ &&
    length($5) == 64 && $5 ~ /^[0-9a-f]+$/ { accepted = 1 }
    END { exit accepted ? 0 : 1 }
' || {
    echo "rootless DSM authentication fallback returned a malformed CSRF token" >&2
    exit 1
}
fallback_auth_requests=1

# Prove the fallback identity is accepted by the package-owned relay for a real
# manager-backed read, rather than only by the synthetic CSRF endpoint.
set +e
fallback_snapshot_response=$(cgi_https=off cgi_port=$user_service_port \
    run_package_cgi_get action=snapshot)
fallback_snapshot_status=$?
set -e
fallback_auth_requests=$((fallback_auth_requests + 1))
fallback_snapshot_response=$(printf '%s' "$fallback_snapshot_response" | tr -d '\r')
require_cgi_json_response "fallback-authenticated snapshot" "$fallback_snapshot_status" \
    "200 OK" sdsync.dsm-api.v1 "$fallback_snapshot_response"
printf '%s\n' "$fallback_snapshot_response" | grep -Fq '"private_queue":true' \
    && printf '%s\n' "$fallback_snapshot_response" | grep -Fq '"mutations":true' || {
    echo "fallback-authenticated snapshot did not expose the live private relay capabilities" >&2
    exit 1
}

# client-event records a preference notification only; it changes no package
# configuration or secret. Its asynchronous result proves the authenticated
# CGI, CSRF verifier, durable queue, controller consumer, and manager all agree.
mutation_request_id=0123456789abcdef0123456789abcdef
mutation_body="{\"schema\":\"sdsync.dsm-request.v1\",\"request_id\":\"$mutation_request_id\",\"operation\":\"client-event\",\"arguments\":{\"event\":\"interface-settings\"}}"
set +e
fallback_queued_response=$(cgi_https=off cgi_port=$user_service_port \
    run_package_cgi_post "$mutation_body" "$fallback_csrf_token")
fallback_queued_status=$?
set -e
fallback_auth_requests=$((fallback_auth_requests + 1))
fallback_queued_response=$(printf '%s' "$fallback_queued_response" | tr -d '\r')
{
    printf '%s\n' '--- fallback-authenticated client event'
    printf 'exit=%s\n' "$fallback_queued_status"
    printf '%s\n' "$fallback_queued_response"
} >> "$fallback_cgi_summary"
require_cgi_json_response "fallback-authenticated client event" "$fallback_queued_status" \
    "202 Accepted" sdsync.dsm-queued.v1 "$fallback_queued_response"
printf '%s\n' "$fallback_queued_response" | grep -Fq "\"request_id\":\"$mutation_request_id\"" \
    && printf '%s\n' "$fallback_queued_response" | grep -Fq '"state":"queued"' \
    && printf '%s\n' "$fallback_queued_response" | grep -Fq '"replayed":false' || {
    echo "fallback-authenticated client event was not durably queued as a new request" >&2
    exit 1
}
queued_job_id=$(printf '%s\n' "$fallback_queued_response" |
    sed -n 's/.*"job_id":"\([0-9a-f][0-9a-f]*\)".*/\1/p' | sed -n '1p')
printf '%s\n' "$queued_job_id" | awk '
    length($0) == 48 && $0 ~ /^[0-9a-f]+$/ { accepted = 1 }
    END { exit accepted ? 0 : 1 }
' || {
    echo "fallback-authenticated client event returned a malformed server job ID" >&2
    exit 1
}

# A client that loses the first 202 response must be able to retry the exact
# body under the same authenticated AppWindow session without publishing a
# second job or audit transaction.
set +e
fallback_replayed_response=$(cgi_https=off cgi_port=$user_service_port \
    run_package_cgi_post "$mutation_body" "$fallback_csrf_token")
fallback_replayed_status=$?
set -e
fallback_auth_requests=$((fallback_auth_requests + 1))
fallback_replayed_response=$(printf '%s' "$fallback_replayed_response" | tr -d '\r')
require_cgi_json_response "replayed fallback-authenticated client event" \
    "$fallback_replayed_status" "202 Accepted" sdsync.dsm-queued.v1 \
    "$fallback_replayed_response"
if ! { printf '%s\n' "$fallback_replayed_response" | grep -Fq "\"request_id\":\"$mutation_request_id\"" \
    && printf '%s\n' "$fallback_replayed_response" | grep -Fq "\"job_id\":\"$queued_job_id\"" \
    && printf '%s\n' "$fallback_replayed_response" | grep -Fq '"state":"queued"' \
    && printf '%s\n' "$fallback_replayed_response" | grep -Fq '"replayed":true'; }; then
    echo "exact fallback-authenticated replay did not return the original queued job" >&2
    exit 1
fi

fallback_result_complete=false
fallback_result_attempt=0
while [ "$fallback_result_attempt" -lt 45 ]; do
    fallback_result_attempt=$((fallback_result_attempt + 1))
    set +e
    fallback_result_response=$(cgi_https=off cgi_port=$user_service_port \
        run_package_cgi_get "action=result&job_id=$queued_job_id")
    fallback_result_status=$?
    set -e
    fallback_auth_requests=$((fallback_auth_requests + 1))
    fallback_result_response=$(printf '%s' "$fallback_result_response" | tr -d '\r')
    fallback_result_status_line=$(printf '%s\n' "$fallback_result_response" | sed -n '1p')
    case $fallback_result_status_line in
        "Status: 202 Accepted")
            require_cgi_json_response "pending fallback-authenticated result" \
                "$fallback_result_status" "202 Accepted" sdsync.dsm-result-status.v1 \
                "$fallback_result_response"
            printf '%s\n' "$fallback_result_response" | grep -Fq '"state":"pending"' \
                && printf '%s\n' "$fallback_result_response" | grep -Fq "\"job_id\":\"$queued_job_id\"" || {
                echo "fallback-authenticated result lost its pending job identity" >&2
                exit 1
            }
            sleep 1
            ;;
        "Status: 200 OK")
            require_cgi_json_response "completed fallback-authenticated result" \
                "$fallback_result_status" "200 OK" sdsync.dsm-result-status.v1 \
                "$fallback_result_response"
            printf '%s\n' "$fallback_result_response" | grep -Fq '"state":"complete"' \
                && printf '%s\n' "$fallback_result_response" | grep -Fq "\"job_id\":\"$queued_job_id\"" \
                && printf '%s\n' "$fallback_result_response" | grep -Fq "\"client_request_id\":\"$mutation_request_id\"" \
                && printf '%s\n' "$fallback_result_response" | grep -Fq "\"actor_uid\":$administrator_uid" \
                && printf '%s\n' "$fallback_result_response" | grep -Fq '"audit_pending":false' \
                && printf '%s\n' "$fallback_result_response" | grep -Fq '"schema":"sdsync.dsm-result.v1"' \
                && printf '%s\n' "$fallback_result_response" | grep -Fq '"ok":true' \
                && printf '%s\n' "$fallback_result_response" | grep -Fq 'Client preference change audited' || {
                echo "fallback-authenticated controller result lost terminal identity or success evidence" >&2
                exit 1
            }
            fallback_result_complete=true
            break
            ;;
        *)
            {
                printf '%s\n' "--- unexpected fallback result attempt $fallback_result_attempt"
                printf 'exit=%s\n' "$fallback_result_status"
                printf '%s\n' "$fallback_result_response"
            } >> "$fallback_cgi_summary"
            echo "fallback-authenticated result polling returned an unexpected CGI envelope" >&2
            exit 1
            ;;
    esac
done
[ "$fallback_result_complete" = true ] || {
    echo "fallback-authenticated client event did not complete within 45 seconds" >&2
    exit 1
}

set +e
fallback_activity_response=$(cgi_https=off cgi_port=$user_service_port \
    run_package_cgi_get 'action=activity&lines=200')
fallback_activity_status=$?
set -e
fallback_auth_requests=$((fallback_auth_requests + 1))
fallback_activity_response=$(printf '%s' "$fallback_activity_response" | tr -d '\r')
require_cgi_json_response "fallback-authenticated activity" "$fallback_activity_status" \
    "200 OK" sdsync.dsm-activity.v1 "$fallback_activity_response"
printf '%s\n' "$fallback_activity_response" | grep -Fq '"code":"audit.succeeded"' \
    && printf '%s\n' "$fallback_activity_response" | grep -Fq '"state":"succeeded"' \
    && printf '%s\n' "$fallback_activity_response" | grep -Fq "\"client_request_id\":\"$mutation_request_id\"" \
    && printf '%s\n' "$fallback_activity_response" | grep -Fq 'Module interface-settings succeeded' || {
    echo "fallback-authenticated Activity feed did not expose the terminal client event" >&2
    exit 1
}

set +e
fallback_logs_response=$(cgi_https=off cgi_port=$user_service_port \
    run_package_cgi_get 'action=logs&lines=200&source=audit')
fallback_logs_status=$?
set -e
fallback_auth_requests=$((fallback_auth_requests + 1))
fallback_logs_response=$(printf '%s' "$fallback_logs_response" | tr -d '\r')
require_cgi_json_response "fallback-authenticated audit logs" "$fallback_logs_status" \
    "200 OK" sdsync.dsm-logs.v1 "$fallback_logs_response"
printf '%s\n' "$fallback_logs_response" | grep -Fq '"source":"audit"' \
    && printf '%s\n' "$fallback_logs_response" | grep -Fq 'interface-settings' \
    && printf '%s\n' "$fallback_logs_response" | grep -Fq 'succeeded' \
    && printf '%s\n' "$fallback_logs_response" | grep -Fq "$mutation_request_id" || {
    echo "fallback-authenticated audit log feed did not expose the terminal client event" >&2
    exit 1
}

[ ! -e "$package_auth_marker" ] && [ ! -L "$package_auth_marker" ] || {
    echo "metadata-unsafe non-executable DSM authentication helper was unexpectedly invoked" >&2
    exit 1
}
if ! { [ -f "$user_service_marker" ] && [ ! -L "$user_service_marker" ] \
    && [ "$(stat -c '%u:%a:%h' "$user_service_marker")" = "0:600:1" ] \
    && [ "$(wc -l < "$user_service_marker" | tr -d ' ')" = "$fallback_auth_requests" ]; }; then
    echo "fixed loopback DSM user-service did not authenticate every fallback CGI request exactly once" >&2
    exit 1
fi
if grep -Fq '"stage":"dsm_authentication"' "$physical_var/log/api.log"; then
    echo "successful fallback-authenticated end-to-end flow emitted an authentication failure" >&2
    exit 1
fi

# Make only the synthetic helper callable. A near-miss system UID with the wrong
# GID must fail closed before the exact DSM 1:1 target is accepted. Requiring one
# helper marker and an unchanged fallback request count then proves the trusted
# executable helper remains primary and neither authentication path is repeated.
kill "$user_service_pid"
wait "$user_service_pid" >/dev/null 2>&1 || true
user_service_pid=
chmod 0755 "$authenticate_helper_parent"
chmod 0755 "$authenticate_target"
chown 1:0 "$authenticate_target"
set +e
wrong_owner_response=$(run_package_cgi)
wrong_owner_status=$?
set -e
wrong_owner_response=$(printf '%s' "$wrong_owner_response" | tr -d '\r')
require_cgi_json_response "wrong-owner DSM authentication helper" \
    "$wrong_owner_status" "200 OK" "sdsync.dsm-error.v1" \
    "$wrong_owner_response"
if ! { printf '%s\n' "$wrong_owner_response" \
        | grep -Fq '"code":"dsm_authentication_helper_unsafe"' \
        && printf '%s\n' "$wrong_owner_response" | grep -Fq '"status":503'; }; then
    echo "wrong-owner DSM authentication helper did not fail closed" >&2
    exit 1
fi
if [ -e "$package_auth_marker" ] || [ -L "$package_auth_marker" ]; then
    echo "wrong-owner DSM authentication helper was unexpectedly invoked" >&2
    exit 1
fi

chown 1:1 "$authenticate_target"
[ -L "$authenticate_helper" ] \
    && [ "$(stat -c '%u:%a' "$authenticate_helper_parent")" = "0:755" ] \
    && [ "$(stat -c '%u:%g:%a:%h' "$authenticate_target")" = "1:1:755:1" ] || {
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
if [ "$(wc -l < "$user_service_marker" | tr -d ' ')" != "$fallback_auth_requests" ]; then
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
