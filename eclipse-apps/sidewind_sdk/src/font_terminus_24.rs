pub const FONT_TERMINUS_24: ::embedded_graphics::mono_font::MonoFont = ::embedded_graphics::mono_font::MonoFont {
    image: ::embedded_graphics::image::ImageRaw::new(
        include_bytes!("font_terminus_24.data"),
        192u32,
    ),
    glyph_mapping: &::embedded_graphics::mono_font::mapping::ASCII,
    character_size: ::embedded_graphics::geometry::Size::new(12u32, 24u32),
    character_spacing: 0u32,
    baseline: 18u32,
    underline: ::embedded_graphics::mono_font::DecorationDimensions::new(20u32, 1u32),
    strikethrough: ::embedded_graphics::mono_font::DecorationDimensions::new(12u32, 1u32),
};
