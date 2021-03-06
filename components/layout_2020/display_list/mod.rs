/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use crate::context::LayoutContext;
use crate::display_list::conversions::ToWebRender;
use crate::fragments::{BoxFragment, Fragment};
use crate::geom::{PhysicalPoint, PhysicalRect};
use crate::replaced::IntrinsicSizes;
use embedder_traits::Cursor;
use euclid::{Point2D, SideOffsets2D, Size2D};
use gfx::text::glyph::GlyphStore;
use mitochondria::OnceCell;
use net_traits::image_cache::UsePlaceholder;
use std::sync::Arc;
use style::dom::OpaqueNode;
use style::properties::ComputedValues;

use style::values::computed::{BorderStyle, Length, LengthPercentage};
use style::values::specified::ui::CursorKind;
use webrender_api::{self as wr, units};

mod background;
mod conversions;
mod gradient;
pub mod stacking_context;

#[derive(Clone, Copy)]
pub struct WebRenderImageInfo {
    pub width: u32,
    pub height: u32,
    pub key: Option<wr::ImageKey>,
}

// `webrender_api::display_item::ItemTag` is private
type ItemTag = (u64, u16);
type HitInfo = Option<ItemTag>;

pub struct DisplayListBuilder<'a> {
    /// The current SpatialId and ClipId information for this `DisplayListBuilder`.
    current_space_and_clip: wr::SpaceAndClipInfo,

    /// The id of the nearest ancestor reference frame for this `DisplayListBuilder`.
    nearest_reference_frame: wr::SpatialId,

    pub context: &'a LayoutContext<'a>,
    pub wr: wr::DisplayListBuilder,

    /// Contentful paint, for the purpose of
    /// https://w3c.github.io/paint-timing/#first-contentful-paint
    /// (i.e. the display list contains items of type text,
    /// image, non-white canvas or SVG). Used by metrics.
    pub is_contentful: bool,
}

impl<'a> DisplayListBuilder<'a> {
    pub fn new(
        pipeline_id: wr::PipelineId,
        context: &'a LayoutContext,
        viewport_size: wr::units::LayoutSize,
    ) -> Self {
        Self {
            current_space_and_clip: wr::SpaceAndClipInfo::root_scroll(pipeline_id),
            nearest_reference_frame: wr::SpatialId::root_reference_frame(pipeline_id),
            is_contentful: false,
            context,
            wr: wr::DisplayListBuilder::new(pipeline_id, viewport_size),
        }
    }

    fn common_properties(&self, clip_rect: units::LayoutRect) -> wr::CommonItemProperties {
        // TODO(gw): Make use of the WR backface visibility functionality.
        wr::CommonItemProperties::new(clip_rect, self.current_space_and_clip)
    }

    fn clipping_and_scrolling_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let previous_space_and_clip = self.current_space_and_clip;
        let previous_nearest_reference_frame = self.nearest_reference_frame;

        let result = f(self);

        self.current_space_and_clip = previous_space_and_clip;
        self.nearest_reference_frame = previous_nearest_reference_frame;

        result
    }
}

