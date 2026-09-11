#! /usr/bin/env bash

# Vendored from linuxdeploy-plugin-gtk at commit
# b5eb8d05b4c0ed40107fe2158c5d8527f94568ef (MIT, LICENSE.txt).
# Cutterhoochee compatibility change: gdk-pixbuf2 2.44.7-1 links Glycin
# instead of installing legacy external loader modules.  In that mode this
# helper stages the installed Glycin loaders, configs, MIME data and bwrap
# runtime before asking the existing linuxdeploy binary to close dependencies.

set -e

if [ "${DEBUG:-}" != "" ]; then
    set -x
    verbose="--verbose"
fi

script=$(readlink -f "$0")
notice_source_dir="${CUTTERHOOCHEE_GTK_RUNTIME_NOTICE_DIR:-$(dirname "$script")/gtk-runtime-notices}"

show_usage() {
    echo "Usage: $script --appdir <path to AppDir>"
    echo
    echo "Bundles resources for applications that use GTK into an AppDir"
    echo
    echo "Required variables:"
    echo "  LINUXDEPLOY=path to linuxdeploy (set automatically by linuxdeploy)"
}

get_pkgconf_variable() {
    local variable="$1"
    local library="$2"
    local default_path="$3"
    local path

    path="$($PKG_CONFIG --variable="$variable" "$library")"
    if [ -n "$path" ]; then
        echo "$path"
    elif [ -n "$default_path" ]; then
        echo "$default_path"
    else
        echo "$0: there is no '$variable' variable for '$library' library." >&2
        echo "Please install the appropriate -dev/-devel package." >&2
        exit 1
    fi
}

copy_tree() {
    local src=("${@:1:$#-1}")
    local dst="${*:$#}"

    for elem in "${src[@]}"; do
        mkdir -p "${dst::-1}$elem"
        cp "$elem" --archive --parents --target-directory="$dst" $verbose
    done
}

search_tool() {
    local tool="$1"
    local directory="$2"
    local path

    if command -v "$tool" >/dev/null 2>&1; then
        command -v "$tool"
        return 0
    fi

    for path in \
        "/usr/lib/$(uname -m)-linux-gnu/$directory/$tool" \
        "/usr/lib/$directory/$tool" \
        "/usr/bin/$tool" \
        "/usr/bin/$tool-64" \
        "/usr/bin/$tool-32"; do
        if [ -x "$path" ]; then
            echo "$path"
            return 0
        fi
    done
    return 1
}

require_file() {
    local path="$1"
    local description="$2"
    if [ ! -f "$path" ]; then
        echo "$script: required $description is missing: $path" >&2
        exit 1
    fi
}

require_executable() {
    local path="$1"
    local description="$2"
    if [ ! -x "$path" ]; then
        echo "$script: required $description is missing or not executable: $path" >&2
        exit 1
    fi
}

DEPLOY_GTK_VERSION="${DEPLOY_GTK_VERSION:-3}"
APPDIR=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --plugin-api-version)
            echo "0"
            exit 0
            ;;
        --appdir)
            [ "$#" -ge 2 ] || { show_usage; exit 1; }
            APPDIR="$2"
            shift 2
            ;;
        --help)
            show_usage
            exit 0
            ;;
        *)
            echo "Invalid argument: $1" >&2
            show_usage
            exit 1
            ;;
    esac
done

if [ -z "$APPDIR" ]; then
    show_usage
    exit 1
fi
if [ -z "${LINUXDEPLOY:-}" ]; then
    echo "$script: LINUXDEPLOY environment variable is not set." >&2
    exit 1
fi

mkdir -p "$APPDIR"
chmod +w "$APPDIR/usr/lib64" 2>/dev/null || true

if command -v pkgconf >/dev/null 2>&1; then
    PKG_CONFIG=pkgconf
elif command -v pkg-config >/dev/null 2>&1; then
    PKG_CONFIG=pkg-config
else
    echo "$script: pkg-config/pkgconf not found in PATH, aborting" >&2
    exit 1
fi
command -v find >/dev/null 2>&1 || { echo "$script: find is required" >&2; exit 1; }
command -v readelf >/dev/null 2>&1 || { echo "$script: readelf is required" >&2; exit 1; }

# Keep the upstream helper's GTK/schema/module/cache flow unchanged.
echo "Installing AppRun hook"
HOOKSDIR="$APPDIR/apprun-hooks"
HOOKFILE="$HOOKSDIR/linuxdeploy-plugin-gtk.sh"
mkdir -p "$HOOKSDIR"
cat > "$HOOKFILE" <<'EOF'
#! /usr/bin/env bash

