# Vendored servo-media-gstreamer 0.7.0 (Eclipse / SilverSurfer)

- `mse.rs`: Media Source Extensions backend. Each source buffer demuxes the
  appended bytes with `qtdemux` (MP4) or `matroskademux` (WebM) into a sample
  store; `servomse://` (`ServoMseSrc`) plays the store with one TIME appsrc per
  track. Seeks move each track's cursor to the keyframe before the target.
- `player.rs`: wires `StreamType::MediaSource` to it.
- `registry_scanner.rs`: accepts RFC 6381 VP9 codec strings (`vp09.*`), which
  YouTube uses in `isTypeSupported`.

End-to-end test (needs fragmented MP4 and WebM files, see the test):
`MSE_TEST_DIR=dir cargo test --release -p servo-media-gstreamer mse -- --include-ignored`

Drop this vendor when Servo ships MSE upstream.
