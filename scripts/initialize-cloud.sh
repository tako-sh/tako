#!/bin/sh
set -eu

# Run as the agent user during cloud initialization.
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
sh "$script_dir/install-tako.sh"
"${TAKO_INSTALL_DIR:-$HOME/.local/bin}/tako" --version
