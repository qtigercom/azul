#![allow(unused_variables)]
#![allow(unused_macros)]

use std::{
    fmt,
    sync::{Arc, Mutex},
    collections::BTreeMap
};
use webrender::api::*;
use app_units::{AU_PER_PX, MIN_AU, MAX_AU, Au};
use euclid::{TypedRect, TypedSize2D};
use glium::glutin::dpi::LogicalSize;
use {
    FastHashMap,
    app_resources::AppResources,
    default_callbacks::StackCheckedPointer,
    traits::Layout,
    ui_state::UiState,
    ui_description::{UiDescription, StyledNode},
    window_state::WindowSize,
    id_tree::{Arena, NodeId},
    css_parser::*,
    dom::{
        IFrameCallback, GlTextureCallback, NodeData,
        NodeType::{self, Div, Text, Image, GlTexture, IFrame, Label}
    },
    css::ParsedCss,
    text_layout::{TextOverflowPass2, ScrollbarInfo},
    images::ImageId,
    text_cache::TextInfo,
    compositor::new_opengl_texture_id,
    window::{WindowInfo, FakeWindow, HidpiAdjustedBounds},
};

const DEFAULT_FONT_COLOR: StyleTextColor = StyleTextColor(ColorU { r: 0, b: 0, g: 0, a: 255 });

pub(crate) struct DisplayList<'a, T: Layout + 'a> {
    pub(crate) ui_descr: &'a UiDescription<T>,
    pub(crate) rectangles: Arena<DisplayRectangle<'a>>
}

impl<'a, T: Layout + 'a> fmt::Debug for DisplayList<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f,
            "DisplayList {{ ui_descr: {:?}, rectangles: {:?} }}", self.ui_descr, self.rectangles)
    }
}

/// DisplayRectangle is the main type which the layout parsing step gets operated on.
#[derive(Debug)]
pub(crate) struct DisplayRectangle<'a> {
    /// `Some(id)` if this rectangle has a callback attached to it
    /// Note: this is not the same as the `NodeId`!
    /// These two are completely separate numbers!
    pub tag: Option<u64>,
    /// The original styled node
    pub(crate) styled_node: &'a StyledNode,
    /// The style properties of the node, parsed
    pub(crate) style: RectStyle,
    /// The layout properties of the node, parsed
    pub(crate) layout: RectLayout,
}

impl<'a> DisplayRectangle<'a> {
    #[inline]
    pub fn new(tag: Option<u64>, styled_node: &'a StyledNode) -> Self {
        Self {
            tag: tag,
            styled_node: styled_node,
            style: RectStyle::default(),
            layout: RectLayout::default(),
        }
    }
}

impl<'a, T: Layout + 'a> DisplayList<'a, T> {

    /// NOTE: This function assumes that the UiDescription has an initialized arena
    ///
    /// This only looks at the user-facing styles of the `UiDescription`, not the actual
    /// layout. The layout is done only in the `into_display_list_builder` step.
    pub(crate) fn new_from_ui_description(ui_description: &'a UiDescription<T>, ui_state: &UiState<T>) -> Self {

        let arena = ui_description.ui_descr_arena.borrow();
        let display_rect_arena = arena.transform(|node, node_id| {
            let style = ui_description.styled_nodes.get(&node_id).unwrap_or(&ui_description.default_style_of_node);
            let tag = ui_state.node_ids_to_tag_ids.get(&node_id).and_then(|tag| Some(*tag));
            let mut rect = DisplayRectangle::new(tag, style);
            populate_css_properties(&mut rect, node_id, &ui_description.dynamic_css_overrides);
            rect
        });

        Self {
            ui_descr: ui_description,
            rectangles: display_rect_arena,
        }
    }

    /// Looks if any new images need to be uploaded and stores the in the image resources
    fn update_resources(
        api: &RenderApi,
        app_resources: &mut AppResources,
        resource_updates: &mut Vec<ResourceUpdate>)
    {
        Self::update_image_resources(api, app_resources, resource_updates);
        Self::update_font_resources(api, app_resources, resource_updates);
    }

    fn update_image_resources(
        api: &RenderApi,
        app_resources: &mut AppResources,
        resource_updates: &mut Vec<ResourceUpdate>)
    {
        use images::{ImageState, ImageInfo};

        let mut updated_images = Vec::<(ImageId, (ImageData, ImageDescriptor))>::new();
        let mut to_delete_images = Vec::<(ImageId, Option<ImageKey>)>::new();

        // possible performance bottleneck (duplicated cloning) !!
        for (key, value) in app_resources.images.iter() {
            match *value {
                ImageState::ReadyForUpload(ref d) => {
                    updated_images.push((key.clone(), d.clone()));
                },
                ImageState::Uploaded(_) => { },
                ImageState::AboutToBeDeleted((ref k, _)) => {
                    to_delete_images.push((key.clone(), k.clone()));
                }
            }
        }

        // Remove any images that should be deleted
        for (resource_key, image_key) in to_delete_images.into_iter() {
            if let Some(image_key) = image_key {
                resource_updates.push(ResourceUpdate::DeleteImage(image_key));
            }
            app_resources.images.remove(&resource_key);
        }

        // Upload all remaining images to the GPU only if the haven't been
        // uploaded yet
        for (resource_key, (data, descriptor)) in updated_images.into_iter() {

            let key = api.generate_image_key();
            resource_updates.push(ResourceUpdate::AddImage(
                AddImage { key, descriptor, data, tiling: None }
            ));

            *app_resources.images.get_mut(&resource_key).unwrap() =
                ImageState::Uploaded(ImageInfo {
                    key: key,
                    descriptor: descriptor
            });
        }
    }

    // almost the same as update_image_resources, but fonts
    // have two HashMaps that need to be updated
    fn update_font_resources(
        api: &RenderApi,
        app_resources: &mut AppResources,
        resource_updates: &mut Vec<ResourceUpdate>)
    {
        use font::FontState;
        use css_parser::FontId;

        let mut updated_fonts = Vec::<(FontId, Vec<u8>)>::new();
        let mut to_delete_fonts = Vec::<(FontId, Option<(FontKey, Vec<FontInstanceKey>)>)>::new();

        for (key, value) in app_resources.font_data.borrow().iter() {
            match &*(*value.2).borrow() {
                FontState::ReadyForUpload(ref bytes) => {
                    updated_fonts.push((key.clone(), bytes.clone()));
                },
                FontState::Uploaded(_) => { },
                FontState::AboutToBeDeleted(ref font_key) => {
                    let to_delete_font_instances = font_key.and_then(|f_key| {
                        let to_delete_font_instances = app_resources.fonts[&f_key].values().cloned().collect();
                        Some((f_key.clone(), to_delete_font_instances))
                    });
                    to_delete_fonts.push((key.clone(), to_delete_font_instances));
                }
            }
        }

        // Delete the complete font. Maybe a more granular option to
        // keep the font data in memory should be added later
        for (resource_key, to_delete_instances) in to_delete_fonts.into_iter() {
            if let Some((font_key, font_instance_keys)) = to_delete_instances {
                for instance in font_instance_keys {
                    resource_updates.push(ResourceUpdate::DeleteFontInstance(instance));
                }
                resource_updates.push(ResourceUpdate::DeleteFont(font_key));
                app_resources.fonts.remove(&font_key);
            }
            app_resources.font_data.borrow_mut().remove(&resource_key);
        }

        // Upload all remaining fonts to the GPU only if the haven't been uploaded yet
        for (resource_key, data) in updated_fonts.into_iter() {
            let key = api.generate_font_key();
            resource_updates.push(ResourceUpdate::AddFont(AddFont::Raw(key, data, 0))); // TODO: use the index better?
            let mut borrow_mut = app_resources.font_data.borrow_mut();
            *borrow_mut.get_mut(&resource_key).unwrap().2.borrow_mut() = FontState::Uploaded(key);
        }
    }

    /// Inserts and solves the top-level DOM (i.e. the DOM with the ID 0)
    pub(crate) fn into_display_list_builder(
        &self,
        app_data: Arc<Mutex<T>>,
        pipeline_id: PipelineId,
        current_epoch: Epoch,
        window_size_has_changed: bool,
        render_api: &RenderApi,
        parsed_css: &ParsedCss,
        window_size: &WindowSize,
        fake_window: &mut FakeWindow<T>,
        app_resources: &mut AppResources)
    -> DisplayListBuilder
    {
        use glium::glutin::dpi::LogicalSize;

        let mut app_data_access = AppDataAccess(app_data);
        let mut resource_updates = Vec::<ResourceUpdate>::new();

        let (laid_out_rectangles, node_depths) = do_the_layout(
            &self,
            &mut resource_updates,
            app_resources,
            render_api,
            window_size.dimensions,
            LogicalPosition::new(0.0, 0.0)
        );

        let LogicalSize { width, height } = window_size.dimensions;
        let mut builder = DisplayListBuilder::with_capacity(pipeline_id, TypedSize2D::new(width as f32, height as f32), self.rectangles.nodes_len());

        // Upload image and font resources
        Self::update_resources(render_api, app_resources, &mut resource_updates);

        let rects_in_rendering_order = ZOrderedRectangles::new(&self.rectangles, &node_depths);

        push_rectangles_into_displaylist(
            &laid_out_rectangles,
            current_epoch,
            rects_in_rendering_order,
            &DisplayListParametersRef {
                ui_description: self.ui_descr,
                render_api,
                display_rectangle_arena: &self.rectangles,
                parsed_css,
            },
            &mut DisplayListParametersMut {
                app_data: &mut app_data_access,
                app_resources,
                fake_window,
                builder: &mut builder,
                resource_updates: &mut resource_updates,
            },
        );

        render_api.update_resources(resource_updates);
        builder
    }
}

