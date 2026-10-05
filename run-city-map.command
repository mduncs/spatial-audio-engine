#!/bin/zsh
# City Map: the Workbench on a city scene plus the live Apple Maps window,
# linked over loopback. User-launched only; never closes another app.
#
#   ./run-city-map.command                 neighborhood scene (bell towers)
#   ./run-city-map.command <scene dir>     any city scene, e.g. the Loop
#                                          (…/loop350-graded-20261002/scene, Willis Tower)
#
# Sources stay off until you press Play (on the map or in the Workbench).
# The volume starts quiet (monitor gain capped at START_GAIN_DB); raise it on
# the map's Volume slider. The output limiter is always on.
set -eu

LANE=/path/to/spatial-audio/engine
WORKBENCH="$LANE/target/release/fightbox-workbench"
CITY_MAP="$LANE/platforms/macos/CityMap/.build/release/CityMap"
SCENE="${1:-/path/to/spatial-audio/evidence/neighborhood-graded-20261002/scene}"
SCENE="${SCENE%/}"
PORT="${CITY_MAP_PORT:-47811}"
START_GAIN_DB="${START_GAIN_DB:-24}"

echo '© OpenStreetMap contributors, ODbL · Apple Maps'
for binary in "$WORKBENCH" "$CITY_MAP"; do
  if [[ ! -x "$binary" ]]; then
    echo "Missing $binary. Build the release workbench and City Map in the engine checkout first."
    exit 1
  fi
done
PACKAGE="$SCENE/city.fightbox"
FIXTURE="$SCENE/workbench.json"
BAKES=("$SCENE"/bakes/*.baked(N))
BAKED="${BAKES[1]:-}"
if [[ ! -d "$PACKAGE" || ! -f "$FIXTURE" || -z "$BAKED" ]]; then
  echo "Not a city scene (needs city.fightbox, workbench.json and bakes/*.baked): $SCENE"
  exit 1
fi
if /usr/bin/pgrep -f '(^|/)fightbox-workbench([[:space:]]|$)' >/dev/null; then
  echo 'A Fightbox Workbench is already running. Close it yourself, then open this launcher again.'
  exit 1
fi
if /usr/bin/nc -z 127.0.0.1 "$PORT" 2>/dev/null; then
  echo "Port $PORT is busy. Set CITY_MAP_PORT to a free port and try again."
  exit 1
fi

echo "Scene: $SCENE"
echo 'Starting the Workbench (sources off, quiet start)…'
# Opens the system-default output; nothing plays until Play.
FIGHTBOX_MAP_LINK="$PORT" FIGHTBOX_START_GAIN_DB="$START_GAIN_DB" \
  "$WORKBENCH" --package "$PACKAGE" --baked "$BAKED" --fixture "$FIXTURE" --start-audio &
WORKBENCH_PID=$!
MAP_PID=""
cleanup() {
  [[ -n "$MAP_PID" ]] && kill "$MAP_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# The map retries on its own, but wait for the link so it opens linked.
for _ in {1..240}; do
  /usr/bin/nc -z 127.0.0.1 "$PORT" 2>/dev/null && break
  kill -0 "$WORKBENCH_PID" 2>/dev/null || { echo 'The Workbench exited during startup.'; exit 1; }
  sleep 0.5
done
echo "Opening City Map on port $PORT. Drag a speaker, tap it to turn it on or off, pick a spot on the right."
"$CITY_MAP" --link "$PORT" &
MAP_PID=$!
wait "$WORKBENCH_PID"