impl Fragment {
    pub(crate) fn build_display_list(
        &self,
        builder: &mut DisplayListBuilder,
        containing_block: &PhysicalRect<Length>,
    ) {
        match self {
            Fragment::Box(b) => BuilderForBoxFragment::new(b, containing_block).build(builder),
            Fragment::Anonymous(_) => {},
            Fragment::Text(t) => {
                builder.is_contentful = true;
                let rect = t
                    .rect
                    .to_physical(t.parent_style.writing_mode, containing_block)
                    .translate(containing_block.origin.to_vector());
                let mut baseline_origin = rect.origin.clone();
                baseline_origin.y += t.ascent;
                let glyphs = glyphs(&t.glyphs, baseline_origin);
                if glyphs.is_empty() {
                    return;
                }
                let mut common = builder.common_properties(rect.clone().to_webrender());
                common.hit_info = hit_info(&t.parent_style, t.tag, Cursor::Text);
                let color = t.parent_style.clone_color();
                builder.wr.push_text(
                    &common,
                    rect.to_webrender(),
                    &glyphs,
                    t.font_key,
                    rgba(color),
                    None,
                );
            },
            Fragment::Image(i) => {
                builder.is_contentful = true;
                let rect = i
                    .rect
                    .to_physical(i.style.writing_mode, containing_block)
                    .translate(containing_block.origin.to_vector());
                let common = builder.common_properties(rect.clone().to_webrender());
                builder.wr.push_image(
                    &common,
                    rect.to_webrender(),
                    image_rendering(i.style.get_inherited_box().image_rendering),
                    wr::AlphaType::PremultipliedAlpha,
                    i.image_key,
                    wr::ColorF::WHITE,
                );
            },
        }
    }
}

struct BuilderForBoxFragment<'a> {
    fragment: &'a BoxFragment,
    containing_block: &'a PhysicalRect<Length>,
    border_rect: units::LayoutRect,
    padding_rect: OnceCell<units::LayoutRect>,
    content_rect: OnceCell<units::LayoutRect>,
    border_radius: wr::BorderRadius,
    border_edge_clip_id: OnceCell<Option<wr::ClipId>>,
    padding_edge_clip_id: OnceCell<Option<wr::ClipId>>,
    content_edge_clip_id: OnceCell<Option<wr::ClipId>>,
}

impl<'a> BuilderForBoxFragment<'a> {
    fn new(fragment: &'a BoxFragment, containing_block: &'a PhysicalRect<Length>) -> Self {
        let border_rect: units::LayoutRect = fragment
            .border_rect()
            .to_physical(fragment.style.writing_mode, containing_block)
            .translate(containing_block.origin.to_vector())
            .to_webrender();

        let border_radius = {
            let resolve = |radius: &LengthPercentage, box_size: f32| {
                radius.percentage_relative_to(Length::new(box_size)).px()
            };
            let corner = |corner: &style::values::computed::BorderCornerRadius| {
                Size2D::new(
                    resolve(&corner.0.width.0, border_rect.size.width),
                    resolve(&corner.0.height.0, border_rect.size.height),
                )
            };
            let b = fragment.style.get_border();
            wr::BorderRadius {
                top_left: corner(&b.border_top_left_radius),
                top_right: corner(&b.border_top_right_radius),
                bottom_right: corner(&b.border_bottom_right_radius),
                bottom_left: corner(&b.border_bottom_left_radius),
            }
        };

        Self {
            fragment,
            containing_block,
            border_rect,
            border_radius,
            padding_rect: OnceCell::new(),
            content_rect: OnceCell::new(),
            border_edge_clip_id: OnceCell::new(),
            padding_edge_clip_id: OnceCell::new(),
            content_edge_clip_id: OnceCell::new(),
        }
    }

    fn content_rect(&self) -> &units::LayoutRect {
        self.content_rect.init_once(|| {
            self.fragment
                .content_rect
                .to_physical(self.fragment.style.writing_mode, self.containing_block)
                .translate(self.containing_block.origin.to_vector())
                .to_webrender()
        })
    }

    fn padding_rect(&self) -> &units::LayoutRect {
        self.padding_rect.init_once(|| {
            self.fragment
                .padding_rect()
                .to_physical(self.fragment.style.writing_mode, self.containing_block)
                .translate(self.containing_block.origin.to_vector())
                .to_webrender()
        })
    }

    fn border_edge_clip(&self, builder: &mut DisplayListBuilder) -> Option<wr::ClipId> {
        *self
            .border_edge_clip_id
            .init_once(|| clip_for_radii(self.border_radius, self.border_rect, builder))
    }