/// Rectangles in rendering order instead of stacking order
#[derive(Debug, Clone)]
struct ZOrderedRectangles(pub BTreeMap<usize, Vec<NodeId>>);

impl ZOrderedRectangles {

    /// Determine the correct implicit z-index rendering order of every rectangle
    pub fn new<'a>(rectangles: &Arena<DisplayRectangle<'a>>, node_depths: &[(usize, NodeId)]) -> ZOrderedRectangles {

        let mut rects_in_rendering_order = BTreeMap::new();
        rects_in_rendering_order.insert(0, vec![NodeId::new(0)]);

        let mut positioned_node_stack = Vec::new();
        let mut node_depths_of_absolute_nodes = BTreeMap::new();

        for (node_depth, parent_id) in node_depths {

            let parent_position = rectangles[*parent_id].data.layout.position.unwrap_or_default();

            if parent_position != LayoutPosition::Static {
                positioned_node_stack.push(*parent_id);
            }

            let z_offset_parent = *(node_depths_of_absolute_nodes.get(parent_id).unwrap_or(&0));

            for child_id in parent_id.children(rectangles) {
                let child_position = rectangles[child_id].data.layout.position.unwrap_or_default();

                // if we have an absolute item, go to the nearest relative item, calculate the number
                // of children of that node, then add it to the self.z_index
                // TODO: sort out all relative nodes when calculating the depth?
                let z_offset_self = if child_position == LayoutPosition::Absolute {
                    let root_id = NodeId::new(0);
                    let last_positioned_node = if positioned_node_stack.is_empty() {
                        &root_id
                    } else {
                        positioned_node_stack.get(positioned_node_stack.len() - 1).unwrap_or(&root_id)
                    };

                    let z_off = get_total_num_children_of_node(*last_positioned_node, rectangles);

                    node_depths_of_absolute_nodes.insert(child_id, z_off);
                    z_off
                } else {
                    0
                };

                let new_node_depth = node_depth + z_offset_parent + z_offset_self + 1;
                rects_in_rendering_order
                    .entry(new_node_depth)
                    .or_insert_with(|| Vec::new())
                    .push(child_id);
            }

            if parent_position != LayoutPosition::Static {
                positioned_node_stack.pop();
            }
        }

        ZOrderedRectangles(rects_in_rendering_order)
    }
}

// Returns how many children a node has (including grandchildren, grand-grandchildren, etc.)
fn get_total_num_children_of_node<T>(id: NodeId, arena: &Arena<T>) -> usize {
    let first_child = match arena[id].first_child {
        None => return 0,
        Some(id) => id,
    };
    let mut last_child = arena[id].last_child.unwrap();
    while let Some(last) = arena[last_child].last_child {
        last_child = last;
    }
    last_child.index() - first_child.index()
}

use glium::glutin::dpi::LogicalPosition;

fn do_the_layout<'a, 'b, T: Layout>(
    display_list: &DisplayList<'a, T>,
    resource_updates: &mut Vec<ResourceUpdate>,
    app_resources: &'b mut AppResources,
    render_api: &RenderApi,
    rect_size: LogicalSize,
    rect_offset: LogicalPosition)
-> (Arena<LayoutRect>, Vec<(usize, NodeId)>)
{
    use text_layout::{split_text_into_words, get_words_cached, Words, FontMetrics};
    use ui_solver::{solve_flex_layout_height, solve_flex_layout_width, get_x_positions, get_y_positions};

    let arena = display_list.ui_descr.ui_descr_arena.borrow();

    let word_cache: BTreeMap<NodeId, (Words, FontMetrics)> = arena
    .linear_iter()
    .filter_map(|id| {

        let (font, font_metrics, font_id, font_size) = match arena[id].data.node_type {
            NodeType::Label(_) | NodeType::Text(_) => {
                use text_layout::TextLayoutOptions;

                let rect = &display_list.rectangles[id].data;
                let style = &rect.style;
                let font_id = style.font_family.as_ref()?.fonts.get(0)?.clone();
                let font_size = style.font_size.unwrap_or(DEFAULT_FONT_SIZE);
                let font_size_app_units = Au((font_size.0.to_pixels() as i32) * AU_PER_PX as i32);
                let font_instance_key = push_font(&font_id, font_size_app_units, resource_updates, app_resources, render_api)?;
                let overflow_behaviour = style.overflow.unwrap_or(LayoutOverflow::default());
                let font = app_resources.get_font(&font_id)?;
                let (horz_alignment, vert_alignment) = determine_text_alignment(rect);

                let text_layout_options = TextLayoutOptions {
                    horz_alignment,
                    vert_alignment,
                    line_height: style.line_height,
                    letter_spacing: style.letter_spacing,
                };
                let font_metrics = FontMetrics::new(&font.0, &font_size, &text_layout_options);

                (font.0, font_metrics, font_id, font_size)
            },
            _ => return None,
        };

        match &arena[id].data.node_type {
            NodeType::Label(ref string_to_render) => {
                Some((id, (split_text_into_words(&string_to_render, &font, font_metrics.font_size_no_line_height, font_metrics.letter_spacing), font_metrics)))
            },
            NodeType::Text(text_id) => {
                // Cloning the words here due to lifetime problems
                Some((id, (get_words_cached(&text_id,
                    &font,
                    &font_id,
                    &font_size,
                    font_metrics.font_size_no_line_height,
                    font_metrics.letter_spacing,
                    &mut app_resources.text_cache).clone(), font_metrics)))
            },
            _ => None,
        }
    }).collect();

    let preferred_widths = arena.transform(|node, _| node.node_type.get_preferred_width(&app_resources.images));
    let solved_widths = solve_flex_layout_width(&display_list.rectangles, preferred_widths, rect_size.width as f32);
    let preferred_heights = arena.transform(|node, id| {
        use text_layout::TextSizePx;
        node.node_type.get_preferred_height_based_on_width(
            TextSizePx(solved_widths.solved_widths[id].data.total()),
            &app_resources.images,
            word_cache.get(&id).and_then(|e| Some(&e.0)),
            word_cache.get(&id).and_then(|e| Some(e.1)),
        ).and_then(|text_size| Some(text_size.0))
    });
    let solved_heights = solve_flex_layout_height(&solved_widths, preferred_heights, rect_size.height as f32);

    let x_positions = get_x_positions(&solved_widths, rect_offset);
    let y_positions = get_y_positions(&solved_heights, &solved_widths, rect_offset);

    (arena.transform(|node, node_id| {
        LayoutRect::new(
            TypedPoint2D::new(x_positions[node_id].data.0, y_positions[node_id].data.0),
            TypedSize2D::new(solved_widths.solved_widths[node_id].data.total(), solved_heights.solved_heights[node_id].data.total())
        )
    }), solved_widths.non_leaf_nodes_sorted_by_depth)
}