gsettings get org.gnome.desktop.interface gtk-theme 2> /dev/null | grep -qi "dark" && GTK_THEME_VARIANT="dark" || GTK_THEME_VARIANT="light"
APPIMAGE_GTK_THEME="${APPIMAGE_GTK_THEME:-"Adwaita:$GTK_THEME_VARIANT"}"

export APPDIR="${APPDIR:-"$(dirname "$(realpath "$0")")"}"
export GTK_DATA_PREFIX="$APPDIR"
export GTK_THEME="$APPIMAGE_GTK_THEME"
export GDK_BACKEND=x11
export XDG_DATA_DIRS="$APPDIR/usr/share:/usr/share:$XDG_DATA_DIRS"
EOF

glib_schemasdir="$(get_pkgconf_variable schemasdir gio-2.0 /usr/share/glib-2.0/schemas)"
copy_tree "$glib_schemasdir" "$APPDIR/"
glib-compile-schemas "$APPDIR/$glib_schemasdir"
cat >> "$HOOKFILE" <<EOF
export GSETTINGS_SCHEMA_DIR="\$APPDIR/$glib_schemasdir"
EOF

case "$DEPLOY_GTK_VERSION" in
    2)
        echo "WARNING: Gtk+2 applications are not fully supported by this plugin" >&2
        ;;
    3)
        echo "Installing GTK 3.0 modules"
        gtk3_exec_prefix="$(get_pkgconf_variable exec_prefix gtk+-3.0)"
        gtk3_libdir="$(get_pkgconf_variable libdir gtk+-3.0)/gtk-3.0"
        gtk3_immodulesdir="$gtk3_libdir/$(get_pkgconf_variable gtk_binary_version gtk+-3.0)/immodules"
        gtk3_printbackendsdir="$gtk3_libdir/$(get_pkgconf_variable gtk_binary_version gtk+-3.0)/printbackends"
        gtk3_immodules_cache_file="$(dirname "$gtk3_immodulesdir")/immodules.cache"
        gtk3_immodules_query="$(search_tool gtk-query-immodules-3.0 libgtk-3-0 || true)"
        copy_tree "$gtk3_libdir" "$APPDIR/"
        cat >> "$HOOKFILE" <<EOF
export GTK_EXE_PREFIX="\$APPDIR/$gtk3_exec_prefix"
export GTK_PATH="\$APPDIR/$gtk3_libdir:/usr/lib64/gtk-3.0:/usr/lib/x86_64-linux-gnu/gtk-3.0"
export GTK_IM_MODULE_FILE="\$APPDIR/$gtk3_immodules_cache_file"

EOF
        if [ -x "$gtk3_immodules_query" ]; then
            echo "Updating immodules cache in $APPDIR/$gtk3_immodules_cache_file"
            "$gtk3_immodules_query" > "$APPDIR/$gtk3_immodules_cache_file"
        else
            echo "WARNING: gtk-query-immodules-3.0 not found" >&2
        fi
        if [ ! -f "$APPDIR/$gtk3_immodules_cache_file" ]; then
            echo "WARNING: immodules.cache file is missing" >&2
        fi
        sed -i "s|$gtk3_libdir/3.0.0/immodules/||g" "$APPDIR/$gtk3_immodules_cache_file"
        ;;
    4)
        echo "Installing GTK 4.0 modules"
        gtk4_exec_prefix="$(get_pkgconf_variable exec_prefix gtk4 /usr)"
        gtk4_libdir="$(get_pkgconf_variable libdir gtk4 /usr)/gtk-4.0"
        copy_tree "$gtk4_libdir" "$APPDIR/"
        cat >> "$HOOKFILE" <<EOF
export GTK_EXE_PREFIX="\$APPDIR/$gtk4_exec_prefix"
export GTK_PATH="\$APPDIR/$gtk4_libdir/modules"
EOF
        ;;
    *)
        echo "$script: '$DEPLOY_GTK_VERSION' is not a valid GTK major version." >&2
        exit 1
        ;;
esac

echo "Inspecting GDK PixBuf loader mode"
gdk_libdir="$(get_pkgconf_variable libdir gdk-pixbuf-2.0)"
gdk_pixbuf_binarydir="$(get_pkgconf_variable gdk_pixbuf_binarydir gdk-pixbuf-2.0)"
gdk_pixbuf_cache_file="$(get_pkgconf_variable gdk_pixbuf_cache_file gdk-pixbuf-2.0)"
gdk_pixbuf_moduledir="$(get_pkgconf_variable gdk_pixbuf_moduledir gdk-pixbuf-2.0)"
gdk_pixbuf_query="$(search_tool gdk-pixbuf-query-loaders gdk-pixbuf-2.0 || true)"
gdk_pixbuf_library=
for candidate in "$gdk_libdir"/libgdk_pixbuf-2.0.so*; do
    if [ -f "$candidate" ] || [ -L "$candidate" ]; then
        gdk_pixbuf_library="$candidate"
        break
    fi
