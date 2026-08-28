#!/bin/sh
#
# Fail the build if community installers grow a third-party fetch, TLS
# skip, background exec, or a "pipe the git default branch" instruction.
# The knexus injection in issue #46 was three of those at once.
set -eu

root="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
cd "$root"

files="install.sh install.ps1 install-desktop.sh"
missing=0
for f in $files; do
    if [ ! -f "$f" ]; then
        printf 'missing %s\n' "$f" >&2
        missing=1
    fi
done
[ "$missing" -eq 0 ] || exit 1

fail=0
# IOCs + the exact patterns used to hide the payload.
if grep -nEi \
    'buildwithknexus|gaganata|fallgganata|install_guard\.js|curl[[:space:]]+-[A-Za-z]*k|-k[[:space:]]+https|wget.*no-check-certificate|nohup[[:space:]]|raw\.githubusercontent\.com' \
    $files; then
    printf 'installer supply-chain check: forbidden download/exec pattern\n' >&2
    fail=1
fi

# Advertised one-liners must be GitHub Release assets, not a git branch.
if ! grep -q 'releases/latest/download/install.sh' install.sh; then
    printf 'install.sh must advertise releases/latest/download/install.sh\n' >&2
    fail=1
fi
if ! grep -q 'releases/latest/download/install.ps1' install.ps1; then
    printf 'install.ps1 must advertise releases/latest/download/install.ps1\n' >&2
    fail=1
fi

# README must not tell people to pipe the default branch.
if grep -n 'raw.githubusercontent.com/.*/dev/install' README.md README.zh-CN.md; then
    printf 'README must not pipe installers from the git default branch\n' >&2
    fail=1
fi

if [ "$fail" -ne 0 ]; then
    exit 1
fi
printf 'installer supply-chain check: ok\n'
