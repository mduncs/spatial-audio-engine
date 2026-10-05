#!/bin/bash
set -u

repo_dir="$(cd "$(dirname "$0")" && pwd)"
echo "AirPods head-tracking probe: wear your AirPods and allow motion access."
echo "A voice in your current output says when to move: face the screen, turn LEFT, face the screen, look UP."
echo "Each turn gets 5 seconds; the farthest point counts, so timing is forgiving."
echo "This takes up to 36 seconds. It speaks with macOS 'say' and opens no audio input."
if app_dir="$("$repo_dir/platforms/macos/HeadTracker/build.sh")"; then
    "$app_dir/Contents/MacOS/HeadTracker" --probe
    result=$?
    if (( result > 1 )); then
        echo "Head tracking failed: helper exited with status $result (see errors above)."
    fi
else
    echo "Head tracking failed: helper build or ad-hoc signing failed (see errors above)."
fi
echo ""
read -r -n 1 -p "Press any key to close."
echo ""
