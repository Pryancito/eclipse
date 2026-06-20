#!/usr/bin/env bash
# pkg-config wrapper: always resolve static libraries for TinyX cross-build.
set -euo pipefail
exec pkg-config --static "$@"
