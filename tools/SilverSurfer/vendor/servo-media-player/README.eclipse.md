# Vendored servo-media-player 0.7.0 (Eclipse / SilverSurfer)

Adds Media Source Extensions to the `Player` trait: `StreamType::MediaSource`,
`AppendResult`, `PlayerError::NotSupported` and the `add_source_buffer` /
`append_source_buffer` / ... methods. They have default implementations that
return `NotSupported`, so backends without MSE keep compiling.

Drop this vendor when Servo ships MSE upstream.
