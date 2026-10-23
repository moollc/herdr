#!/bin/sh
# managed by herdr; add custom hooks beside this file.
# HERDR_INTEGRATION_ID=cursor
# HERDR_INTEGRATION_VERSION=1
command -v python3 >/dev/null 2>&1 || exit 0
exec python3 "$(dirname "$0")/herdr-turn-report.py" cursor "${1:-}"
