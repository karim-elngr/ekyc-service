#!/bin/sh
set -eu
test -s /portal/index.html || { echo "Mount the built portal at /portal." >&2; exit 1; }
export DISPLAY=:93 NO_AT_BRIDGE=0 GTK_MODULES=gail:atk-bridge XDG_RUNTIME_DIR=/tmp/orca-runtime
mkdir -m 700 "$XDG_RUNTIME_DIR"
Xvfb :93 -screen 0 1280x900x24 >/tmp/display.log 2>&1 &
sleep 1
openbox >/tmp/window.log 2>&1 &
gsettings set org.gnome.desktop.interface toolkit-accessibility true

pulseaudio --start --exit-idle-time=-1 --log-target=file:/tmp/audio.log
pactl load-module module-null-sink sink_name=reader >/dev/null
pactl set-default-sink reader
speech-dispatcher -d
timeout 10 spd-say --wait "Screen reader test."

# A terminal makes Orca line-buffer its speech log while background polls run.
script --quiet --flush --return --command \
    'orca --enable=speech --disable=braille --debug-file=/dev/tty' /tmp/orca.log >/dev/null &
sleep 2
orca --version
chromium --version
exec python3 -u /qa/journey.py
