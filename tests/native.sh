#!/usr/bin/env bash
# Exercise the public DSO with deterministic hardware-boundary fixtures.
set -euo pipefail
library=$(realpath "$1")
directory=$2
mkdir -p "$directory"
library_directory=$(dirname "$library")
test -f "$library"
sha256sum "$library"
"${CC:-cc}" -std=c11 -Wall -Wextra -Werror -Iinclude tests/descriptor_smoke.c \
    -L"$library_directory" -lrptadv_gpio_adapter \
    -Wl,-rpath,"$library_directory" -o "$directory/descriptor-smoke"
"$directory/descriptor-smoke"
"${CC:-cc}" -Wall -Wextra -Werror -shared -fPIC tests/modem_ioctl.c \
    -o "$directory/test-modem-ioctl.so" -ldl
shim=$(realpath "$directory/test-modem-ioctl.so")
LD_PRELOAD="$shim" "${PYTHON:-python3}" tests/serial_tty.py "$library"
for mode in persistent-get failed-startup; do
    LD_PRELOAD="$shim" "${PYTHON:-python3}" tests/serial_tty.py "$library" "$mode"
done
LD_PRELOAD="$shim" "${PYTHON:-python3}" tests/keyboard_input.py "$library"
LD_PRELOAD="$shim" "${PYTHON:-python3}" tests/keyboard_failures.py "$library"
LD_PRELOAD="$shim" "${PYTHON:-python3}" tests/serial_failures.py "$library"
"${CC:-cc}" -Wall -Wextra -Werror -shared -fPIC tests/usb_fixture.c -o "$directory/test-usb.so"
"${CC:-cc}" -D_POSIX_C_SOURCE=200809L -std=c11 -Wall -Wextra -Werror -Iinclude \
    tests/device_smoke.c -L"$library_directory" -lrptadv_gpio_adapter \
    -Wl,-rpath,"$library_directory" -ldl -o "$directory/device-smoke"
LD_PRELOAD="$(realpath "$directory/test-usb.so"):$shim" "$directory/device-smoke"