done
require_file "$gdk_pixbuf_library" "gdk-pixbuf shared library"

glycin_linked=false
if readelf -d "$gdk_pixbuf_library" 2>/dev/null | grep -q 'libglycin-2\.so'; then
    glycin_linked=true
fi

loaders=(glycin-image-rs glycin-svg glycin-heif glycin-jxl)
if [ "$glycin_linked" = true ]; then
    echo "Detected linked Glycin gdk-pixbuf runtime; staging modern loaders"
    glycin_prefix="$(get_pkgconf_variable prefix glycin-2 /usr)"
    glycin_loader_source="$glycin_prefix/lib/glycin-loaders/2+"
    glycin_config_source="$glycin_prefix/share/glycin-loaders/2+/conf.d"
    mime_prefix="$(get_pkgconf_variable prefix shared-mime-info /usr)"
    mime_source="$mime_prefix/share/mime"
    require_file "$notice_source_dir/gtk-runtime-SOURCE.txt" "GTK runtime provenance notice"
    require_file "$notice_source_dir/linuxdeploy-plugin-gtk-LICENSE.txt" "GTK helper license notice"
    require_file "$notice_source_dir/glycin-LICENSE.txt" "Glycin license notice"
    require_file "$notice_source_dir/bubblewrap-LICENSE.txt" "bubblewrap license notice"
    require_file "$notice_source_dir/shared-mime-info-LICENSE.txt" "shared-mime-info license notice"
    require_file "$notice_source_dir/gdk-pixbuf-LICENSE.txt" "gdk-pixbuf license notice"
    require_file "$notice_source_dir/libheif-LICENSE.txt" "libheif license notice"
    require_file "$mime_source/mime.cache" "shared MIME cache"
    mkdir -p \
        "$APPDIR/usr/bin" \
        "$APPDIR/usr/share/glycin-loaders/2+/conf.d" \
        "$APPDIR/usr/lib/Cutterhoochee/resources/notices/gtk-runtime"
    for loader in "${loaders[@]}"; do
        source="$glycin_loader_source/$loader"
        config="$glycin_config_source/$loader.conf"
        require_executable "$source" "Glycin decoder $loader"
        require_file "$config" "Glycin config $loader"
        grep -Eq "^Exec=.*(/|=)${loader}([[:space:]]|$)" "$config" || {
            echo "$script: Glycin config is not configured for decoder $loader: $config" >&2
            exit 1
        }
        install -m 0755 "$source" "$APPDIR/usr/bin/$loader"
        install -m 0644 "$config" "$APPDIR/usr/share/glycin-loaders/2+/conf.d/$loader.conf"
    done
    copy_tree "$mime_source" "$APPDIR/"
    require_file "$APPDIR/usr/share/mime/mime.cache" "staged shared MIME cache"

    bwrap_source="$(type -P bwrap || true)"
    require_executable "$bwrap_source" "bubblewrap"
    install -m 0755 "$bwrap_source" "$APPDIR/usr/bin/cutterhoochee-bwrap"
    install -m 0755 "$(dirname "$script")/bwrap" "$APPDIR/usr/bin/bwrap"
    for notice in \
        gtk-runtime-SOURCE.txt \
        linuxdeploy-plugin-gtk-LICENSE.txt \
        glycin-LICENSE.txt \
        libheif-LICENSE.txt \
        bubblewrap-LICENSE.txt \
        shared-mime-info-LICENSE.txt \
        gdk-pixbuf-LICENSE.txt; do
        install -m 0644 "$notice_source_dir/$notice" \
            "$APPDIR/usr/lib/Cutterhoochee/resources/notices/gtk-runtime/$notice"
    done
else
    echo "Using legacy gdk-pixbuf module/cache runtime"
    copy_tree "$gdk_pixbuf_binarydir" "$APPDIR/"
    cat >> "$HOOKFILE" <<EOF
export GDK_PIXBUF_MODULE_FILE="\$APPDIR/$gdk_pixbuf_cache_file"
EOF
    if [ -x "$gdk_pixbuf_query" ]; then
        echo "Updating loaders cache in $APPDIR/$gdk_pixbuf_cache_file"
        "$gdk_pixbuf_query" > "$APPDIR/$gdk_pixbuf_cache_file"
    else
        echo "WARNING: gdk-pixbuf-query-loaders not found" >&2
    fi
    if [ ! -f "$APPDIR/$gdk_pixbuf_cache_file" ]; then
        echo "WARNING: loaders.cache file is missing" >&2
    fi
    sed -i "s|$gdk_pixbuf_moduledir/||g" "$APPDIR/$gdk_pixbuf_cache_file"