    fn padding_edge_clip(&self, builder: &mut DisplayListBuilder) -> Option<wr::ClipId> {
        *self.padding_edge_clip_id.init_once(|| {
            clip_for_radii(
                inner_radii(
                    self.border_radius,
                    self.fragment
                        .border
                        .to_physical(self.fragment.style.writing_mode)
                        .to_webrender(),
                ),
                self.border_rect,
                builder,
            )
        })
    }

    fn content_edge_clip(&self, builder: &mut DisplayListBuilder) -> Option<wr::ClipId> {
        *self.content_edge_clip_id.init_once(|| {
            clip_for_radii(
                inner_radii(
                    self.border_radius,
                    (&self.fragment.border + &self.fragment.padding)
                        .to_physical(self.fragment.style.writing_mode)
                        .to_webrender(),
                ),
                self.border_rect,
                builder,
            )
        })
    }

    fn build(&mut self, builder: &mut DisplayListBuilder) {
        self.build_hit_test(builder);
        self.build_background(builder);
        self.build_border(builder);
    }

    fn build_hit_test(&self, builder: &mut DisplayListBuilder) {
        let hit_info = hit_info(&self.fragment.style, self.fragment.tag, Cursor::Default);
        if hit_info.is_some() {
            let mut common = builder.common_properties(self.border_rect);
            common.hit_info = hit_info;
            if let Some(clip_id) = self.border_edge_clip(builder) {
                common.clip_id = clip_id
            }
            builder.wr.push_hit_test(&common)
        }
    }

    fn build_background(&mut self, builder: &mut DisplayListBuilder) {
        use style::values::computed::image::{Image, ImageLayer};
        let b = self.fragment.style.get_background();
        let background_color = self.fragment.style.resolve_color(b.background_color);
        if background_color.alpha > 0 {
            // https://drafts.csswg.org/css-backgrounds/#background-color
            // “The background color is clipped according to the background-clip
            //  value associated with the bottom-most background image layer.”
            let layer_index = b.background_image.0.len() - 1;
            let (_, common) = background::painting_area(self, builder, layer_index);
            builder.wr.push_rect(&common, rgba(background_color))
        }
        // Reverse because the property is top layer first, we want to paint bottom layer first.
        for (index, layer) in b.background_image.0.iter().enumerate().rev() {
            match layer {
                ImageLayer::None => {},
                ImageLayer::Image(image) => match image {
                    Image::Gradient(gradient) => {
                        let intrinsic = IntrinsicSizes {
                            width: None,
                            height: None,
                            ratio: None,
                        };
                        if let Some(layer) =
                            &background::layout_layer(self, builder, index, intrinsic)
                        {
                            gradient::build(&self.fragment.style, gradient, layer, builder)
                        }
                    },
                    Image::Url(image_url) => {
                        // FIXME: images won’t always have in intrinsic width or height
                        // when support for SVG is added.
                        // Or a WebRender `ImageKey`, for that matter.
                        let (width, height, key) = match image_url.url() {
                            Some(url) => {
                                match builder.context.get_webrender_image_for_url(
                                    self.fragment.tag,
                                    url.clone(),
                                    UsePlaceholder::No,
                                ) {
                                    Some(WebRenderImageInfo {
                                        width,
                                        height,
                                        key: Some(key),
                                    }) => (width, height, key),
                                    _ => continue,
                                }
                            },
                            None => continue,
                        };

                        // FIXME: https://drafts.csswg.org/css-images-4/#the-image-resolution
                        let dppx = 1.0;

                        let intrinsic = IntrinsicSizes {
                            width: Some(Length::new(width as f32 / dppx)),
                            height: Some(Length::new(height as f32 / dppx)),
                            // FIXME https://github.com/w3c/csswg-drafts/issues/4572
                            ratio: Some(width as f32 / height as f32),
                        };

                        if let Some(layer) =
                            background::layout_layer(self, builder, index, intrinsic)
                        {
                            let image_rendering =
                                image_rendering(self.fragment.style.clone_image_rendering());
                            if layer.repeat {
                                builder.wr.push_repeating_image(
                                    &layer.common,
                                    layer.bounds,
                                    layer.tile_size,
                                    layer.tile_spacing,
                                    image_rendering,
                                    wr::AlphaType::PremultipliedAlpha,
                                    key,
                                    wr::ColorF::WHITE,
                                )
                            } else {
                                builder.wr.push_image(
                                    &layer.common,
                                    layer.bounds,
                                    image_rendering,
                                    wr::AlphaType::PremultipliedAlpha,
                                    key,
                                    wr::ColorF::WHITE,
                                )
                            }
                        }
                    },
                    // Gecko-only value, represented as a (boxed) empty enum on non-Gecko.
                    Image::Rect(rect) => match **rect {},
                },
            }
        }
    }