fn push_rectangles_into_displaylist<'a, 'b, 'c, 'd, 'e, T: Layout>(
    solved_rects: &Arena<LayoutRect>,
    epoch: Epoch,
    z_ordered_rectangles: ZOrderedRectangles,
    referenced_content: &DisplayListParametersRef<'a,'b,'c,'d, T>,
    referenced_mutable_content: &mut DisplayListParametersMut<'e, T>)
{
    let arena = referenced_content.ui_description.ui_descr_arena.borrow();

    // Workaround for https://github.com/servo/webrender/issues/3262
    let webrender_gamma_hack_necessary = needs_webrender_gamma_correction_hack(&arena);

    for (z_index, rects) in z_ordered_rectangles.0.into_iter() {
        for rect_idx in rects {
            let rectangle = DisplayListRectParams {
                epoch,
                rect_idx,
                html_node: &arena[rect_idx].data.node_type,
            };

            displaylist_handle_rect(solved_rects[rect_idx].data, rectangle, referenced_content, referenced_mutable_content, webrender_gamma_hack_necessary);
        }
    }
}

/// Lazy-lock the Arc<Mutex<T>> - if it is already locked, just construct
/// a `&'a mut T`, if not, push the
pub(crate) struct AppDataAccess<T: Layout>(Arc<Mutex<T>>);

/// Parameters that apply to a single rectangle / div node
#[derive(Copy, Clone)]
pub(crate) struct DisplayListRectParams<'a, T: 'a + Layout> {
    pub epoch: Epoch,
    pub rect_idx: NodeId,
    pub html_node: &'a NodeType<T>,
}

/// Push a single rectangle into the display list builder
#[inline]
fn displaylist_handle_rect<'a,'b,'c,'d,'e,'f, T: Layout>(
    bounds: LayoutRect,
    rectangle: DisplayListRectParams<'c, T>,
    referenced_content: &DisplayListParametersRef<'a,'b,'d,'e, T>,
    referenced_mutable_content: &mut DisplayListParametersMut<'f, T>,
    webrender_gamma_hack_necessary: bool)
{
    use text_layout::{TextOverflow, TextSizePx};
    use webrender::api::BorderRadius;

    let DisplayListParametersRef {
        render_api, parsed_css, ui_description, display_rectangle_arena
    } = referenced_content;

    let DisplayListRectParams {
        epoch, rect_idx, html_node,
    } = rectangle;

    let rect = &display_rectangle_arena[rect_idx].data;

    let info = LayoutPrimitiveInfo {
        rect: bounds,
        clip_rect: bounds,
        is_backface_visible: false,
        tag: rect.tag.and_then(|tag| Some((tag, 0))),
    };

    let clip_region_id = rect.style.border_radius.and_then(|border_radius| {
        let region = ComplexClipRegion {
            rect: bounds,
            radii: border_radius.into(),
            mode: ClipMode::Clip,
        };
        Some(referenced_mutable_content.builder.define_clip(bounds, vec![region], None))
    });

    // Push the "outset" box shadow, before the clip is active
    push_box_shadow(
        referenced_mutable_content.builder,
        &rect.style,
        &bounds,
        BoxShadowClipMode::Outset,
        webrender_gamma_hack_necessary);

    if let Some(id) = clip_region_id {
        referenced_mutable_content.builder.push_clip_id(id);
    }

    // We always have to push the rect, otherwise the hit-testing gets confused
    push_rect(&info,
              referenced_mutable_content.builder,
              &rect.style.background_color.unwrap_or_default(),
              webrender_gamma_hack_necessary);

    if let Some(ref bg) = rect.style.background {
        push_background(
            &info,
            &bounds,
            referenced_mutable_content.builder,
            bg,
            &referenced_mutable_content.app_resources,
            webrender_gamma_hack_necessary);
    };

    if let Some(ref border) = rect.style.border {
        push_border(
            &info,
            referenced_mutable_content.builder,
            &border,
            &rect.style.border_radius,
            webrender_gamma_hack_necessary);
    }

    let (horz_alignment, vert_alignment) = determine_text_alignment(rect);

    let scrollbar_style = ScrollbarInfo {
        width: TextSizePx(17.0),
        padding: TextSizePx(2.0),
        background_color: StyleBackgroundColor(ColorU { r: 241, g: 241, b: 241, a: 255 }),
        triangle_color: StyleBackgroundColor(ColorU { r: 163, g: 163, b: 163, a: 255 }),
        bar_color: StyleBackgroundColor(ColorU { r: 193, g: 193, b: 193, a: 255 }),
    };

    // The only thing changed between TextId and String is
    //`TextInfo::Cached` vs `TextInfo::Uncached` - reduce code duplication
    let push_text_wrapper = |
        text_info: &TextInfo,
        builder: &mut DisplayListBuilder,
        app_resources: &mut AppResources,
        resource_updates: &mut Vec<ResourceUpdate>,
        webrender_gamma_hack_necessary: bool|
    {
        // Adjust the bounds by the padding
        let mut text_bounds = rect.layout.padding.as_ref().and_then(|padding| {
            Some(subtract_padding(&bounds, padding))
        }).unwrap_or(bounds);

        text_bounds.size.width = text_bounds.size.width.max(0.0);
        text_bounds.size.height = text_bounds.size.height.max(0.0);

        let text_clip_region_id = rect.layout.padding.and_then(|_|
            Some(builder.define_clip(text_bounds, vec![ComplexClipRegion {
                rect: text_bounds,
                radii: BorderRadius::zero(),
                mode: ClipMode::Clip,
            }], None))
        );

        if let Some(text_clip_id) = text_clip_region_id {
            builder.push_clip_id(text_clip_id);
        }

        let overflow = push_text(
            &info,
            text_info,
            builder,
            &rect.style,
            app_resources,
            &render_api,
            &text_bounds,
            resource_updates,
            horz_alignment,
            vert_alignment,
            &scrollbar_style,
            webrender_gamma_hack_necessary);

        if text_clip_region_id.is_some() {
            builder.pop_clip_id();
        }

        overflow
    };

    // Handle the special content of the node, return if it overflows in the vertical direction
    let overflow_result = match html_node {
        Div => { None },
        Label(text) => push_text_wrapper(
            &TextInfo::Uncached(text.clone()),
            referenced_mutable_content.builder,
            referenced_mutable_content.app_resources,
            referenced_mutable_content.resource_updates,
            webrender_gamma_hack_necessary),
        Text(text_id) => push_text_wrapper(
            &TextInfo::Cached(*text_id),
            referenced_mutable_content.builder,
            referenced_mutable_content.app_resources,
            referenced_mutable_content.resource_updates,
            webrender_gamma_hack_necessary),
        Image(image_id) => push_image(
            &info,
            referenced_mutable_content.builder,
            referenced_mutable_content.app_resources,
            image_id,
            info.rect.size),
        GlTexture(callback) => push_opengl_texture(callback, &info, rectangle, referenced_content, referenced_mutable_content),
        IFrame(callback) => push_iframe(callback, &info, rectangle, referenced_content, referenced_mutable_content),
    };

    // Push the inset shadow (if any)
    push_box_shadow(
        referenced_mutable_content.builder,
        &rect.style,
        &bounds,
        BoxShadowClipMode::Inset,
        webrender_gamma_hack_necessary);

    if let Some(overflow) = &overflow_result {
        // push scrollbars if necessary
        // If the rectangle should have a scrollbar, push a scrollbar onto the display list
        if rect.style.overflow.unwrap_or_default().allows_vertical_scrollbar() {
            if let TextOverflow::IsOverflowing(amount_vert) = overflow.text_overflow.vertical {
                push_scrollbar(referenced_mutable_content.builder, &overflow.text_overflow, &scrollbar_style, &bounds, &rect.style.border, webrender_gamma_hack_necessary)
            }
        }
        if rect.style.overflow.unwrap_or_default().allows_horizontal_scrollbar() {
            if let TextOverflow::IsOverflowing(amount_horz) = overflow.text_overflow.horizontal {
                push_scrollbar(referenced_mutable_content.builder, &overflow.text_overflow, &scrollbar_style, &bounds, &rect.style.border, webrender_gamma_hack_necessary)
            }
        }
    }

    if clip_region_id.is_some() {
        referenced_mutable_content.builder.pop_clip_id();
    }
}

