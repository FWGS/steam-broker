#!/bin/sh
# Install steam-broker binary and user systemd unit into a local prefix.
#
# Usage:
#   contrib/install.sh                 # installs into ~/.local
#   PREFIX=$HOME/opt contrib/install.sh
#   contrib/install.sh --uninstall

set -eu

PREFIX="${PREFIX:-$HOME/.local}"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
UNIT_NAME="steam-broker.service"

SCRIPT_DIR=$( CDPATH= cd -- "$( dirname -- "$0" )" && pwd )
PROJECT_DIR=$( CDPATH= cd -- "$SCRIPT_DIR/.." && pwd )

LIB_DIR="$PREFIX/lib/steam-broker"

case "${1:-}" in
	--uninstall)
		systemctl --user disable --now "$UNIT_NAME" 2>/dev/null || true
		rm -vf "$UNIT_DIR/$UNIT_NAME"
		rm -vf "$PREFIX/bin/steam-broker"
		rm -vf "$LIB_DIR/libsteam_api.so"
		systemctl --user daemon-reload
		echo "Uninstalled steam-broker."
		exit 0
		;;
	"" ) ;;
	*)
		echo "Unknown argument: $1" >&2
		exit 1
		;;
esac

if [ -f "$PROJECT_DIR/Cargo.toml" ]; then
	echo "Building release binary..."
	cargo build --release --manifest-path "$PROJECT_DIR/Cargo.toml"

	BIN_SRC="$PROJECT_DIR/target/release/steam-broker"
	LIB_SRC=$( ls "$PROJECT_DIR"/target/release/build/steamworks-sys-*/out/libsteam_api.so 2>/dev/null | head -n1 )
	if [ -z "$LIB_SRC" ] || [ ! -f "$LIB_SRC" ]; then
		echo "error: libsteam_api.so not found under target/release/build/steamworks-sys-*/out/" >&2
		exit 1
	fi
else
	# Bundled archive: binary and libsteam_api.so sit next to this script.
	BIN_SRC="$SCRIPT_DIR/steam-broker"
	LIB_SRC="$SCRIPT_DIR/libsteam_api.so"
	for f in "$BIN_SRC" "$LIB_SRC" "$SCRIPT_DIR/$UNIT_NAME"; do
		if [ ! -f "$f" ]; then
			echo "error: $f not found (expected bundled archive layout)" >&2
			exit 1
		fi
	done
fi

echo "Installing binary into $PREFIX/bin..."
install -v -Dm755 "$BIN_SRC" "$PREFIX/bin/steam-broker"

echo "Installing libsteam_api.so into $LIB_DIR..."
install -v -Dm755 "$LIB_SRC" "$LIB_DIR/libsteam_api.so"

echo "Installing user unit into $UNIT_DIR..."
mkdir -pv "$UNIT_DIR"
sed "s|^ExecStart=[^ ]*/steam-broker|ExecStart=$PREFIX/bin/steam-broker|" "$SCRIPT_DIR/$UNIT_NAME" > "$UNIT_DIR/$UNIT_NAME"
chmod 0644 "$UNIT_DIR/$UNIT_NAME"

systemctl --user daemon-reload

cat <<EOF
Installed:
  $PREFIX/bin/steam-broker
  $LIB_DIR/libsteam_api.so
  $UNIT_DIR/$UNIT_NAME

Enable and start with:
  systemctl --user enable --now $UNIT_NAME
EOF