    fn build_border(&mut self, builder: &mut DisplayListBuilder) {
        let b = self.fragment.style.get_border();
        let widths = SideOffsets2D::new(
            b.border_top_width.px(),
            b.border_right_width.px(),
            b.border_bottom_width.px(),
            b.border_left_width.px(),
        );
        if widths == SideOffsets2D::zero() {
            return;
        }
        let side = |style, color| wr::BorderSide {
            color: rgba(self.fragment.style.resolve_color(color)),
            style: match style {
                BorderStyle::None => wr::BorderStyle::None,
                BorderStyle::Solid => wr::BorderStyle::Solid,
                BorderStyle::Double => wr::BorderStyle::Double,
                BorderStyle::Dotted => wr::BorderStyle::Dotted,
                BorderStyle::Dashed => wr::BorderStyle::Dashed,
                BorderStyle::Hidden => wr::BorderStyle::Hidden,
                BorderStyle::Groove => wr::BorderStyle::Groove,
                BorderStyle::Ridge => wr::BorderStyle::Ridge,
                BorderStyle::Inset => wr::BorderStyle::Inset,
                BorderStyle::Outset => wr::BorderStyle::Outset,
            },
        };
        let common = builder.common_properties(self.border_rect);
        let details = wr::BorderDetails::Normal(wr::NormalBorder {
            top: side(b.border_top_style, b.border_top_color),
            right: side(b.border_right_style, b.border_right_color),
            bottom: side(b.border_bottom_style, b.border_bottom_color),
            left: side(b.border_left_style, b.border_left_color),
            radius: self.border_radius,
            do_aa: true,
        });
        builder
            .wr
            .push_border(&common, self.border_rect, widths, details)
    }
}

fn rgba(rgba: cssparser::RGBA) -> wr::ColorF {
    wr::ColorF::new(
        rgba.red_f32(),
        rgba.green_f32(),
        rgba.blue_f32(),
        rgba.alpha_f32(),
    )
}

fn glyphs(
    glyph_runs: &[Arc<GlyphStore>],
    mut origin: PhysicalPoint<Length>,
) -> Vec<wr::GlyphInstance> {
    use gfx_traits::ByteIndex;
    use range::Range;

    let mut glyphs = vec![];
    for run in glyph_runs {
        for glyph in run.iter_glyphs_for_byte_range(&Range::new(ByteIndex(0), run.len())) {
            if !run.is_whitespace() {
                let glyph_offset = glyph.offset().unwrap_or(Point2D::zero());
                let point = units::LayoutPoint::new(
                    origin.x.px() + glyph_offset.x.to_f32_px(),
                    origin.y.px() + glyph_offset.y.to_f32_px(),
                );
                let glyph = wr::GlyphInstance {
                    index: glyph.id(),
                    point,
                };
                glyphs.push(glyph);
            }
            origin.x += Length::from(glyph.advance());
        }
    }
    glyphs
}