fn push_opengl_texture<'a, 'b, 'c, 'd, 'e,'f, T: Layout>(
    (texture_callback, texture_stack_ptr): &(GlTextureCallback<T>, StackCheckedPointer<T>),
    info: &LayoutPrimitiveInfo,
    rectangle: DisplayListRectParams<'c, T>,
    referenced_content: &DisplayListParametersRef<'a,'b,'d,'e, T>,
    referenced_mutable_content: &mut DisplayListParametersMut<'f, T>,
) -> Option<OverflowInfo>
{
    use compositor::{ActiveTexture, ACTIVE_GL_TEXTURES};

    let bounds = HidpiAdjustedBounds::from_bounds(&referenced_mutable_content.fake_window, info.rect);

    let texture;

    {
        // Make sure that the app data is locked before invoking the callback
        let _lock = referenced_mutable_content.app_data.0.lock().unwrap();
        let window_info = WindowInfo {
            window: referenced_mutable_content.fake_window,
            resources: &referenced_mutable_content.app_resources,
        };

        texture = (texture_callback.0)(&texture_stack_ptr, window_info, bounds);
    }

    let texture = texture?;

    let opaque = false;
    let allow_mipmaps = true;
    let descriptor = ImageDescriptor::new(info.rect.size.width as u32, info.rect.size.height as u32, ImageFormat::BGRA8, opaque, allow_mipmaps);
    let key = referenced_content.render_api.generate_image_key();
    let external_image_id = ExternalImageId(new_opengl_texture_id() as u64);

    let data = ImageData::External(ExternalImageData {
        id: external_image_id,
        channel_index: 0,
        image_type: ExternalImageType::TextureHandle(TextureTarget::Default),
    });

    ACTIVE_GL_TEXTURES.lock().unwrap()
        .entry(rectangle.epoch).or_insert_with(|| FastHashMap::default())
        .insert(external_image_id, ActiveTexture { texture: texture.clone() });

    referenced_mutable_content.resource_updates.push(ResourceUpdate::AddImage(
        AddImage { key, descriptor, data, tiling: None }
    ));

    referenced_mutable_content.builder.push_image(
        &info,
        LayoutSize::new(texture.inner.width() as f32, texture.inner.height() as f32),
        LayoutSize::zero(),
        ImageRendering::Auto,
        AlphaType::Alpha,
        key,
        ColorF::WHITE);

    None
}

fn push_iframe<'a, 'b, 'c, 'd, 'e, 'f, T: Layout>(
    (iframe_callback, iframe_pointer): &(IFrameCallback<T>, StackCheckedPointer<T>),
    info: &LayoutPrimitiveInfo,
    rectangle: DisplayListRectParams<'c, T>,
    referenced_content: &DisplayListParametersRef<'a,'b,'d,'e, T>,
    referenced_mutable_content: &mut DisplayListParametersMut<'f, T>,
) -> Option<OverflowInfo>
{
    use glium::glutin::dpi::{LogicalPosition, LogicalSize};

    let bounds = HidpiAdjustedBounds::from_bounds(&referenced_mutable_content.fake_window, info.rect);

    let new_dom;

    {
        // Make sure that the app data is locked before invoking the callback
        let _lock = referenced_mutable_content.app_data.0.lock().unwrap();

        let window_info = WindowInfo {
            window: referenced_mutable_content.fake_window,
            resources: &referenced_mutable_content.app_resources,
        };
        new_dom = (iframe_callback.0)(&iframe_pointer, window_info, bounds);
    }

    let ui_state = UiState::from_dom(new_dom);
    let ui_description = UiDescription::<T>::from_dom(&ui_state, &referenced_content.parsed_css);
    let display_list = DisplayList::new_from_ui_description(&ui_description, &ui_state);

    // Insert the DOM into the solver so we can solve the layout of the rectangles
    let rect_size = LogicalSize::new(info.rect.size.width as f64, info.rect.size.height as f64);
    let rect_origin = LogicalPosition::new(info.rect.origin.x as f64, info.rect.origin.y as f64);

    let (laid_out_rectangles, node_depths) = do_the_layout(
        &display_list,
        &mut referenced_mutable_content.resource_updates,
        &mut referenced_mutable_content.app_resources,
        &referenced_content.render_api,
        rect_size,
        rect_origin);

    let z_ordered_rectangles = ZOrderedRectangles::new(&display_list.rectangles, &node_depths);
    let referenced_content = DisplayListParametersRef {
        // Important: Need to update the ui description, otherwise this function would be endlessly recursive
        ui_description: &ui_description,
        display_rectangle_arena: &display_list.rectangles,
        .. *referenced_content
    };

    push_rectangles_into_displaylist(
        &laid_out_rectangles,
        rectangle.epoch,
        z_ordered_rectangles,
        &referenced_content,
        referenced_mutable_content);

    None
}

/// Since the display list can take a lot of parameters, we don't want to
/// continually pass them as parameters of the function and rather use a
/// struct to pass them around. This is purely for ergonomic reasons.
///
/// `DisplayListParametersRef` has only members that are
///  **immutable references** to other things that need to be passed down the display list
#[derive(Copy, Clone)]
struct DisplayListParametersRef<'a, 'b, 'c, 'd, T: 'a + Layout> {
    pub ui_description: &'a UiDescription<T>,
    /// The CSS that should be applied to the DOM
    pub parsed_css: &'b ParsedCss,
    /// Necessary to push
    pub render_api: &'c RenderApi,
    /// Reference to the arena that contains all the styled rectangles
    pub display_rectangle_arena: &'d Arena<DisplayRectangle<'d>>,
}

/// Same as `DisplayListParametersRef`, but for `&mut Something`
///
/// Note: The `'a` in the `'a + Layout` is technically not required.
/// Only rustc 1.28 requires this, more modern compiler versions insert it automatically.
struct DisplayListParametersMut<'a, T: 'a + Layout> {
    /// Needs to be present, because the dom_to_displaylist_builder
    /// could call (recursively) a sub-DOM function again, for example an OpenGL callback
    pub app_data: &'a mut AppDataAccess<T>,
    /// The original, top-level display list builder that we need to push stuff into
    pub builder: &'a mut DisplayListBuilder,
    /// The app resources, so that a sub-DOM / iframe can register fonts and images
    /// TODO: How to handle cleanup ???
    pub app_resources: &'a mut AppResources,
    /// If new fonts or other stuff are created, we need to tell WebRender about this
    pub resource_updates: &'a mut Vec<ResourceUpdate>,
    /// Window access, so that sub-items can register OpenGL textures
    pub fake_window: &'a mut FakeWindow<T>,
}

#[inline]
fn push_rect(
    info: &PrimitiveInfo<LayoutPixel>,
    builder: &mut DisplayListBuilder,
    color: &StyleBackgroundColor,
    webrender_gamma_hack_necessary: bool)
{
    builder.push_rect(&info, reverse_webrender_hack_3262(color.0.into(), webrender_gamma_hack_necessary));
}

struct OverflowInfo {
    pub text_overflow: TextOverflowPass2,
}

/// Note: automatically pushes the scrollbars on the parent,
/// this should be refined later
#[inline]
fn push_text(
    info: &PrimitiveInfo<LayoutPixel>,
    text: &TextInfo,
    builder: &mut DisplayListBuilder,
    style: &RectStyle,
    app_resources: &mut AppResources,
    render_api: &RenderApi,
    bounds: &TypedRect<f32, LayoutPixel>,
    resource_updates: &mut Vec<ResourceUpdate>,
    horz_alignment: StyleTextAlignmentHorz,
    vert_alignment: StyleTextAlignmentVert,
    scrollbar_info: &ScrollbarInfo,
    webrender_gamma_correction_hack_necessary: bool)