fi

echo "Copying more libraries"
gobject_libdir="$(get_pkgconf_variable libdir gobject-2.0)"
gio_libdir="$(get_pkgconf_variable libdir gio-2.0)"
librsvg_libdir="$(get_pkgconf_variable libdir librsvg-2.0)"
pango_libdir="$(get_pkgconf_variable libdir pango)"
pangocairo_libdir="$(get_pkgconf_variable libdir pangocairo)"
pangoft2_libdir="$(get_pkgconf_variable libdir pangoft2)"
FIND_ARRAY=(
    "$gdk_libdir" "libgdk_pixbuf-*.so*"
    "$gobject_libdir" "libgobject-*.so*"
    "$gio_libdir" "libgio-*.so*"
    "$librsvg_libdir" "librsvg-*.so*"
    "$pango_libdir" "libpango-*.so*"
    "$pangocairo_libdir" "libpangocairo-*.so*"
    "$pangoft2_libdir" "libpangoft2-*.so*"
)
LIBRARIES=()
for (( i=0; i<${#FIND_ARRAY[@]}; i+=2 )); do
    directory=${FIND_ARRAY[i]}
    library=${FIND_ARRAY[i+1]}
    while IFS= read -r -d '' file; do
        LIBRARIES+=( "--library=$file" )
    done < <(find "$directory" \( -type l -o -type f \) -name "$library" -print0)
done

EXECUTABLES=()
if [ "$glycin_linked" = true ]; then
    for loader in "${loaders[@]}"; do
        EXECUTABLES+=( "--executable=$APPDIR/usr/bin/$loader" )
    done
    EXECUTABLES+=( "--executable=$APPDIR/usr/bin/cutterhoochee-bwrap" )
fi
env LINUXDEPLOY_PLUGIN_MODE=1 "$LINUXDEPLOY" --appdir="$APPDIR" "${LIBRARIES[@]}" "${EXECUTABLES[@]}"

if [ "$glycin_linked" = true ]; then
    command -v patchelf >/dev/null 2>&1 || { echo "$script: patchelf is required for Glycin runtime ELFs" >&2; exit 1; }
    for loader in "${loaders[@]}"; do
        patchelf --set-rpath '$ORIGIN/../lib' "$APPDIR/usr/bin/$loader"
    done
    patchelf --set-rpath '$ORIGIN/../lib' "$APPDIR/usr/bin/cutterhoochee-bwrap"
    mkdir -p "$APPDIR/usr/share/cutterhoochee"
    cat > "$APPDIR/usr/share/cutterhoochee/gtk-runtime.json" <<'EOF'
{
  "version": 1,
  "glycinCompatVersion": "2+",
  "loaders": ["glycin-image-rs", "glycin-svg", "glycin-heif", "glycin-jxl"],
  "provenance": {
    "gdkPixbufPackage": "gdk-pixbuf2 2.44.7-1",
    "glycinPackage": "glycin 2.1.5-2",
    "bubblewrapPackage": "bubblewrap 0.11.2-1",
    "sharedMimeInfoPackage": "shared-mime-info 2.5.1-2",
    "gtkHelperCommit": "b5eb8d05b4c0ed40107fe2158c5d8527f94568ef"
  }
}
EOF
fi

# Preserve the upstream GTK/GIO extras and WebKit compatibility steps.
PATCH_ARRAY=(
    "$gtk3_immodulesdir"
    "$gtk3_printbackendsdir"
    "$gdk_pixbuf_moduledir"
)
for directory in "${PATCH_ARRAY[@]}"; do
    while IFS= read -r -d '' file; do
        ln $verbose -s "${file/\/usr\/lib\//}" "$APPDIR/usr/lib" 2>/dev/null || true
    done < <(find "$directory" -name '*.so' -print0 2>/dev/null)
done
chmod +w "$APPDIR/usr/lib64" 2>/dev/null || true
find /usr/lib* -name libgiognutls.so -exec mkdir -p "$APPDIR/$(dirname '{}')" \; -exec cp --parents '{}' "$APPDIR/" \; 2>/dev/null || true
gio_extras_dir=$(find "$APPDIR"/usr/lib* -name libgiognutls.so -exec dirname '{}' \; 2>/dev/null | head -n 1 || true)
if [ -n "$gio_extras_dir" ]; then
    cat >> "$HOOKFILE" <<EOF
export GIO_EXTRA_MODULES="\$APPDIR/${gio_extras_dir#"$APPDIR"/}"
EOF
fi
find "$APPDIR"/usr/lib* -name 'libwebkit*' -exec sed -i -e 's|/usr|././|g' '{}' \; 2>/dev/null || true
