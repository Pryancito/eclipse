pub const FONT_TERMINUS_16: ::embedded_graphics::mono_font::MonoFont = ::embedded_graphics::mono_font::MonoFont {
    image: ::embedded_graphics::image::ImageRaw::new(
        include_bytes!("font_terminus_16.data"),
        128u32,
    ),
    glyph_mapping: &::embedded_graphics::mono_font::mapping::ASCII,
    character_size: ::embedded_graphics::geometry::Size::new(8u32, 16u32),
    character_spacing: 0u32,
    baseline: 11u32,
    underline: ::embedded_graphics::mono_font::DecorationDimensions::new(13u32, 1u32),
    strikethrough: ::embedded_graphics::mono_font::DecorationDimensions::new(8u32, 1u32),
};
