#!/bin/bash
set -euo pipefail
root=$(cd -P "$(dirname "${BASH_SOURCE[0]}")" && pwd)
destination="$HOME/.local/bin"
force=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin-dir)
      [[ $# -ge 2 ]] || { printf '%s\n' 'install.sh: --bin-dir requires an absolute directory' >&2; exit 2; }
      destination=$2
      shift 2
      ;;
    --force)
      force=1
      shift
      ;;
    -h|--help)
      printf '%s\n' 'Usage: ./install.sh [--bin-dir ABSOLUTE_DIRECTORY] [--force]'
      printf '%s\n' 'Existing regular files are preserved unless --force is explicit.'
      exit 0
      ;;
    *)
      printf 'install.sh: unknown option: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

[[ $EUID -ne 0 ]] || { printf '%s\n' 'install.sh: refusing to run as root' >&2; exit 1; }
[[ "$destination" = /* && "$destination" != / && "$destination" != "$HOME" ]] || {
  printf 'install.sh: unsafe destination: %s\n' "$destination" >&2
  exit 2
}

mkdir -p "$destination"
destination=$(cd -P "$destination" && pwd)
chmod +x "$root/bin/workstats" "$root/bin/gitstats"

cargo_path=$(type -P cargo || true)
[[ -n "$cargo_path" ]] || { printf '%s\n' 'install.sh: cargo not found; install Rust from https://rustup.rs' >&2; exit 1; }
(cd "$root" && "$cargo_path" build --release --locked)
native_binary="$root/target/release/workstats"
[[ -x "$native_binary" ]] || { printf '%s\n' 'install.sh: Rust build did not produce workstats' >&2; exit 1; }

install_file() {
  local source=$1 name=$2 target backup temporary suffix
  target="$destination/$name"
  if [[ -e "$target" || -L "$target" ]]; then
    if [[ $force -ne 1 ]]; then
      printf 'install.sh: refusing to replace %s; rerun with --force to preserve it as .before-workstats\n' "$target" >&2
      exit 1
    fi
    backup="$target.before-workstats"
    suffix=1
    while [[ -e "$backup" || -L "$backup" ]]; do
      backup="$target.before-workstats.$suffix"
      suffix=$((suffix + 1))
    done
    mv "$target" "$backup"
  fi
  temporary="$destination/.${name}.workstats-install.$$"
  cp "$source" "$temporary"
  chmod 0755 "$temporary"
  mv "$temporary" "$target"
}

install_file "$native_binary" workstats
install_file "$native_binary" gitstats
printf 'Installed workstats and gitstats in %s\n' "$destination"
