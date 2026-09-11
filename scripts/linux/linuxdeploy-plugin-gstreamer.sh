#!/bin/bash
# The upstream deployment pass rewrites executable RUNPATHs. Finalize the
# proc-free Glycin sandbox paths afterwards, without changing its media setup.
set -euo pipefail

script_dir=$(dirname "$(readlink -f "$0")")
"$script_dir/cutterhoochee-gstreamer-upstream.sh" "$@"

appdir=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --appdir) appdir="$2"; shift 2 ;;
        *) shift ;;
    esac
done

if [ -n "$appdir" ] && [ -f "$appdir/usr/share/cutterhoochee/gtk-runtime.json" ]; then
    for executable in cutterhoochee-bwrap glycin-image-rs glycin-svg glycin-heif glycin-jxl; do
        patchelf --set-rpath '$ORIGIN/../lib:/cutterhoochee/lib' "$appdir/usr/bin/$executable"
    done
fi
