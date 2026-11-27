#!/bin/sh
set -eu

candidate=$(realpath "$1")
bins=$(realpath "$2")
baseline_tag=$3
root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
verifier="$root/scripts/verify_installer_profile.py"
version=$(dpkg-deb --field "$candidate" Version)
baseline_version=${baseline_tag#v}
if ! dpkg --compare-versions "$version" gt "$baseline_version"; then
    printf 'Candidate must be newer than the baseline.\n' >&2
    exit 1
fi
if dpkg-query -W fliuno > /dev/null 2>&1; then
    printf 'Run the lifecycle check on a fresh runner.\n' >&2
    exit 1
fi

useradd --create-home fliuno-test
home=$(getent passwd fliuno-test | cut -d: -f6)
baseline="$home/baseline"
profile="$home/profile"
runuser -u fliuno-test -- python3 "$verifier" download "$baseline_tag" linux "$baseline"
runuser -u fliuno-test -- python3 "$verifier" seed "$profile" "$baseline/fliuno-app-server" "$baseline_version"
sh "$root/packaging/debian/build.sh" "$baseline" "$home"
old_package="$home/fliuno-v$baseline_version-linux-amd64.deb"

check_installation() {
    test "$(dpkg-query -W -f='${Version}' fliuno)" = "$1"
    test "$(runuser -u fliuno-test -- sh -c 'command -v fliuno')" = /usr/bin/fliuno
    test "$(runuser -u fliuno-test -- fliuno --version)" = "fliuno $1"
    cmp /usr/bin/fliuno "$2/fliuno"
    cmp /usr/bin/fliuno-app-server "$2/fliuno-app-server"
}

check_removed() {
    test ! -e /usr/bin/fliuno
    test ! -e /usr/bin/fliuno-app-server
    test -f "$profile/.fliuno/config.toml"
    test -f "$profile/.fliuno/credentials/openai.key"
    test -f "$profile/.fliuno/state/workbench.db"
}

dpkg -i "$candidate"
check_installation "$version" "$bins"
dpkg --purge fliuno
check_removed

dpkg -i "$old_package"
check_installation "$baseline_version" "$baseline"
dpkg -i "$candidate"
check_installation "$version" "$bins"
runuser -u fliuno-test -- python3 "$verifier" verify "$profile" /usr/bin/fliuno-app-server "$version"

dpkg --purge fliuno
check_removed
dpkg -i "$candidate"
check_installation "$version" "$bins"
runuser -u fliuno-test -- python3 "$verifier" verify "$profile" /usr/bin/fliuno-app-server "$version"