fn hit_info(style: &ComputedValues, tag: OpaqueNode, auto_cursor: Cursor) -> HitInfo {
    use style::computed_values::pointer_events::T as PointerEvents;

    let inherited_ui = style.get_inherited_ui();
    if inherited_ui.pointer_events == PointerEvents::None {
        None
    } else {
        let cursor = cursor(inherited_ui.cursor.keyword, auto_cursor);
        Some((tag.0 as u64, cursor as u16))
    }
}

fn cursor(kind: CursorKind, auto_cursor: Cursor) -> Cursor {
    match kind {
        CursorKind::Auto => auto_cursor,
        CursorKind::None => Cursor::None,
        CursorKind::Default => Cursor::Default,
        CursorKind::Pointer => Cursor::Pointer,
        CursorKind::ContextMenu => Cursor::ContextMenu,
        CursorKind::Help => Cursor::Help,
        CursorKind::Progress => Cursor::Progress,
        CursorKind::Wait => Cursor::Wait,
        CursorKind::Cell => Cursor::Cell,
        CursorKind::Crosshair => Cursor::Crosshair,
        CursorKind::Text => Cursor::Text,
        CursorKind::VerticalText => Cursor::VerticalText,
        CursorKind::Alias => Cursor::Alias,
        CursorKind::Copy => Cursor::Copy,
        CursorKind::Move => Cursor::Move,
        CursorKind::NoDrop => Cursor::NoDrop,
        CursorKind::NotAllowed => Cursor::NotAllowed,
        CursorKind::Grab => Cursor::Grab,
        CursorKind::Grabbing => Cursor::Grabbing,
        CursorKind::EResize => Cursor::EResize,
        CursorKind::NResize => Cursor::NResize,
        CursorKind::NeResize => Cursor::NeResize,
        CursorKind::NwResize => Cursor::NwResize,
        CursorKind::SResize => Cursor::SResize,
        CursorKind::SeResize => Cursor::SeResize,
        CursorKind::SwResize => Cursor::SwResize,
        CursorKind::WResize => Cursor::WResize,
        CursorKind::EwResize => Cursor::EwResize,
        CursorKind::NsResize => Cursor::NsResize,
        CursorKind::NeswResize => Cursor::NeswResize,
        CursorKind::NwseResize => Cursor::NwseResize,
        CursorKind::ColResize => Cursor::ColResize,
        CursorKind::RowResize => Cursor::RowResize,
        CursorKind::AllScroll => Cursor::AllScroll,
        CursorKind::ZoomIn => Cursor::ZoomIn,
        CursorKind::ZoomOut => Cursor::ZoomOut,
    }
}

fn image_rendering(ir: style::computed_values::image_rendering::T) -> wr::ImageRendering {
    use style::computed_values::image_rendering::T as ImageRendering;
    match ir {
        ImageRendering::Auto => wr::ImageRendering::Auto,
        ImageRendering::CrispEdges => wr::ImageRendering::CrispEdges,
        ImageRendering::Pixelated => wr::ImageRendering::Pixelated,
    }
}

/// Radii for the padding edge or content edge
fn inner_radii(mut radii: wr::BorderRadius, offsets: units::LayoutSideOffsets) -> wr::BorderRadius {
    radii.top_left.width -= -offsets.left;
    radii.bottom_left.width -= offsets.left;

    radii.top_right.width -= offsets.right;
    radii.bottom_right.width -= offsets.right;

    radii.top_left.height -= offsets.top;
    radii.top_right.height -= offsets.top;

    radii.bottom_left.height -= offsets.bottom;
    radii.bottom_right.height -= offsets.bottom;
    radii
}

fn clip_for_radii(
    radii: wr::BorderRadius,
    rect: units::LayoutRect,
    builder: &mut DisplayListBuilder,
) -> Option<wr::ClipId> {
    if radii.is_zero() {
        None
    } else {
        Some(builder.wr.define_clip(
            &builder.current_space_and_clip,
            rect,
            Some(wr::ComplexClipRegion {
                rect,
                radii,
                mode: wr::ClipMode::Clip,
            }),
            None,
        ))
    }
}
