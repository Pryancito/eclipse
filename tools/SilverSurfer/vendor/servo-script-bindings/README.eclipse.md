# Vendored servo-script-bindings 0.7.0 (Eclipse / SilverSurfer)

Adds the Media Source Extensions WebIDL (`MediaSource`, `SourceBuffer`,
`SourceBufferList`), `URL.createObjectURL((Blob or MediaSource))` and their
entries in `codegen/Bindings.conf`. The DOM side lives in the vendored
servo-script (`dom/media/mediasource.rs` and friends).

Drop this vendor when Servo ships MSE upstream.
