pub const FONT_TERMINUS_18: ::embedded_graphics::mono_font::MonoFont = ::embedded_graphics::mono_font::MonoFont {
    image: ::embedded_graphics::image::ImageRaw::new(
        include_bytes!("font_terminus_18.data"),
        160u32,
    ),
    glyph_mapping: &::embedded_graphics::mono_font::mapping::ASCII,
    character_size: ::embedded_graphics::geometry::Size::new(10u32, 18u32),
    character_spacing: 0u32,
    baseline: 14u32,
    underline: ::embedded_graphics::mono_font::DecorationDimensions::new(16u32, 1u32),
    strikethrough: ::embedded_graphics::mono_font::DecorationDimensions::new(9u32, 1u32),
};