-> Option<OverflowInfo>
{
    use text_layout::{self, TextLayoutOptions};

    if text.is_empty_text(&*app_resources) {
        return None;
    }

    let font_id = style.font_family.as_ref()?.fonts.get(0)?.clone();
    let font_size = style.font_size.unwrap_or(DEFAULT_FONT_SIZE);
    let font_size_app_units = Au((font_size.0.to_pixels() as i32) * AU_PER_PX as i32);
    let font_instance_key = push_font(&font_id, font_size_app_units, resource_updates, app_resources, render_api)?;
    let overflow_behaviour = style.overflow.unwrap_or_default();

    let text_layout_options = TextLayoutOptions {
        horz_alignment,
        vert_alignment,
        line_height: style.line_height,
        letter_spacing: style.letter_spacing,
    };

    let (positioned_glyphs, text_overflow) = text_layout::get_glyphs(
        app_resources,
        bounds,
        &font_id,
        &font_size,
        &text_layout_options,
        text,
        &overflow_behaviour,
        scrollbar_info
    );

    // WARNING: Do not enable FontInstanceFlags::FONT_SMOOTHING or FontInstanceFlags::FORCE_AUTOHINT -
    // they seem to interfere with the text layout thereby messing with the actual text layout.
    let font_color = reverse_webrender_hack_3262(style.font_color.unwrap_or(DEFAULT_FONT_COLOR).0.into(), webrender_gamma_correction_hack_necessary);
    let mut flags = FontInstanceFlags::empty();
    flags.set(FontInstanceFlags::SUBPIXEL_BGR, true);
    flags.set(FontInstanceFlags::LCD_VERTICAL, true);

    let options = GlyphOptions {
        render_mode: FontRenderMode::Subpixel,
        flags: flags,
    };

    builder.push_text(&info, &positioned_glyphs, font_instance_key, font_color, Some(options));

    Some(OverflowInfo { text_overflow })
}

/// Adds a scrollbar to the left or bottom side of a rectangle.
/// TODO: make styling configurable (like the width / style of the scrollbar)
fn push_scrollbar(
    builder: &mut DisplayListBuilder,
    scrollbar_info: &TextOverflowPass2,
    scrollbar_style: &ScrollbarInfo,
    bounds: &TypedRect<f32, LayoutPixel>,
    border: &Option<StyleBorder>,
    webrender_gamma_hack_necessary: bool)
{
    use euclid::TypedPoint2D;

    // The border is inside the rectangle - subtract the border width on the left and bottom side,
    // so that the scrollbar is laid out correctly
    let mut bounds = *bounds;
    if let Some(StyleBorder { left: Some(l), bottom: Some(b), .. }) = border {
        bounds.size.width -= l.border_width.to_pixels();
        bounds.size.height -= b.border_width.to_pixels();
    }

    // Background of scrollbar (vertical)
    let scrollbar_vertical_background = TypedRect::<f32, LayoutPixel> {
        origin: TypedPoint2D::new(bounds.origin.x + bounds.size.width - scrollbar_style.width.0, bounds.origin.y),
        size: TypedSize2D::new(scrollbar_style.width.0, bounds.size.height),
    };

    let scrollbar_vertical_background_info = PrimitiveInfo {
        rect: scrollbar_vertical_background,
        clip_rect: bounds,
        is_backface_visible: false,
        tag: None, // TODO: for hit testing
    };

    push_rect(&scrollbar_vertical_background_info, builder, &scrollbar_style.background_color, webrender_gamma_hack_necessary);

    // Actual scroll bar
    let scrollbar_vertical_bar = TypedRect::<f32, LayoutPixel> {
        origin: TypedPoint2D::new(
            bounds.origin.x + bounds.size.width - scrollbar_style.width.0 + scrollbar_style.padding.0,
            bounds.origin.y + scrollbar_style.width.0),
        size: TypedSize2D::new(
            scrollbar_style.width.0 - (scrollbar_style.padding.0 * 2.0),
            bounds.size.height - (scrollbar_style.width.0 * 2.0)),
    };

    let scrollbar_vertical_bar_info = PrimitiveInfo {
        rect: scrollbar_vertical_bar,
        clip_rect: bounds,
        is_backface_visible: false,
        tag: None, // TODO: for hit testing
    };

    push_rect(&scrollbar_vertical_bar_info, builder, &scrollbar_style.bar_color, webrender_gamma_hack_necessary);

    // Triangle top
    let mut scrollbar_triangle_rect = TypedRect::<f32, LayoutPixel> {
        origin: TypedPoint2D::new(
            bounds.origin.x + bounds.size.width - scrollbar_style.width.0 + scrollbar_style.padding.0,
            bounds.origin.y + scrollbar_style.padding.0),
        size: TypedSize2D::new(
            scrollbar_style.width.0 - (scrollbar_style.padding.0 * 2.0),
            scrollbar_style.width.0 - (scrollbar_style.padding.0 * 2.0)),
    };

    scrollbar_triangle_rect.origin.x += scrollbar_triangle_rect.size.width / 4.0;
    scrollbar_triangle_rect.origin.y += scrollbar_triangle_rect.size.height / 4.0;
    scrollbar_triangle_rect.size.width /= 2.0;
    scrollbar_triangle_rect.size.height /= 2.0;

    push_triangle(&scrollbar_triangle_rect, builder, &scrollbar_style.triangle_color, TriangleDirection::PointUp);

    // Triangle bottom
    scrollbar_triangle_rect.origin.y += bounds.size.height - scrollbar_style.width.0 + scrollbar_style.padding.0;
    push_triangle(&scrollbar_triangle_rect, builder, &scrollbar_style.triangle_color, TriangleDirection::PointDown);
}

enum TriangleDirection {
    PointUp,
    PointDown,
    PointRight,
    PointLeft,
}

fn push_triangle(
    bounds: &TypedRect<f32, LayoutPixel>,
    builder: &mut DisplayListBuilder,
    background_color: &StyleBackgroundColor,
    direction: TriangleDirection)
{
    use self::TriangleDirection::*;
    use webrender::api::{LayoutSideOffsets, BorderRadius};

    // see: https://css-tricks.com/snippets/css/css-triangle/
    // uses the "3d effect" for making a triangle

    let triangle_rect_info = PrimitiveInfo {
        rect: *bounds,
        clip_rect: *bounds,
        is_backface_visible: false,
        tag: None,
    };

    const TRANSPARENT: ColorU = ColorU { r: 0, b: 0, g: 0, a: 0 };

    // make all borders but one transparent
    let [b_left, b_right, b_top, b_bottom] = match direction {
        PointUp         => [
            (TRANSPARENT, BorderStyle::Hidden),
            (TRANSPARENT, BorderStyle::Hidden),
            (TRANSPARENT, BorderStyle::Hidden),
            (background_color.0, BorderStyle::Solid)
        ],
        PointDown       => [
            (TRANSPARENT, BorderStyle::Hidden),
            (TRANSPARENT, BorderStyle::Hidden),
            (background_color.0, BorderStyle::Solid),
            (TRANSPARENT, BorderStyle::Hidden)
        ],
        PointLeft       => [
            (TRANSPARENT, BorderStyle::Hidden),
            (background_color.0, BorderStyle::Solid),
            (TRANSPARENT, BorderStyle::Hidden),
            (TRANSPARENT, BorderStyle::Hidden)
        ],
        PointRight      => [
            (background_color.0, BorderStyle::Solid),
            (TRANSPARENT, BorderStyle::Hidden),
            (TRANSPARENT, BorderStyle::Hidden),
            (TRANSPARENT, BorderStyle::Hidden)
        ],
    };

    let border_details = BorderDetails::Normal(NormalBorder {
        left:   BorderSide { color: b_left.0.into(),         style: b_left.1   },
        right:  BorderSide { color: b_right.0.into(),        style: b_right.1  },
        top:    BorderSide { color: b_top.0.into(),          style: b_top.1    },
        bottom: BorderSide { color: b_bottom.0.into(),       style: b_bottom.1 },
        radius: BorderRadius::zero(),
        do_aa: true,
    });

    // make the borders half the width / height of the rectangle,
    // so that the border looks like a triangle
    let left = bounds.size.width / 2.0;
    let top = bounds.size.height / 2.0;
    let bottom = top;
    let right = left;

    let border_widths = LayoutSideOffsets::new(top, right, bottom, left);

    builder.push_border(&triangle_rect_info, border_widths, border_details);
}

