#!/bin/sh
# The `build_script_override` of `.github/workflows/release.yml` (spec §7 C32).
#
# The `release` job has already built the four binaries and renamed them to
# the release asset names under `prepared/`. This script only copies them
# into `dist/`, which `gh-extension-precompile` then attaches to the release.
# A tag with a prerelease suffix, for example `v0.1.0-rc1`, makes the release
# a prerelease. The action passes the tag as the first argument; the names
# already carry the version, so the script does not need it.
set -eu
mkdir -p dist
cp prepared/gh-klon_v* dist/
ls -l dist
