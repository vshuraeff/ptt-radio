#!/bin/sh
set -eu

fail() { printf 'install: %s\n' "$*" >&2; exit 1; }
usage() { printf '%s\n' 'usage: sh install.sh [--factorio-dir DIR] [--mods-dir DIR] [--version vVERSION]'; }

main() {
    factorio_dir=
    mods_dir=
    tag=
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --factorio-dir|--mods-dir|--version)
                [ "$#" -ge 2 ] && [ -n "$2" ] || fail "missing value for $1"
                case "$2" in -*) fail "invalid value for $1: $2" ;; esac
                case "$1" in
                    --factorio-dir) factorio_dir=$2 ;;
                    --mods-dir) mods_dir=$2 ;;
                    --version) tag=$2 ;;
                esac
                shift 2 ;;
            --help|-h) usage; exit 0 ;;
            *) usage >&2; fail "unknown argument: $1" ;;
        esac
    done

    case "$(uname -s):$(uname -m)" in
        Darwin:arm64|Darwin:x86_64) platform=macos-universal; extension=zip
            default_dir=$HOME/Library/Application\ Support/factorio ;;
        Linux:x86_64|Linux:amd64) platform=linux-x86_64; extension=tar.gz
            default_dir=$HOME/.factorio ;;
        *) fail 'no companion release archive is published for this OS/architecture' ;;
    esac
    for tool in curl awk mktemp od sed; do command -v "$tool" >/dev/null 2>&1 || fail "required tool missing: $tool"; done
    if command -v sha256sum >/dev/null 2>&1; then hasher=sha256sum
    elif command -v shasum >/dev/null 2>&1; then hasher=shasum
    else fail 'sha256sum or shasum is required'; fi

    releases=https://github.com/vshuraeff/ptt-radio/releases
    protocols='=https'
    if [ -n "${PTT_RADIO_INSTALL_TEST_RELEASE_URL:-}" ]; then
        releases=${PTT_RADIO_INSTALL_TEST_RELEASE_URL%/}
        protocols='=https,http,file'
    fi
    if [ -z "$tag" ]; then
        resolved=$(curl --fail --silent --show-error --location --proto "$protocols" --proto-redir "$protocols" \
            --connect-timeout 20 --speed-limit 1024 --speed-time 60 --output /dev/null --write-out '%{url_effective}' "$releases/latest")
        case "$resolved" in "$releases/tag/"*) tag=${resolved##*/} ;; *) fail 'could not resolve the latest release tag' ;; esac
    fi
    case "$tag" in v[0-9]*) ;; *) fail 'version must be a tag such as v0.4.0' ;; esac
    case "$tag" in *[!A-Za-z0-9._-]*) fail 'invalid release tag' ;; esac
    asset=ptt-radio-companion-${tag#v}-$platform.$extension

    factorio_dir=${factorio_dir:-$default_dir}
    case "$factorio_dir" in /*) ;; *) factorio_dir=$PWD/$factorio_dir ;; esac
    mods_dir=${mods_dir:-$factorio_dir/mods}
    case "$mods_dir" in /*) ;; *) mods_dir=$PWD/$mods_dir ;; esac
    mkdir -p "$factorio_dir"
    factorio_dir=$(CDPATH='' cd -- "$factorio_dir" && pwd -P)
    install_dir=$factorio_dir/ptt-radio-companion
    [ ! -L "$install_dir" ] || fail 'the install directory must not be a symbolic link'
    [ ! -e "$install_dir" ] || [ -d "$install_dir" ] || fail 'the install path is not a directory'
    stage=$(mktemp -d "$factorio_dir/.ptt-radio-install.XXXXXX")
    backup=
    cleanup() {
        # the backup path is set before the move, so an interrupt between the move and the
        # assignment cannot lose it; a move that never happened leaves no backup to restore
        if [ -n "$backup" ] && [ -e "$backup" ]; then
            if [ ! -e "$install_dir" ] && mv "$backup" "$install_dir"; then
                backup=
            else
                printf 'install: could not restore installation; backup kept at %s\n' "$backup" >&2
                return
            fi
        fi
        rm -rf "$stage"
    }
    trap cleanup 0
    trap 'exit 1' HUP INT TERM

    download() {
        curl --fail --silent --show-error --location --proto "$protocols" --proto-redir "$protocols" \
            --connect-timeout 20 --speed-limit 1024 --speed-time 60 --output "$2" "$1"
    }
    download "$releases/download/$tag/$asset" "$stage/$asset"
    download "$releases/download/$tag/SHA256SUMS" "$stage/SHA256SUMS"
    expected=$(awk -v name="$asset" '$2 == name { n++; hash=$1 } END { if (n != 1 || length(hash) != 64 || hash ~ /[^0-9a-fA-F]/) exit 1; print tolower(hash) }' "$stage/SHA256SUMS") || fail 'missing or invalid checksum'
    if [ "$hasher" = sha256sum ]; then actual=$(sha256sum < "$stage/$asset")
    else actual=$(shasum -a 256 < "$stage/$asset"); fi
    actual=${actual%% *}
    [ "$actual" = "$expected" ] || fail 'checksum mismatch; existing installation was kept'

    mkdir "$stage/new"
    if [ "$extension" = zip ]; then
        command -v unzip >/dev/null 2>&1 || fail 'unzip is required'
        unzip -Z1 "$stage/$asset" > "$stage/entries"
    else
        command -v tar >/dev/null 2>&1 || fail 'tar is required'
        tar -tzf "$stage/$asset" > "$stage/entries"
    fi
    awk -v extension="$extension" 'extension == "zip" && ($0 ~ /^\._/ || $0 ~ /^__MACOSX\//) { next }
        ($0 != "ptt-radio-companion" && $0 != "LICENSE" && $0 != "README.md") || seen[$0]++ { exit 1 }
        END { if (!seen["ptt-radio-companion"] || !seen["LICENSE"] || !seen["README.md"]) exit 1 }' "$stage/entries" || fail 'unexpected archive layout'
    for file in ptt-radio-companion LICENSE README.md; do
        if [ "$extension" = zip ]; then unzip -p "$stage/$asset" "$file" > "$stage/new/$file"
        else tar -xOzf "$stage/$asset" "$file" > "$stage/new/$file"; fi
    done
    [ -s "$stage/new/ptt-radio-companion" ] || fail 'empty companion binary'
    chmod 755 "$stage/new/ptt-radio-companion"
    printf '{"version":1,"mods_dir":' > "$stage/new/install.json"
    printf '%s' "$mods_dir" | od -An -v -tu1 | LC_ALL=C awk 'BEGIN { printf "\"" }
        { for(i=1;i<=NF;i++) { n=$i; if(n==92 || n==34) printf "\\%c",n; else if(n<32) printf "\\u%04x",n; else printf "%c",n } }
        END { printf "\"" }' >> "$stage/new/install.json"
    printf ',"installed_by":"ptt-radio installer"}\n' >> "$stage/new/install.json"
    if [ -d "$install_dir" ]; then
        backup=$stage/previous
        mv "$install_dir" "$backup"
    fi
    mv "$stage/new" "$install_dir"
    backup=
    printf 'Installed: %s\n' "$install_dir"
    quoted=$(printf '%s' "$install_dir/ptt-radio-companion" | sed "s/'/'\\\\''/g")
    printf "Start: '%s'\n" "$quoted"
    steam_quoted=$(printf '%s' "$install_dir/ptt-radio-companion" | sed 's/[\\`"$]/\\&/g')
    printf 'Steam launch options: "%s" --launch %%command%%\n' "$steam_quoted"
}

main "$@"
