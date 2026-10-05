# ADR 0001: Pin Steam Audio 4.8.1 as the propagation kernel

- Status: accepted
- Date: 2026-07-28

The engine uses Steam Audio **4.8.1** as its initial propagation and binaural-rendering kernel, behind the Rust-owned backend boundary. The authoritative upstream tag is commit `0da1825`.

This pin applies to the SDK version recorded in fixtures, bakes, captures, and package provenance. It does not expose SDK handles through the public engine API or make the kernel permanent. Replacing it requires measured evidence and a dated decision record. A pathing capability is not claimed until a real `iplPathBakerBake` bake is serialized, reloaded in a fresh process, and produces the required S3 evidence.