/// Workaround for https://github.com/servo/webrender/issues/3262
///
/// If the frame contains an OpenGL texture, webrender seems to apply different brushes (?)
/// and "forget" to correct colors to linear space, making colors lighter than they need to be
///
/// This function scans all nodes for an `GlTexture` callback and returns true if the "reverse gamma correction"
/// needs to be applied.
#[inline]
fn needs_webrender_gamma_correction_hack<T: Layout>(arena: &Arena<NodeData<T>>) -> bool {
    arena.linear_iter().any(|item| match &arena[item].data.node_type {
        NodeType::GlTexture(_) => true,
        _ => false,
    })
}

/// Workaround for https://github.com/servo/webrender/issues/3262
///
/// The hack only needs to be applied if there are any OpenGL textures in the frame,
/// see `needs_webrender_gamma_correction_hack` for more info.
#[inline]
fn reverse_webrender_hack_3262(color: ColorF, hack_necessary: bool) -> ColorF {

    /// Returns a darker color than the original one
    #[inline]
    fn reverse_gamma(color: ColorF) -> ColorF {
        const GAMMA: f32 = 2.2;

        ColorF {
            r: color.r.powf(GAMMA),
            g: color.g.powf(GAMMA),
            b: color.b.powf(GAMMA),
            a: color.a,
        }
    }

    if hack_necessary {
        reverse_gamma(color)
    } else {
        // No hack necessary, just return the original color
        color
    }
}

/// WARNING: For "inset" shadows, you must push a clip ID first, otherwise the
/// shadow will not show up.
///
/// To prevent a shadow from being pushed twice, you have to annotate the clip
/// mode for this - outset or inset.
#[inline]
fn push_box_shadow(
    builder: &mut DisplayListBuilder,
    style: &RectStyle,
    bounds: &LayoutRect,
    shadow_type: BoxShadowClipMode,
    webrender_gamma_correction_hack_necessary: bool)
{
    fn push_box_shadow_inner(
        builder: &mut DisplayListBuilder,
        pre_shadow: &Option<BoxShadowPreDisplayItem>,
        border_radius: StyleBorderRadius,
        bounds: &LayoutRect,
        clip_rect: LayoutRect,
        shadow_type: BoxShadowClipMode,
        webrender_gamma_correction_hack_necessary: bool)
    {
        use webrender::api::LayoutVector2D;

        let pre_shadow = match pre_shadow {
            None => return,
            Some(ref s) => s,
        };

        // The pre_shadow is missing the StyleBorderRadius & LayoutRect
        if pre_shadow.clip_mode != shadow_type {
            return;
        }

        let full_screen_rect = LayoutRect::new(LayoutPoint::zero(), builder.content_size());;

        // prevent shadows that are larger than the full screen
        let clip_rect = clip_rect.intersection(&full_screen_rect).unwrap_or(clip_rect);

        // Apply a gamma of 2.2 to the original value
        //
        // NOTE: strangely box-shadow is the only thing that needs to be gamma-corrected...
        fn apply_gamma(color: ColorF) -> ColorF {

            const GAMMA: f32 = 2.2;
            const GAMMA_F: f32 = 1.0 / GAMMA;

            ColorF {
                r: color.r.powf(GAMMA_F),
                g: color.g.powf(GAMMA_F),
                b: color.b.powf(GAMMA_F),
                a: color.a,
            }
        }

        let info = LayoutPrimitiveInfo::with_clip_rect(LayoutRect::zero(), clip_rect);
        builder.push_box_shadow(
            &info,
            *bounds,
            LayoutVector2D::new(pre_shadow.offset[0].to_pixels(), pre_shadow.offset[1].to_pixels()),
            apply_gamma(reverse_webrender_hack_3262(pre_shadow.color.into(), webrender_gamma_correction_hack_necessary)),
            pre_shadow.blur_radius.to_pixels(),
            pre_shadow.spread_radius.to_pixels(),
            border_radius.into(),
            pre_shadow.clip_mode
        );
    }

    fn get_clip_rect(pre_shadow: &BoxShadowPreDisplayItem, bounds: &LayoutRect) -> LayoutRect {
        if pre_shadow.clip_mode == BoxShadowClipMode::Inset {
            // inset shadows do not work like outset shadows
            // for inset shadows, you have to push a clip ID first, so that they are
            // clipped to the bounds -we trust that the calling function knows to do this
            *bounds
        } else {
            // calculate the maximum extent of the outset shadow
            let mut clip_rect = *bounds;

            let origin_displace = (pre_shadow.spread_radius.to_pixels() + pre_shadow.blur_radius.to_pixels()) * 2.0;
            clip_rect.origin.x = clip_rect.origin.x - pre_shadow.offset[0].to_pixels() - origin_displace;
            clip_rect.origin.y = clip_rect.origin.y - pre_shadow.offset[1].to_pixels() - origin_displace;

            clip_rect.size.height = clip_rect.size.height + (origin_displace * 2.0);
            clip_rect.size.width = clip_rect.size.width + (origin_displace * 2.0);
            clip_rect
        }
    }

    fn push_single_box_shadow_edge(
            builder: &mut DisplayListBuilder,
            current_shadow: &BoxShadowPreDisplayItem,
            bounds: &LayoutRect,
            border_radius: StyleBorderRadius,
            shadow_type: BoxShadowClipMode,
            top: &Option<Option<BoxShadowPreDisplayItem>>,
            bottom: &Option<Option<BoxShadowPreDisplayItem>>,
            left: &Option<Option<BoxShadowPreDisplayItem>>,
            right: &Option<Option<BoxShadowPreDisplayItem>>,
            webrender_gamma_correction_hack_necessary: bool,
    ) {
        let is_inset_shadow = current_shadow.clip_mode == BoxShadowClipMode::Inset;
        let origin_displace = (current_shadow.spread_radius.to_pixels() + current_shadow.blur_radius.to_pixels()) * 2.0;

        let mut shadow_bounds = *bounds;
        let mut clip_rect = *bounds;

        if is_inset_shadow {
            // If the shadow is inset, we adjust the clip rect to be
            // exactly the amount of the shadow
            if let Some(Some(top)) = top {
                clip_rect.size.height = origin_displace;
                shadow_bounds.size.width += origin_displace;
                shadow_bounds.origin.x -= origin_displace / 2.0;
            } else if let Some(Some(bottom)) = bottom {
                clip_rect.size.height = origin_displace;
                clip_rect.origin.y += bounds.size.height - origin_displace;
                shadow_bounds.size.width += origin_displace;
                shadow_bounds.origin.x -= origin_displace / 2.0;
            } else if let Some(Some(left)) = left {
                clip_rect.size.width = origin_displace;
                shadow_bounds.size.height += origin_displace;
                shadow_bounds.origin.y -= origin_displace / 2.0;
            } else if let Some(Some(right)) = right {
                clip_rect.size.width = origin_displace;
                clip_rect.origin.x += bounds.size.width - origin_displace;
                shadow_bounds.size.height += origin_displace;
                shadow_bounds.origin.y -= origin_displace / 2.0;
            }
        } else {
            if let Some(Some(top)) = top {
                clip_rect.size.height = origin_displace;
                clip_rect.origin.y -= origin_displace;
                shadow_bounds.size.width += origin_displace;
                shadow_bounds.origin.x -= origin_displace / 2.0;
            } else if let Some(Some(bottom)) = bottom {
                clip_rect.size.height = origin_displace;
                clip_rect.origin.y += bounds.size.height;
                shadow_bounds.size.width += origin_displace;
                shadow_bounds.origin.x -= origin_displace / 2.0;
            } else if let Some(Some(left)) = left {
                clip_rect.size.width = origin_displace;
                clip_rect.origin.x -= origin_displace;
                shadow_bounds.size.height += origin_displace;
                shadow_bounds.origin.y -= origin_displace / 2.0;
            } else if let Some(Some(right)) = right {
                clip_rect.size.width = origin_displace;
                clip_rect.origin.x += bounds.size.width;
                shadow_bounds.size.height += origin_displace;
                shadow_bounds.origin.y -= origin_displace / 2.0;
            }
        }

        push_box_shadow_inner(builder, &Some(*current_shadow), border_radius, &shadow_bounds, clip_rect, shadow_type, webrender_gamma_correction_hack_necessary);
    }

    // Box-shadow can be applied to each corner separately. This means, in practice
    // that we simply overlay multiple shadows with shifted clipping rectangles
    let StyleBoxShadow { top, left, bottom, right } = match &style.box_shadow {
        Some(s) => s,
        None => return,
    };
    let border_radius = style.border_radius.unwrap_or(StyleBorderRadius::zero());

    enum ShouldPushShadow {
        PushOneShadow,
        PushTwoShadows,
        PushAllShadows,
    }

    let what_shadow_to_push = match [top, left, bottom, right].iter().filter(|x| x.is_some()).count() {
        1 => ShouldPushShadow::PushOneShadow,
        2 => ShouldPushShadow::PushTwoShadows,
        4 => ShouldPushShadow::PushAllShadows,
        _ => return,
    };

    match what_shadow_to_push {
        ShouldPushShadow::PushOneShadow => {
            let current_shadow = match (top, left, bottom, right) {
                 | (Some(Some(shadow)), None, None, None)
                 | (None, Some(Some(shadow)), None, None)
                 | (None, None, Some(Some(shadow)), None)
                 | (None, None, None, Some(Some(shadow)))
                 => shadow,
                 _ => return, // reachable, but invalid box-shadow
            };

            push_single_box_shadow_edge(builder, current_shadow, bounds, border_radius, shadow_type,
                                        top, bottom, left, right, webrender_gamma_correction_hack_necessary);
        },
        // Two shadows in opposite directions:
        //
        // box-shadow-top: 0px 0px 5px red;
        // box-shadow-bottom: 0px 0px 5px blue;
        ShouldPushShadow::PushTwoShadows => {
            match (top, left, bottom, right) {
                (Some(Some(t)), None, Some(Some(b)), right) => {
                    push_single_box_shadow_edge(builder, t, bounds, border_radius, shadow_type,
                                                top, &None, &None, &None, webrender_gamma_correction_hack_necessary);
                    push_single_box_shadow_edge(builder, b, bounds, border_radius, shadow_type,
                                                &None, bottom, &None, &None, webrender_gamma_correction_hack_necessary);

                },
                (None, Some(Some(l)), None, Some(Some(r))) => {
                    push_single_box_shadow_edge(builder, l, bounds, border_radius, shadow_type,
                                                &None, &None, left, &None, webrender_gamma_correction_hack_necessary);
                    push_single_box_shadow_edge(builder, r, bounds, border_radius, shadow_type,
                                                &None, &None, &None, right, webrender_gamma_correction_hack_necessary);
                }
                _ => return, // reachable, but invalid
            }
        },
        ShouldPushShadow::PushAllShadows => {
            // Assumes that all box shadows are the same, so just use the top shadow
            let top_shadow = top.unwrap();
            let clip_rect = top_shadow.as_ref().and_then(|top_shadow| Some(get_clip_rect(top_shadow, bounds))).unwrap_or(*bounds);
            push_box_shadow_inner(builder, &top_shadow, border_radius, bounds, clip_rect, shadow_type, webrender_gamma_correction_hack_necessary);
        }
    }
}

