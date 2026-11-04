# Vendored servo-script 0.7.0 (Eclipse / SilverSurfer)

Upstream crates.io `servo-script` 0.7.0 with a local fix in
`dom/webgl/webglbuffer.rs` (and a small VAO teardown harden):

During ScriptThread / JS GC finalize on window close, WebGLBuffer objects can
still look "attached" to a VAO. Upstream `delete()` asserts `is_deleted()` and
panics inside a finalizer; mozjs then hits `panic_cannot_unwind` on the next
`WebGLVertexArrayObject` finalize.

Drop this vendor when Servo softens that assert upstream.

Media Source Extensions (`dom/media/mediasource.rs`, `sourcebuffer.rs`,
`sourcebufferlist.rs`, hooks in `htmlmediaelement.rs` and `url/url.rs`): an
object URL of a MediaSource attaches it to the media element instead of
fetching, and the player is created with `StreamType::MediaSource`. See
vendor/servo-media-gstreamer/README.eclipse.md.
