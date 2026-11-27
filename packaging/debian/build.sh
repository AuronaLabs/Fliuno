#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
    printf 'Usage: sh %s BIN_DIR OUTPUT_DIR\n' "$0" >&2
    exit 2
fi

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
bins=$(CDPATH='' cd -- "$1" && pwd)
mkdir -p -- "$2"
out=$(CDPATH='' cd -- "$2" && pwd)
version=$("$bins/fliuno" --version | sed -n 's/^fliuno \([0-9]\+\.[0-9]\+\.[0-9]\+\)$/\1/p')
if [ -z "$version" ]; then
    printf 'Could not determine the Fliuno version from %s/fliuno\n' "$bins" >&2
    exit 1
fi
if [ "$(dpkg --print-architecture)" != amd64 ]; then
    printf 'Build the amd64 package on Debian amd64.\n' >&2
    exit 1
fi

stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT
package="$stage/fliuno"
install -d "$package/DEBIAN" "$package/usr/bin" "$package/usr/share/doc/fliuno"
install -m 0755 "$bins/fliuno" "$bins/fliuno-app-server" "$package/usr/bin"
install -m 0644 "$root/README.md" "$package/usr/share/doc/fliuno"
gzip -n -c "$root/CHANGELOG.md" > "$package/usr/share/doc/fliuno/changelog.gz"

cat > "$package/DEBIAN/control" <<EOF
Package: fliuno
Version: $version
Section: utils
Priority: optional
Architecture: amd64
Maintainer: AuronaLabs <auronalabs@users.noreply.github.com>
Homepage: https://github.com/AuronaLabs/Fliuno
Description: Terminal agent for working in a local project
 Fliuno provides a conversational terminal interface and a local app-server
 for agent tools, workspace history, operation approvals, and deliverables.
EOF
# dpkg-shlibdeps needs a source control file even when emitting only variables.
install -d "$stage/debian"
printf 'Source: fliuno\n\nPackage: fliuno\nArchitecture: amd64\n' > "$stage/debian/control"
depends=$(cd "$stage" && dpkg-shlibdeps -O -e"$package/usr/bin/fliuno" -e"$package/usr/bin/fliuno-app-server")
case "$depends" in
    shlibs:Depends=*) printf 'Depends: %s\n' "${depends#shlibs:Depends=}" >> "$package/DEBIAN/control" ;;
    *) printf 'Unable to determine native dependencies.\n' >&2; exit 1 ;;
esac

dpkg-deb --root-owner-group --build "$package" "$out/fliuno-v$version-linux-amd64.deb"
