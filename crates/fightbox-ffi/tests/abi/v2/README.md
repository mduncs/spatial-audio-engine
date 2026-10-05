# Fightbox C ABI V2 contract

This fixture freezes the additive Wave 17 neutral-spatial boundary while the
V1 fixture remains immutable and hash-pinned in the adjacent `v1` directory.

`abi_layout_contract.h` pins every V2 constant, enum value, 64-bit Apple C
layout, field offset, and fixed direct-plane slot. `abi_declaration_contract.h`
pins the five additive create, configure, complete control-frame update,
prepare, and render symbol signatures. `header_contract.c` is a strict C11
compile probe against the canonical generated header.

`adversarial_client.c` is compiled in every run and linked in real-library
mode. It exercises header-first undersized structures, bad version/reserved and
path handling, null output handling, the public neutral construction barrier,
immutable source shapes, wrong-route rejection, nested reserved-field
rejection, exact feed/environment metadata, successful block-clock advancement,
and guard canaries around every callback-owned buffer. The complete control
frame evidence rejects bad outer headers, reserved/count/stride/pointer graphs,
an invalid listener, and an invalid last source before initial publication,
then proves one valid batch admits neutral prepare/render and both V1- and
V2-created legacy sessions. A rejected block leaves all caller buffers and
metadata untouched; Rust unit tests cover the otherwise-unreachable
pointer/stride/overflow/alias validator branches.

After a successful Steam direct-simulation publication, V2 commits matching
RuntimeGraph activity before attempting the scheduled pathing or reflection
pass. A later-pass error is returned but does not roll activity back. The
activity publication carries Steam's shared direct-generation token, which the
render seam compares before DSP or state advancement. An inter-publication race
therefore yields one advancing silent discontinuity, never mixed truth.

The V2 timing publication covers each complete successful callback from outer
block-header/current-prefix validation and copy through finite-input
validation, block assembly, Runtime/Steam rendering, metadata mapping, and
caller-bank scatter. Rejected structural calls are not recorded as completed
timing observations.

One accepted `FbControlFrameV2` is one complete synchronous simulation/cadence
transaction regardless of source count. Neutral publication remains correlated
by the direct-generation token. The legacy route deliberately retains its
frozen independent orientation, activity, and Steam-propagation publications;
batch acceptance does not claim render-atomic legacy snapshot visibility.

From the repository root:

```sh
bash scripts/check-fightbox-ffi-v2-abi.sh
```

To close the exported-symbol and real-session boundary on macOS, build the
archive against the verified SDK and pass its actual dynamic-library directory
to both the linker and loader. In an isolated worktree, derive the ignored SDK
from the canonical checkout rather than assuming the worktree has its own
`.cache`:

```sh
canonical_checkout="$(dirname "$(git rev-parse --git-common-dir)")"
export STEAM_AUDIO_SDK_DIR="$canonical_checkout/.cache/steam-audio/steamaudio-4.8.1/steamaudio"
cargo build -p fightbox-ffi --release
sdk_lib="$STEAM_AUDIO_SDK_DIR/lib/osx"
bash scripts/check-fightbox-ffi-v2-abi.sh -- \
  target/release/libfightbox_ffi.a \
  -L"$sdk_lib" -lphonon -Wl,-rpath,"$sdk_lib"
```

The script runs the immutable V1 gate first in both modes.