#[inline]
fn push_background(
    info: &PrimitiveInfo<LayoutPixel>,
    bounds: &TypedRect<f32, LayoutPixel>,
    builder: &mut DisplayListBuilder,
    background: &StyleBackground,
    app_resources: &AppResources,
    needs_webrender_gamma_correction_hack: bool)
{
    use css_parser::StyleBackground::*;
    match background {
        RadialGradient(gradient) => {
            use css_parser::Shape;

            let mut stops: Vec<GradientStop> = gradient.stops.iter().map(|gradient_pre|
                GradientStop {
                    offset: gradient_pre.offset.unwrap().get(),
                    color: reverse_webrender_hack_3262(gradient_pre.color.into(), needs_webrender_gamma_correction_hack),
                }).collect();

            let center = bounds.center();

            // Note: division by 2.0 because it's the radius, not the diameter
            let radius = match gradient.shape {
                Shape::Ellipse => TypedSize2D::new(bounds.size.width / 2.0, bounds.size.height / 2.0),
                Shape::Circle => {
                    let largest_bound_size = bounds.size.width.max(bounds.size.height);
                    TypedSize2D::new(largest_bound_size / 2.0, largest_bound_size / 2.0)
                },
            };
            let gradient = builder.create_radial_gradient(center, radius, stops, gradient.extend_mode);
            builder.push_radial_gradient(&info, gradient, bounds.size, LayoutSize::zero());
        },
        LinearGradient(gradient) => {

            let mut stops: Vec<GradientStop> = gradient.stops.iter().map(|gradient_pre|
                GradientStop {
                    offset: gradient_pre.offset.unwrap().get() / 100.0,
                    color: reverse_webrender_hack_3262(gradient_pre.color.into(), needs_webrender_gamma_correction_hack),
                }).collect();

            let (mut begin_pt, mut end_pt) = gradient.direction.to_points(&bounds);
            let gradient = builder.create_gradient(begin_pt, end_pt, stops, gradient.extend_mode);
            builder.push_gradient(&info, gradient, bounds.size, LayoutSize::zero());
        },
        Image(css_image_id) => {
            // TODO: background-origin, background-position, background-repeat
            if let Some(image_id) = app_resources.css_ids_to_image_ids.get(&css_image_id.0) {
                let bounds = info.rect;
                let image_dimensions = app_resources.images.get(image_id).and_then(|i| Some(i.get_dimensions()))
                    .unwrap_or((bounds.size.width, bounds.size.height)); // better than crashing...

                let size = TypedSize2D::new(image_dimensions.0, image_dimensions.1);
                push_image(info, builder, app_resources, image_id, size);
            }
        },
        NoBackground => { },
    }
}

#[inline]
fn push_image(
    info: &PrimitiveInfo<LayoutPixel>,
    builder: &mut DisplayListBuilder,
    app_resources: &AppResources,
    image_id: &ImageId,
    size: TypedSize2D<f32, LayoutPixel>)
-> Option<OverflowInfo>
{
    use images::ImageState::*;

    let image_info = app_resources.images.get(image_id)?;

    match image_info {
        Uploaded(image_info) => {

            // NOTE: The webrender gamma hack doesn't apply to images,
            // since webrender has no way of easily coloring images
            // without using stacking contexts.
            //
            // This leads to lighter images, but that's just how things are right now

            builder.push_image(
                    &info,
                    size,
                    LayoutSize::zero(),
                    ImageRendering::Auto,
                    AlphaType::PremultipliedAlpha,
                    image_info.key,
                    ColorF::WHITE);
        },
        _ => { },
    }

    // TODO: determine if image has overflown its container
    None
}

#[inline]
fn push_border(
    info: &PrimitiveInfo<LayoutPixel>,
    builder: &mut DisplayListBuilder,
    border: &StyleBorder,
    border_radius: &Option<StyleBorderRadius>,
    webrender_gamma_hack_necessary: bool)
{
    if let Some((border_widths, mut border_details)) = border.get_webrender_border(*border_radius) {
        if let BorderDetails::Normal(normal_border) = &mut border_details {
            normal_border.top.color = reverse_webrender_hack_3262(normal_border.top.color, webrender_gamma_hack_necessary);
            normal_border.bottom.color = reverse_webrender_hack_3262(normal_border.bottom.color, webrender_gamma_hack_necessary);
            normal_border.left.color = reverse_webrender_hack_3262(normal_border.left.color, webrender_gamma_hack_necessary);
            normal_border.right.color = reverse_webrender_hack_3262(normal_border.right.color, webrender_gamma_hack_necessary);
        }
        builder.push_border(info, border_widths, border_details);
    }
}

#[inline]
fn push_font(
    font_id: &FontId,
    font_size_app_units: Au,
    resource_updates: &mut Vec<ResourceUpdate>,
    app_resources: &mut AppResources,
    render_api: &RenderApi)
