#!/bin/zsh
# Fightbox workbench launcher — compiled Printers Row Chicago block
cd "$(dirname "$0")"
exec ./target/release/fightbox-workbench \
  --package "$HOME/fightbox-runs/2026-07-29/chicago-block-a.fightbox" \
  --baked "$HOME/fightbox-runs/2026-07-29/chicago-block-baked" \
  --fixture fixtures/city/chicago-block/workbench-fixture.json
