#!/bin/zsh
# Fightbox workbench launcher — megablock 6x6 synthetic city, autopilot + FP PiP build
cd "$(dirname "$0")"
exec ./target/debug/fightbox-workbench \
  --package "$HOME/fightbox-runs/megablock-seed1/megablock.fightbox" \
  --baked "$HOME/fightbox-runs/megablock-seed1/megablock.baked" \
  --fixture fixtures/city/megablock/fixture.json
