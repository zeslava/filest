#!/bin/sh
set -e

BIN_SRC="target/release/filest"
BIN_DST="/usr/local/bin/filest"
RC_SRC="deploy/filest.rc"
RC_DST="/usr/local/etc/rc.d/filest"
ENV_SRC="deploy/filest.env"
ENV_DST="/usr/local/etc/filest.env"
LOG="/var/log/filest.log"

if [ ! -f "${BIN_SRC}" ]; then
    echo "Binary not found at ${BIN_SRC}"
    echo "Run: cargo build --release"
    exit 1
fi

echo "Installing filest..."

service filest stop 2>/dev/null || true
pkill -f "${BIN_DST}" 2>/dev/null || true
sleep 1

cp "${BIN_SRC}" "${BIN_DST}"
chmod 755 "${BIN_DST}"
echo "  ${BIN_DST}"

cp "${RC_SRC}" "${RC_DST}"
chmod 755 "${RC_DST}"
echo "  ${RC_DST}"

if [ ! -f "${ENV_DST}" ]; then
    cp "${ENV_SRC}" "${ENV_DST}"
    echo "  ${ENV_DST} (new)"
else
    echo "  ${ENV_DST} (kept existing)"
fi

touch "${LOG}"
echo "  ${LOG}"

if ! grep -q 'filest_enable' /etc/rc.conf; then
    echo 'filest_enable="YES"' >> /etc/rc.conf
    echo "  Added to /etc/rc.conf"
fi

echo "Done. Edit ${ENV_DST} then: service filest start"