-> Option<FontInstanceKey>
{
    use font::FontState;

    if font_size_app_units < MIN_AU || font_size_app_units > MAX_AU {
        #[cfg(feature = "logging")] {
            error!("warning: too big or too small font size");
        }
        return None;
    }

    let font_state = app_resources.get_font_state(font_id)?;

    let borrow = font_state.borrow();

    match &*borrow {
        FontState::Uploaded(font_key) => {
            let font_sizes_hashmap = app_resources.fonts.entry(*font_key)
                                     .or_insert(FastHashMap::default());
            let font_instance_key = font_sizes_hashmap.entry(font_size_app_units)
                .or_insert_with(|| {
                    let f_instance_key = render_api.generate_font_instance_key();
                    resource_updates.push(ResourceUpdate::AddFontInstance(
                        AddFontInstance {
                            key: f_instance_key,
                            font_key: *font_key,
                            glyph_size: font_size_app_units,
                            options: None,
                            platform_options: None,
                            variations: Vec::new(),
                        }
                    ));
                    f_instance_key
                }
            );

            Some(*font_instance_key)
        },
        _ => {
            // This can happen when the font is loaded for the first time in `.get_font_state`
            // TODO: Make a pre-pass that queries and uploads all non-available fonts
            // error!("warning: trying to use font {:?} that isn't yet available", font_id);
            None
        },
    }
}

/// For a given rectangle, determines what text alignment should be used
fn determine_text_alignment<'a>(rect: &DisplayRectangle<'a>)
-> (StyleTextAlignmentHorz, StyleTextAlignmentVert)
{
    let mut horz_alignment = StyleTextAlignmentHorz::default();
    let mut vert_alignment = StyleTextAlignmentVert::default();

    if let Some(align_items) = rect.layout.align_items {
        // Vertical text alignment
        use css_parser::LayoutAlignItems;
        match align_items {
            LayoutAlignItems::Start => vert_alignment = StyleTextAlignmentVert::Top,
            LayoutAlignItems::End => vert_alignment = StyleTextAlignmentVert::Bottom,
            // technically stretch = blocktext, but we don't have that yet
            _ => vert_alignment = StyleTextAlignmentVert::Center,
        }
    }

    if let Some(justify_content) = rect.layout.justify_content {
        use css_parser::LayoutJustifyContent;
        // Horizontal text alignment
        match justify_content {
            LayoutJustifyContent::Start => horz_alignment = StyleTextAlignmentHorz::Left,
            LayoutJustifyContent::End => horz_alignment = StyleTextAlignmentHorz::Right,
            _ => horz_alignment = StyleTextAlignmentHorz::Center,
        }
    }

    if let Some(text_align) = rect.style.text_align {
        // Horizontal text alignment with higher priority
        horz_alignment = text_align;
    }

    (horz_alignment, vert_alignment)
}

/// Subtracts the padding from the bounds, returning the new bounds
///
/// Warning: The resulting rectangle may have negative width or height
fn subtract_padding(bounds: &TypedRect<f32, LayoutPixel>, padding: &LayoutPadding)
-> TypedRect<f32, LayoutPixel>
{
    let top     = padding.top.and_then(|top| Some(top.to_pixels())).unwrap_or(0.0);
    let bottom  = padding.bottom.and_then(|bottom| Some(bottom.to_pixels())).unwrap_or(0.0);
    let left    = padding.left.and_then(|left| Some(left.to_pixels())).unwrap_or(0.0);
    let right   = padding.right.and_then(|right| Some(right.to_pixels())).unwrap_or(0.0);

    let mut new_bounds = *bounds;

    new_bounds.origin.x += left;
    new_bounds.size.width -= right + left;
    new_bounds.origin.y += top;
    new_bounds.size.height -= top + bottom;

    new_bounds
}

/// Populate the CSS style properties of the `DisplayRectangle`
fn populate_css_properties(
    rect: &mut DisplayRectangle,
    node_id: NodeId,
    css_overrides: &BTreeMap<NodeId, FastHashMap<String, ParsedCssProperty>>)
{
    use css_parser::ParsedCssProperty::{self, *};

    fn apply_parsed_css_property(rect: &mut DisplayRectangle, property: &ParsedCssProperty) {
        match property {
            BorderRadius(b)     => { rect.style.border_radius = Some(*b);                   },
            BackgroundColor(c)  => { rect.style.background_color = Some(*c);                },
            TextColor(t)        => { rect.style.font_color = Some(*t);                      },
            Border(b)           => { StyleBorder::merge(&mut rect.style.border, &b);        },
            Background(b)       => { rect.style.background = Some(b.clone());               },
            FontSize(f)         => { rect.style.font_size = Some(*f);                       },
            FontFamily(f)       => { rect.style.font_family = Some(f.clone());              },
            LetterSpacing(l)    => { rect.style.letter_spacing = Some(*l);                  },
            Overflow(o)         => { LayoutOverflow::merge(&mut rect.style.overflow, &o);   },
            TextAlign(ta)       => { rect.style.text_align = Some(*ta);                     },
            BoxShadow(b)        => { StyleBoxShadow::merge(&mut rect.style.box_shadow, b);  },
            LineHeight(lh)      => { rect.style.line_height = Some(*lh);                    },

            Width(w)            => { rect.layout.width = Some(*w);                          },
            Height(h)           => { rect.layout.height = Some(*h);                         },
            MinWidth(mw)        => { rect.layout.min_width = Some(*mw);                     },
            MinHeight(mh)       => { rect.layout.min_height = Some(*mh);                    },
            MaxWidth(mw)        => { rect.layout.max_width = Some(*mw);                     },
            MaxHeight(mh)       => { rect.layout.max_height = Some(*mh);                    },

            Position(p)         => { rect.layout.position = Some(*p);                       },
            Top(t)              => { rect.layout.top = Some(*t);                            },
            Bottom(b)           => { rect.layout.bottom = Some(*b);                         },
            Right(r)            => { rect.layout.right = Some(*r);                          },
            Left(l)             => { rect.layout.left = Some(*l);                           },

            Padding(p)          => { LayoutPadding::merge(&mut rect.layout.padding, &p);    },
            Margin(m)           => { LayoutMargin::merge(&mut rect.layout.margin, &m);      },

            FlexGrow(g)         => { rect.layout.flex_grow = Some(*g)                       },
            FlexShrink(s)       => { rect.layout.flex_shrink = Some(*s)                     },
            FlexWrap(w)         => { rect.layout.wrap = Some(*w);                           },
            FlexDirection(d)    => { rect.layout.direction = Some(*d);                      },
            JustifyContent(j)   => { rect.layout.justify_content = Some(*j);                },
            AlignItems(a)       => { rect.layout.align_items = Some(*a);                    },
            AlignContent(a)     => { rect.layout.align_content = Some(*a);                  },
        }
    }

    use css::DynamicCssPropertyDefault;

    // Assert that the types of two properties matches
    fn property_type_matches(a: &ParsedCssProperty, b: &DynamicCssPropertyDefault) -> bool {
        use std::mem::discriminant;
        use css::DynamicCssPropertyDefault::*;
        match b {
            Exact(e) => discriminant(a) == discriminant(e),
            Auto => true, // "auto" always matches
        }
    }

    // Apply / static / dynamic properties
    for constraint in &rect.styled_node.css_constraints.list {
        use css::CssDeclaration::*;
        match constraint {
            Static(static_property) => apply_parsed_css_property(rect, static_property),
            Dynamic(dynamic_property) => {
                if let Some(overridden_property) = css_overrides.get(&node_id).and_then(|overrides| overrides.get(&dynamic_property.dynamic_id)) {
                    // Only apply the dynamic CSS property default, if it isn't set to auto
                    if property_type_matches(overridden_property, &dynamic_property.default) {
                        apply_parsed_css_property(rect, overridden_property);
                    } else {
                        #[cfg(feature = "logging")] {
                            error!(
                                "Dynamic CSS property on rect {:?} don't have the same discriminant type,\r\n
                                cannot override {:?} with {:?} - enum discriminant mismatch",
                                rect, dynamic_property.default, overridden_property
                            )
                        }
                    }
                } else if let DynamicCssPropertyDefault::Exact(default) = &dynamic_property.default {
                    apply_parsed_css_property(rect, default);
                }
            }
        }
    }
}
