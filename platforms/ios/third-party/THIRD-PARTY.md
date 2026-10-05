# iOS third-party archives

Steam Audio's iOS `libphonon.a` leaves PFFFT and libmysofa as host-supplied
static dependencies. The checked-in archives under `lib/ios/` are arm64
iPhoneOS slices with a minimum deployment target of iOS 15.0. They contain no
simulator slice.

`build-ios.sh` is the reproducible source of truth. It pins:

- PFFFT to `marton78/pffft` commit
  `e0bf595c98ded55cc457a371c1b29c8cab552628`. This is the exact PFFFT revision
  selected by Steam Audio v4.8.1 in `core/build/dependencies.json`. Its source
  carries the Julien Pommier/FFTPACK and UCAR redistribution terms.
- libmysofa to stable release `v1.3.4`, commit
  `7a0c07111a3d7230ec534e08925c24a7525f33c0`, under BSD-3-Clause.

Both are built from source with Apple clang and the installed iPhoneOS SDK.
libmysofa finds the SDK's zlib headers and link stub; zlib is not downloaded or
built. The app links `libz.tbd`.

Run from the repository root:

```sh
platforms/ios/third-party/build-ios.sh
```

Set `FIGHTBOX_IOS_THIRD_PARTY_BUILD_ROOT` to retain sources and intermediates at
a chosen location. By default the script uses a fixed directory under
`TMPDIR`, outside the repository. `PFFFT_SOURCE_DIR` and
`LIBMYSOFA_SOURCE_DIR` may point at already-fetched source trees for an offline
rebuild.

The archives currently checked in were built by `build-ios.sh` from the exact
pinned revisions above. Their SHA-256 digests are:

```text
1a5a6ce99162101e3d311ce0e81a801e20d116fefc94ce313082825ef00b36a7  libpffft.a
1070ad69d324eddb4587602eeb61f36b926304e122ffba96481fa973a64e4d8a  libmysofa.a
```

Both pinned upstreams declare a pre-3.5 `cmake_minimum_required`, which
current CMake refuses to configure; the script passes
`-DCMAKE_POLICY_VERSION_MINIMUM=3.5` to both configures for compatibility.
