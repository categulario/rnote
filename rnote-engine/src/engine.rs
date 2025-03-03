use std::path::PathBuf;
use std::sync::Arc;

use crate::document::Layout;
use crate::import::PdfImportPrefs;
use crate::pens::penholder::PenStyle;
use crate::pens::PenMode;
use crate::store::StrokeKey;
use crate::strokes::strokebehaviour::GeneratedStrokeImages;
use crate::{render, AudioPlayer, DrawBehaviour, DrawOnDocBehaviour, WidgetFlags};
use crate::{Camera, Document, PenHolder, StrokeStore};
use gtk4::Snapshot;
use piet::RenderContext;
use rnote_compose::helpers::{AABBHelpers, Vector2Helpers};
use rnote_compose::penhelpers::{PenEvent, ShortcutKey};
use rnote_compose::transform::TransformBehaviour;
use rnote_fileformats::rnoteformat::RnotefileMaj0Min5;
use rnote_fileformats::{xoppformat, FileFormatSaver};

use anyhow::Context;
use futures::channel::{mpsc, oneshot};
use p2d::bounding_volume::{BoundingVolume, AABB};
use serde::{Deserialize, Serialize};

/// A view into the rest of the engine, excluding the penholder
#[allow(missing_debug_implementations)]
pub struct EngineView<'a> {
    pub tasks_tx: EngineTaskSender,
    pub doc: &'a Document,
    pub store: &'a StrokeStore,
    pub camera: &'a Camera,
    pub audioplayer: &'a Option<AudioPlayer>,
}

/// A mutable view into the rest of the engine, excluding the penholder
#[allow(missing_debug_implementations)]
pub struct EngineViewMut<'a> {
    pub tasks_tx: EngineTaskSender,
    pub doc: &'a mut Document,
    pub store: &'a mut StrokeStore,
    pub camera: &'a mut Camera,
    pub audioplayer: &'a mut Option<AudioPlayer>,
}

impl<'a> EngineViewMut<'a> {
    // converts itself to the immutable view
    pub fn as_im<'m>(&'m self) -> EngineView<'m> {
        EngineView::<'m> {
            tasks_tx: self.tasks_tx.clone(),
            doc: self.doc,
            store: self.store,
            camera: self.camera,
            audioplayer: self.audioplayer,
        }
    }
}

#[derive(Debug, Clone)]
/// A engine task, usually coming from a spawned thread and to be processed with `process_received_task()`.
pub enum EngineTask {
    /// Replace the images of the render_comp.
    /// Note that usually the state of the render component should be set **before** spawning a thread, generating images and sending this task,
    /// to avoid spawning large amounts of already outdated rendering tasks when checking the render component state on resize / zooming, etc.
    UpdateStrokeWithImages {
        key: StrokeKey,
        images: GeneratedStrokeImages,
    },
    /// Appends the images to the rendering of the stroke
    /// Note that usually the state of the render component should be set **before** spawning a thread, generating images and sending this task,
    /// to avoid spawning large amounts of already outdated rendering tasks when checking the render component state on resize / zooming, etc.
    AppendImagesToStroke {
        key: StrokeKey,
        images: GeneratedStrokeImages,
    },
    /// indicates that the application is quitting. Usually handled to quit the async loop which receives the tasks
    Quit,
}

#[allow(missing_debug_implementations)]
#[derive(Serialize, Deserialize)]
#[serde(default, rename = "engine_config")]
struct EngineConfig {
    #[serde(rename = "document")]
    document: serde_json::Value,
    #[serde(rename = "penholder")]
    penholder: serde_json::Value,
    #[serde(rename = "pdf_import_prefs")]
    pdf_import_prefs: serde_json::Value,
    #[serde(rename = "pen_sounds")]
    pen_sounds: serde_json::Value,
}

impl Default for EngineConfig {
    fn default() -> Self {
        let engine = RnoteEngine::new(None);

        Self {
            document: serde_json::to_value(&engine.document).unwrap(),
            penholder: serde_json::to_value(&engine.penholder).unwrap(),

            pdf_import_prefs: serde_json::to_value(&engine.pdf_import_prefs).unwrap(),
            pen_sounds: serde_json::to_value(&engine.pen_sounds).unwrap(),
        }
    }
}

pub type EngineTaskSender = mpsc::UnboundedSender<EngineTask>;
pub type EngineTaskReceiver = mpsc::UnboundedReceiver<EngineTask>;

/// The engine.
#[allow(missing_debug_implementations)]
#[derive(Serialize, Deserialize)]
#[serde(default, rename = "engine")]
pub struct RnoteEngine {
    #[serde(rename = "document")]
    pub document: Document,
    #[serde(rename = "penholder")]
    pub penholder: PenHolder,
    #[serde(rename = "store")]
    pub store: StrokeStore,
    #[serde(rename = "camera")]
    pub camera: Camera,

    #[serde(rename = "pdf_import_prefs")]
    pub pdf_import_prefs: PdfImportPrefs,
    #[serde(rename = "pen_sounds")]
    pub pen_sounds: bool,

    #[serde(skip)]
    pub audioplayer: Option<AudioPlayer>,
    #[serde(skip)]
    pub visual_debug: bool,
    #[serde(skip)]
    pub tasks_tx: EngineTaskSender,
    /// To be taken out into a loop which processes the receiver stream. The received tasks should be processed with process_received_task()
    #[serde(skip)]
    pub tasks_rx: Option<EngineTaskReceiver>,
}

impl Default for RnoteEngine {
    fn default() -> Self {
        Self::new(None)
    }
}

impl RnoteEngine {
    /// The used image scale factor on export
    pub const EXPORT_IMAGE_SCALE: f64 = 1.5;

    #[allow(clippy::new_without_default)]
    pub fn new(data_dir: Option<PathBuf>) -> Self {
        let (tasks_tx, tasks_rx) = futures::channel::mpsc::unbounded::<EngineTask>();
        let pen_sounds = false;
        let audioplayer = if let Some(data_dir) = data_dir {
            AudioPlayer::new(data_dir)
                .map_err(|e| {
                    log::error!(
                        "failed to create a new audio player in PenHolder::default(), Err {}",
                        e
                    );
                })
                .map(|mut audioplayer| {
                    audioplayer.enabled = pen_sounds;
                    audioplayer
                })
                .ok()
        } else {
            None
        };

        Self {
            document: Document::default(),
            penholder: PenHolder::default(),
            store: StrokeStore::default(),
            camera: Camera::default(),

            pdf_import_prefs: PdfImportPrefs::default(),
            pen_sounds,

            audioplayer,
            visual_debug: false,
            tasks_tx,
            tasks_rx: Some(tasks_rx),
        }
    }

    pub fn tasks_tx(&self) -> EngineTaskSender {
        self.tasks_tx.clone()
    }

    /// Gets the EngineView
    pub fn view(&self) -> EngineView {
        EngineView {
            tasks_tx: self.tasks_tx.clone(),
            doc: &self.document,
            store: &self.store,
            camera: &self.camera,
            audioplayer: &self.audioplayer,
        }
    }

    /// Gets the EngineViewMut
    pub fn view_mut(&mut self) -> EngineViewMut {
        EngineViewMut {
            tasks_tx: self.tasks_tx.clone(),
            doc: &mut self.document,
            store: &mut self.store,
            camera: &mut self.camera,
            audioplayer: &mut self.audioplayer,
        }
    }

    /// wether pen sounds are enabled
    pub fn pen_sounds(&self) -> bool {
        self.pen_sounds
    }

    /// enables / disables the pen sounds
    pub fn set_pen_sounds(&mut self, pen_sounds: bool) {
        self.pen_sounds = pen_sounds;

        if let Some(audioplayer) = self.audioplayer.as_mut() {
            audioplayer.enabled = pen_sounds;
        }
    }

    /// records the current store state and saves it as a history entry.
    pub fn record(&mut self) -> WidgetFlags {
        self.store.record()
    }

    /// Undo the latest changes
    pub fn undo(&mut self) -> WidgetFlags {
        let mut widget_flags = WidgetFlags::default();
        let current_pen_style = self.penholder.current_style_w_override();

        if current_pen_style != PenStyle::Selector {
            widget_flags.merge_with_other(self.handle_pen_event(PenEvent::Cancel, None));
        }

        widget_flags.merge_with_other(self.store.undo());

        if !self.store.selection_keys_unordered().is_empty() {
            widget_flags.merge_with_other(
                self.penholder
                    .force_style_override_without_sideeffects(None),
            );
            widget_flags.merge_with_other(
                self.penholder
                    .force_style_without_sideeffects(PenStyle::Selector),
            );
        }

        self.resize_autoexpand();
        self.update_pens_states();
        self.update_rendering_current_viewport();

        widget_flags.redraw = true;

        widget_flags
    }

    /// redo the latest changes
    pub fn redo(&mut self) -> WidgetFlags {
        let mut widget_flags = WidgetFlags::default();
        let current_pen_style = self.penholder.current_style_w_override();

        if current_pen_style != PenStyle::Selector {
            widget_flags.merge_with_other(self.handle_pen_event(PenEvent::Cancel, None));
        }

        widget_flags.merge_with_other(self.store.redo());

        if !self.store.selection_keys_unordered().is_empty() {
            widget_flags.merge_with_other(
                self.penholder
                    .force_style_override_without_sideeffects(None),
            );
            widget_flags.merge_with_other(
                self.penholder
                    .force_style_without_sideeffects(PenStyle::Selector),
            );
        }

        self.resize_autoexpand();
        self.update_pens_states();
        self.update_rendering_current_viewport();

        widget_flags.redraw = true;

        widget_flags
    }

    // Clears the store
    pub fn clear(&mut self) {
        self.store.clear();
        self.update_pens_states();
    }

    /// processes the received task from tasks_rx.
    /// Returns widget flags to indicate what needs to be updated in the UI.
    /// An example how to use it:
    /// ```rust, ignore
    /// let main_cx = glib::MainContext::default();

    /// main_cx.spawn_local(clone!(@strong canvas, @strong appwindow => async move {
    ///            let mut task_rx = canvas.engine().borrow_mut().store.tasks_rx.take().unwrap();

    ///           loop {
    ///              if let Some(task) = task_rx.next().await {
    ///                    let widget_flags = canvas.engine().borrow_mut().process_received_task(task);
    ///                    if appwindow.handle_widget_flags(widget_flags) {
    ///                         break;
    ///                    }
    ///                }
    ///            }
    ///        }));
    /// ```
    /// Processes a received store task. Usually called from a receiver loop which polls tasks_rx.
    pub fn process_received_task(&mut self, task: EngineTask) -> WidgetFlags {
        let mut widget_flags = WidgetFlags::default();

        match task {
            EngineTask::UpdateStrokeWithImages { key, images } => {
                if let Err(e) = self.store.replace_rendering_with_images(key, images) {
                    log::error!("replace_rendering_with_images() in process_received_task() failed with Err {}", e);
                }

                widget_flags.redraw = true;
                widget_flags.indicate_changed_store = true;
            }
            EngineTask::AppendImagesToStroke { key, images } => {
                if let Err(e) = self.store.append_rendering_images(key, images) {
                    log::error!(
                        "append_rendering_images() in process_received_task() failed with Err {}",
                        e
                    );
                }

                widget_flags.redraw = true;
                widget_flags.indicate_changed_store = true;
            }
            EngineTask::Quit => {
                widget_flags.quit = true;
            }
        }

        widget_flags
    }

    /// handle an pen event
    pub fn handle_pen_event(&mut self, event: PenEvent, pen_mode: Option<PenMode>) -> WidgetFlags {
        self.penholder.handle_pen_event(
            event,
            pen_mode,
            &mut EngineViewMut {
                tasks_tx: self.tasks_tx(),
                doc: &mut self.document,
                store: &mut self.store,
                camera: &mut self.camera,
                audioplayer: &mut self.audioplayer,
            },
        )
    }

    /// Handle a pressed shortcut key
    pub fn handle_pen_pressed_shortcut_key(&mut self, shortcut_key: ShortcutKey) -> WidgetFlags {
        self.penholder.handle_pressed_shortcut_key(
            shortcut_key,
            &mut EngineViewMut {
                tasks_tx: self.tasks_tx(),
                doc: &mut self.document,
                store: &mut self.store,
                camera: &mut self.camera,
                audioplayer: &mut self.audioplayer,
            },
        )
    }

    /// change the pen style
    pub fn change_pen_style(&mut self, new_style: PenStyle) -> WidgetFlags {
        self.penholder.change_style(
            new_style,
            &mut EngineViewMut {
                tasks_tx: self.tasks_tx(),
                doc: &mut self.document,
                store: &mut self.store,
                camera: &mut self.camera,
                audioplayer: &mut self.audioplayer,
            },
        )
    }

    /// change the pen style override
    pub fn change_pen_style_override(
        &mut self,
        new_style_override: Option<PenStyle>,
    ) -> WidgetFlags {
        self.penholder.change_style_override(
            new_style_override,
            &mut EngineViewMut {
                tasks_tx: self.tasks_tx(),
                doc: &mut self.document,
                store: &mut self.store,
                camera: &mut self.camera,
                audioplayer: &mut self.audioplayer,
            },
        )
    }

    /// change the pen mode. Relevant for stylus input
    pub fn change_pen_mode(&mut self, pen_mode: PenMode) -> WidgetFlags {
        self.penholder.change_pen_mode(
            pen_mode,
            &mut EngineViewMut {
                tasks_tx: self.tasks_tx(),
                doc: &mut self.document,
                store: &mut self.store,
                camera: &mut self.camera,
                audioplayer: &mut self.audioplayer,
            },
        )
    }

    /// updates the background rendering for the current viewport.
    /// if the background pattern or zoom has changed, background.regenerate_pattern() needs to be called first.
    pub fn update_background_rendering_current_viewport(&mut self) {
        let viewport = self.camera.viewport();

        // Update background and strokes for the new viewport
        if let Err(e) = self.document.background.update_rendernodes(viewport) {
            log::error!(
                "failed to update background rendernodes on canvas resize with Err {}",
                e
            );
        }
    }

    /// updates the content rendering for the current viewport. including the background rendering.
    pub fn update_rendering_current_viewport(&mut self) {
        let viewport = self.camera.viewport();
        let image_scale = self.camera.image_scale();

        self.update_background_rendering_current_viewport();

        self.store.regenerate_rendering_in_viewport_threaded(
            self.tasks_tx(),
            false,
            viewport,
            image_scale,
        );
    }

    // Generates bounds for each page on the document which contains content
    pub fn pages_bounds_w_content(&self) -> Vec<AABB> {
        let doc_bounds = self.document.bounds();
        let keys = self.store.stroke_keys_as_rendered();

        let strokes_bounds = self.store.strokes_bounds(&keys);

        let pages_bounds = doc_bounds
            .split_extended_origin_aligned(na::vector![
                self.document.format.width,
                self.document.format.height
            ])
            .into_iter()
            .filter(|page_bounds| {
                // Filter the pages out that doesn't intersect with any stroke
                strokes_bounds
                    .iter()
                    .any(|stroke_bounds| stroke_bounds.intersects(page_bounds))
            })
            .collect::<Vec<AABB>>();

        if pages_bounds.is_empty() {
            // If no page has content, return the origin page
            vec![AABB::new(
                na::point![0.0, 0.0],
                na::point![self.document.format.width, self.document.format.height],
            )]
        } else {
            pages_bounds
        }
    }

    /// Generates bounds which contain all pages on the doc with content extended to fit the format.
    pub fn bounds_w_content_extended(&self) -> Option<AABB> {
        let pages_bounds = self.pages_bounds_w_content();

        if pages_bounds.is_empty() {
            return None;
        }

        Some(
            pages_bounds
                .into_iter()
                .fold(AABB::new_invalid(), |prev, next| prev.merged(&next)),
        )
    }

    /// the current document layout
    pub fn doc_layout(&self) -> Layout {
        self.document.layout()
    }

    pub fn set_doc_layout(&mut self, layout: Layout) {
        self.document.set_layout(layout, &self.store, &self.camera);
    }

    /// resizes the doc to the format and to fit all strokes
    /// Document background rendering then needs to be updated.
    pub fn resize_to_fit_strokes(&mut self) {
        self.document
            .resize_to_fit_strokes(&self.store, &self.camera);
    }

    /// resize the doc when in autoexpanding layouts. called e.g. when finishing a new stroke
    /// Document background rendering then needs to be updated.
    pub fn resize_autoexpand(&mut self) {
        self.document.resize_autoexpand(&self.store, &self.camera);
    }

    /// Updates the camera and expands doc dimensions with offset
    /// Document background rendering then needs to be updated.
    pub fn update_camera_offset(&mut self, new_offset: na::Vector2<f64>) {
        self.camera.offset = new_offset;

        match self.document.layout() {
            Layout::FixedSize => {
                // Does not resize in fixed size mode, use resize_doc_to_fit_strokes() for it.
            }
            Layout::ContinuousVertical => {
                self.document
                    .resize_doc_continuous_vertical_layout(&self.store);
            }
            Layout::Infinite => {
                // only expand, don't resize to fit strokes
                self.document
                    .expand_doc_infinite_layout(self.camera.viewport());
            }
        }
    }

    /// Updates pens state with the current engine state.
    /// needs to be called when the engine state was changed outside of pen events. ( e.g. trash all strokes, set strokes selected, etc. )
    pub fn update_pens_states(&mut self) {
        self.penholder.update_internal_state(&EngineView {
            tasks_tx: self.tasks_tx(),
            doc: &self.document,
            store: &self.store,
            camera: &self.camera,
            audioplayer: &self.audioplayer,
        });
    }

    /// Fetches clipboard content from current state.
    /// Returns (the content, mime_type)
    pub fn fetch_clipboard_content(&self) -> anyhow::Result<Option<(Vec<u8>, String)>> {
        // First try exporting the selection as svg
        if let Some(selection_svg) = self.export_selection_as_svg_string(false)? {
            return Ok(Some((
                selection_svg.into_bytes(),
                String::from("image/svg+xml"),
            )));
        }

        // else fetch from pen
        self.penholder.fetch_clipboard_content(&EngineView {
            tasks_tx: self.tasks_tx(),
            doc: &self.document,
            store: &self.store,
            camera: &self.camera,
            audioplayer: &self.audioplayer,
        })
    }

    // pastes clipboard content
    pub fn paste_clipboard_content(
        &mut self,
        clipboard_content: &[u8],
        mime_types: Vec<String>,
    ) -> WidgetFlags {
        self.penholder.paste_clipboard_content(
            clipboard_content,
            mime_types,
            &mut EngineViewMut {
                tasks_tx: self.tasks_tx(),
                doc: &mut self.document,
                store: &mut self.store,
                camera: &mut self.camera,
                audioplayer: &mut self.audioplayer,
            },
        )
    }

    /// Imports and replace the engine config. NOT for opening files
    pub fn load_engine_config(&mut self, serialized_config: &str) -> anyhow::Result<()> {
        let engine_config = serde_json::from_str::<EngineConfig>(serialized_config)?;

        self.document = serde_json::from_value(engine_config.document)?;
        self.penholder = serde_json::from_value(engine_config.penholder)?;
        self.pdf_import_prefs = serde_json::from_value(engine_config.pdf_import_prefs)?;
        self.pen_sounds = serde_json::from_value(engine_config.pen_sounds)?;

        // Set the pen sounds to update the audioplayer
        self.set_pen_sounds(self.pen_sounds);

        Ok(())
    }

    /// Exports the current engine config as JSON string
    pub fn save_engine_config(&self) -> anyhow::Result<String> {
        let engine_config = EngineConfig {
            document: serde_json::to_value(&self.document)?,
            penholder: serde_json::to_value(&self.penholder)?,
            pdf_import_prefs: serde_json::to_value(&self.pdf_import_prefs)?,
            pen_sounds: serde_json::to_value(&self.pen_sounds)?,
        };

        Ok(serde_json::to_string(&engine_config)?)
    }

    /// Saves the current state as a .rnote file.
    pub fn save_as_rnote_bytes(
        &self,
        file_name: String,
    ) -> anyhow::Result<oneshot::Receiver<anyhow::Result<Vec<u8>>>> {
        let (oneshot_sender, oneshot_receiver) = oneshot::channel::<anyhow::Result<Vec<u8>>>();

        let mut store_snapshot = self.store.take_store_snapshot();
        Arc::make_mut(&mut store_snapshot).process_before_saving();

        // the doc is currently not thread safe, so we have to serialize it in the same thread that holds the engine
        let doc = serde_json::to_value(&self.document)?;

        rayon::spawn(move || {
            let result = || -> anyhow::Result<Vec<u8>> {
                let rnote_file = RnotefileMaj0Min5 {
                    document: doc,
                    store_snapshot: serde_json::to_value(&*store_snapshot)?,
                };

                rnote_file.save_as_bytes(&file_name)
            };

            if let Err(_data) = oneshot_sender.send(result()) {
                log::error!("sending result to receiver in save_as_rnote_bytes() failed. Receiver already dropped.");
            }
        });

        Ok(oneshot_receiver)
    }

    /// Exports the entire engine state as JSON string
    /// Only use for debugging
    pub fn export_state_as_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// generates the doc svg.
    /// The coordinates are translated so that the svg has origin 0.0, 0.0
    /// without root or xml header.
    pub fn gen_doc_svg(&self, with_background: bool) -> Result<render::Svg, anyhow::Error> {
        let doc_bounds = self.document.bounds();

        let strokes = self.store.stroke_keys_as_rendered();

        let mut doc_svg = if with_background {
            let mut background_svg = self.document.background.gen_svg(doc_bounds)?;

            background_svg.wrap_svg_root(
                Some(AABB::new(
                    na::point![0.0, 0.0],
                    na::Point2::from(doc_bounds.extents()),
                )),
                Some(doc_bounds),
                true,
            );

            background_svg
        } else {
            // we can have invalid bounds here, because we know we merge them with the strokes svg
            render::Svg {
                svg_data: String::new(),
                bounds: AABB::new(na::point![0.0, 0.0], na::Point2::from(doc_bounds.extents())),
            }
        };

        doc_svg.merge([render::Svg::gen_with_piet_cairo_backend(
            |piet_cx| {
                piet_cx.transform(kurbo::Affine::translate(
                    doc_bounds.mins.coords.to_kurbo_vec(),
                ));

                self.store.draw_stroke_keys_to_piet(
                    &strokes,
                    piet_cx,
                    RnoteEngine::EXPORT_IMAGE_SCALE,
                )
            },
            AABB::new(na::point![0.0, 0.0], na::Point2::from(doc_bounds.extents())),
        )?]);

        Ok(doc_svg)
    }

    /// generates the doc svg for the given viewport.
    /// The coordinates are translated so that the svg has origin 0.0, 0.0
    /// without root or xml header.
    pub fn gen_doc_svg_with_viewport(
        &self,
        viewport: AABB,
        with_background: bool,
    ) -> Result<render::Svg, anyhow::Error> {
        // Background bounds are still doc bounds, for correct alignment of the background pattern
        let mut doc_svg = if with_background {
            let mut background_svg = self.document.background.gen_svg(viewport)?;

            background_svg.wrap_svg_root(
                Some(AABB::new(
                    na::point![0.0, 0.0],
                    na::Point2::from(viewport.extents()),
                )),
                Some(viewport),
                true,
            );

            background_svg
        } else {
            // we can have invalid bounds here, because we know we merge them with the other svg
            render::Svg {
                svg_data: String::new(),
                bounds: AABB::new(na::point![0.0, 0.0], na::Point2::from(viewport.extents())),
            }
        };

        let strokes_in_viewport = self
            .store
            .stroke_keys_as_rendered_intersecting_bounds(viewport);

        doc_svg.merge([render::Svg::gen_with_piet_cairo_backend(
            |piet_cx| {
                piet_cx.transform(kurbo::Affine::translate(
                    -viewport.mins.coords.to_kurbo_vec(),
                ));

                self.store.draw_stroke_keys_to_piet(
                    &strokes_in_viewport,
                    piet_cx,
                    RnoteEngine::EXPORT_IMAGE_SCALE,
                )
            },
            AABB::new(na::point![0.0, 0.0], na::Point2::from(viewport.extents())),
        )?]);

        Ok(doc_svg)
    }

    /// generates the selection svg.
    /// The coordinates are translated so that the svg has origin 0.0, 0.0
    /// without root or xml header.
    pub fn gen_selection_svg(
        &self,
        with_background: bool,
    ) -> Result<Option<render::Svg>, anyhow::Error> {
        let selection_keys = self.store.selection_keys_as_rendered();

        if selection_keys.is_empty() {
            return Ok(None);
        }

        let selection_bounds =
            if let Some(selection_bounds) = self.store.bounds_for_strokes(&selection_keys) {
                selection_bounds
            } else {
                return Ok(None);
            };

        let mut selection_svg = if with_background {
            let mut background_svg = self.document.background.gen_svg(selection_bounds)?;

            background_svg.wrap_svg_root(
                Some(AABB::new(
                    na::point![0.0, 0.0],
                    na::Point2::from(selection_bounds.extents()),
                )),
                Some(selection_bounds),
                true,
            );

            background_svg
        } else {
            render::Svg {
                svg_data: String::new(),
                bounds: AABB::new(
                    na::point![0.0, 0.0],
                    na::Point2::from(selection_bounds.extents()),
                ),
            }
        };

        selection_svg.merge([render::Svg::gen_with_piet_cairo_backend(
            |piet_cx| {
                piet_cx.transform(kurbo::Affine::translate(
                    -selection_bounds.mins.coords.to_kurbo_vec(),
                ));

                self.store.draw_stroke_keys_to_piet(
                    &selection_keys,
                    piet_cx,
                    RnoteEngine::EXPORT_IMAGE_SCALE,
                )
            },
            AABB::new(
                na::point![0.0, 0.0],
                na::Point2::from(selection_bounds.extents()),
            ),
        )?]);

        Ok(Some(selection_svg))
    }

    /// Exports the doc with the strokes as a SVG string.
    pub fn export_doc_as_svg_string(&self, with_background: bool) -> Result<String, anyhow::Error> {
        let doc_svg = self.gen_doc_svg(with_background)?;

        Ok(rnote_compose::utils::add_xml_header(
            rnote_compose::utils::wrap_svg_root(
                doc_svg.svg_data.as_str(),
                Some(doc_svg.bounds),
                Some(doc_svg.bounds),
                true,
            )
            .as_str(),
        ))
    }

    /// Exports the current selection as a SVG string
    pub fn export_selection_as_svg_string(
        &self,
        with_background: bool,
    ) -> anyhow::Result<Option<String>> {
        let selection_svg = match self.gen_selection_svg(with_background)? {
            Some(selection_svg) => selection_svg,
            None => return Ok(None),
        };

        Ok(Some(rnote_compose::utils::add_xml_header(
            rnote_compose::utils::wrap_svg_root(
                selection_svg.svg_data.as_str(),
                Some(selection_svg.bounds),
                Some(selection_svg.bounds),
                true,
            )
            .as_str(),
        )))
    }

    /// Exporting doc as encoded image bytes (Png / Jpg, etc.)
    pub fn export_doc_as_bitmapimage_bytes(
        &self,
        format: image::ImageOutputFormat,
        with_background: bool,
    ) -> Result<Vec<u8>, anyhow::Error> {
        let image_scale = 1.0;

        let doc_svg = self.gen_doc_svg(with_background)?;
        let doc_svg_bounds = doc_svg.bounds;

        render::Image::gen_image_from_svg(doc_svg, doc_svg_bounds, image_scale)?
            .into_encoded_bytes(format)
    }

    /// Exporting selection as encoded image bytes (Png / Jpg, etc.)
    pub fn export_selection_as_bitmapimage_bytes(
        &self,
        format: image::ImageOutputFormat,
        with_background: bool,
    ) -> Result<Option<Vec<u8>>, anyhow::Error> {
        let image_scale = 1.0;

        let selection_svg = match self.gen_selection_svg(with_background)? {
            Some(selection_svg) => selection_svg,
            None => return Ok(None),
        };
        let selection_svg_bounds = selection_svg.bounds;

        Ok(Some(
            render::Image::gen_image_from_svg(selection_svg, selection_svg_bounds, image_scale)?
                .into_encoded_bytes(format)?,
        ))
    }

    /// Exports the doc with the strokes as a Xournal++ .xopp file. Excluding the current selection.
    pub fn export_doc_as_xopp_bytes(&self, filename: &str) -> Result<Vec<u8>, anyhow::Error> {
        let current_dpi = self.document.format.dpi;

        // Only one background for all pages
        let background = xoppformat::XoppBackground {
            name: None,
            bg_type: xoppformat::XoppBackgroundType::Solid {
                color: self.document.background.color.into(),
                style: xoppformat::XoppBackgroundSolidStyle::Plain,
            },
        };

        // xopp spec needs at least one page in vec, but its fine because pages_bounds_w_content() always produces at least one
        let pages = self
            .pages_bounds_w_content()
            .iter()
            .map(|&page_bounds| {
                let page_keys = self
                    .store
                    .stroke_keys_as_rendered_intersecting_bounds(page_bounds);

                let strokes = self.store.clone_strokes(&page_keys);

                // Translate strokes to to page mins and convert to XoppStrokStyle
                let xopp_strokestyles = strokes
                    .into_iter()
                    .filter_map(|mut stroke| {
                        stroke.translate(-page_bounds.mins.coords);

                        stroke.into_xopp(current_dpi)
                    })
                    .collect::<Vec<xoppformat::XoppStrokeType>>();

                // Extract the strokes
                let xopp_strokes = xopp_strokestyles
                    .iter()
                    .filter_map(|stroke| {
                        if let xoppformat::XoppStrokeType::XoppStroke(xoppstroke) = stroke {
                            Some(xoppstroke.clone())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<xoppformat::XoppStroke>>();

                // Extract the texts
                let xopp_texts = xopp_strokestyles
                    .iter()
                    .filter_map(|stroke| {
                        if let xoppformat::XoppStrokeType::XoppText(xopptext) = stroke {
                            Some(xopptext.clone())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<xoppformat::XoppText>>();

                // Extract the images
                let xopp_images = xopp_strokestyles
                    .iter()
                    .filter_map(|stroke| {
                        if let xoppformat::XoppStrokeType::XoppImage(xoppstroke) = stroke {
                            Some(xoppstroke.clone())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<xoppformat::XoppImage>>();

                let layer = xoppformat::XoppLayer {
                    name: None,
                    strokes: xopp_strokes,
                    texts: xopp_texts,
                    images: xopp_images,
                };

                let page_dimensions = crate::utils::convert_coord_dpi(
                    page_bounds.extents(),
                    current_dpi,
                    xoppformat::XoppFile::DPI,
                );

                xoppformat::XoppPage {
                    width: page_dimensions[0],
                    height: page_dimensions[1],
                    background: background.clone(),
                    layers: vec![layer],
                }
            })
            .collect::<Vec<xoppformat::XoppPage>>();

        let title = String::from("Xournal++ document - see https://github.com/xournalpp/xournalpp (exported from Rnote - see https://github.com/flxzt/rnote)");

        let xopp_root = xoppformat::XoppRoot {
            title,
            fileversion: String::from("4"),
            preview: String::from(""),
            pages,
        };
        let xopp_file = xoppformat::XoppFile { xopp_root };

        let xoppfile_bytes = xopp_file.save_as_bytes(filename)?;

        Ok(xoppfile_bytes)
    }

    /// Exports the doc with the strokes as a PDF file.
    pub fn export_doc_as_pdf_bytes(
        &self,
        title: String,
        with_background: bool,
    ) -> oneshot::Receiver<anyhow::Result<Vec<u8>>> {
        let (oneshot_sender, oneshot_receiver) = oneshot::channel::<anyhow::Result<Vec<u8>>>();
        let doc_bounds = self.document.bounds();
        let format_size = na::vector![self.document.format.width, self.document.format.height];
        let store_snapshot = self.store.take_store_snapshot();

        let background_svg = if with_background {
            self.document
                .background
                .gen_svg(doc_bounds)
                .map_err(|e| {
                    log::error!(
                        "background.gen_svg() failed in export_doc_as_pdf_bytes() with Err {}",
                        e
                    )
                })
                .ok()
        } else {
            None
        };

        let pages_strokes = self
            .pages_bounds_w_content()
            .into_iter()
            .map(|page_bounds| {
                let strokes_in_viewport = self
                    .store
                    .stroke_keys_as_rendered_intersecting_bounds(page_bounds);

                (page_bounds, strokes_in_viewport)
            })
            .collect::<Vec<(AABB, Vec<StrokeKey>)>>();

        // Fill the pdf surface on a new thread to avoid blocking
        rayon::spawn(move || {
            let result = || -> anyhow::Result<Vec<u8>> {
                let surface =
                    cairo::PdfSurface::for_stream(format_size[0], format_size[1], Vec::<u8>::new())
                        .context("pdfsurface creation failed")?;

                surface
                    .set_metadata(cairo::PdfMetadata::Title, title.as_str())
                    .context("set pdf surface title metadata failed")?;
                surface
                    .set_metadata(
                        cairo::PdfMetadata::CreateDate,
                        crate::utils::now_formatted_string().as_str(),
                    )
                    .context("set pdf surface date metadata failed")?;

                // New scope to avoid errors when flushing
                {
                    let cairo_cx =
                        cairo::Context::new(&surface).context("cario cx new() failed")?;

                    for (i, (page_bounds, page_strokes)) in pages_strokes.into_iter().enumerate() {
                        // We can't render the background svg with piet, so we have to do it with cairo.
                        cairo_cx.save()?;
                        cairo_cx.translate(-page_bounds.mins[0], -page_bounds.mins[1]);

                        if let Some(background_svg) = background_svg.clone() {
                            render::Svg::draw_svgs_to_cairo_context(&[background_svg], &cairo_cx)?;
                        }
                        cairo_cx.restore()?;

                        // Draw the strokes with piet
                        let mut piet_cx = piet_cairo::CairoRenderContext::new(&cairo_cx);
                        piet_cx.save().map_err(|e| anyhow::anyhow!("{}", e))?;
                        piet_cx.transform(kurbo::Affine::translate(
                            -page_bounds.mins.coords.to_kurbo_vec(),
                        ));

                        for stroke in page_strokes.into_iter() {
                            if let Some(stroke) = store_snapshot.stroke_components.get(stroke) {
                                stroke.draw(&mut piet_cx, RnoteEngine::EXPORT_IMAGE_SCALE)?;
                            }
                        }

                        cairo_cx.show_page().map_err(|e| {
                            anyhow::anyhow!(
                                "show_page() failed when exporting page {} as pdf, Err {}",
                                i,
                                e
                            )
                        })?;

                        piet_cx.restore().map_err(|e| anyhow::anyhow!("{}", e))?;
                    }
                }
                let data = *surface
                    .finish_output_stream()
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "finish_outputstream() failed in export_doc_as_pdf_bytes with Err {:?}",
                            e
                        )
                    })?
                    .downcast::<Vec<u8>>()
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "downcast() finished output stream failed in export_doc_as_pdf_bytes with Err {:?}",
                            e
                        )
                    })?;

                Ok(data)
            };

            if let Err(_data) = oneshot_sender.send(result()) {
                log::error!("sending result to receiver in export_doc_as_pdf_bytes() failed. Receiver already dropped.");
            }
        });

        oneshot_receiver
    }

    /// Draws the entire engine (doc, pens, strokes, selection, ..) on a GTK snapshot.
    pub fn draw_on_snapshot(
        &self,
        snapshot: &Snapshot,
        surface_bounds: AABB,
    ) -> anyhow::Result<()> {
        let doc_bounds = self.document.bounds();
        let viewport = self.camera.viewport();

        snapshot.save();
        snapshot.transform(Some(&self.camera.transform_for_gtk_snapshot()));

        self.document.draw_shadow(snapshot);

        self.document
            .background
            .draw(snapshot, doc_bounds, &self.camera)?;

        self.document
            .format
            .draw(snapshot, doc_bounds, &self.camera)?;

        self.store
            .draw_strokes_to_snapshot(snapshot, doc_bounds, viewport);

        snapshot.restore();

        self.penholder.draw_on_doc_snapshot(
            snapshot,
            &EngineView {
                tasks_tx: self.tasks_tx(),
                doc: &self.document,
                store: &self.store,
                camera: &self.camera,
                audioplayer: &self.audioplayer,
            },
        )?;
        /*
               {
                   use crate::utils::GrapheneRectHelpers;
                   use gtk4::graphene;
                   use piet::RenderContext;
                   use rnote_compose::helpers::Affine2Helpers;

                   let zoom = self.camera.zoom();

                   let cairo_cx = snapshot.append_cairo(&graphene::Rect::from_p2d_aabb(surface_bounds));
                   let mut piet_cx = piet_cairo::CairoRenderContext::new(&cairo_cx);

                   // Transform to doc coordinate space
                   piet_cx.transform(self.camera.transform().to_kurbo());

                   piet_cx.save().map_err(|e| anyhow::anyhow!("{}", e))?;
                   self.store
                       .draw_strokes_immediate_w_piet(&mut piet_cx, doc_bounds, viewport, zoom)?;
                   piet_cx.restore().map_err(|e| anyhow::anyhow!("{}", e))?;

                   piet_cx.save().map_err(|e| anyhow::anyhow!("{}", e))?;

                   self.penholder
                       .draw_on_doc(&mut piet_cx, doc_bounds, &self.camera)?;
                   piet_cx.restore().map_err(|e| anyhow::anyhow!("{}", e))?;

                   piet_cx.finish().map_err(|e| anyhow::anyhow!("{}", e))?;
               }
        */
        snapshot.save();
        snapshot.transform(Some(&self.camera.transform_for_gtk_snapshot()));

        // visual debugging
        if self.visual_debug {
            visual_debug::draw_debug(snapshot, self, surface_bounds)?;
        }

        snapshot.restore();

        if self.visual_debug {
            visual_debug::draw_statistics_overlay(snapshot, self, surface_bounds)?;
        }

        Ok(())
    }
}

/// module for visual debugging
pub mod visual_debug {
    use gtk4::{gdk, graphene, gsk, Snapshot};
    use p2d::bounding_volume::{BoundingVolume, AABB};
    use piet::{RenderContext, Text, TextLayoutBuilder};
    use rnote_compose::helpers::Vector2Helpers;
    use rnote_compose::shapes::Rectangle;

    use crate::pens::eraser::EraserState;
    use crate::pens::penholder::PenStyle;
    use crate::utils::{GdkRGBAHelpers, GrapheneRectHelpers};
    use crate::{DrawOnDocBehaviour, RnoteEngine};
    use rnote_compose::Color;

    use super::EngineView;

    pub const COLOR_POS: Color = Color {
        r: 1.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };
    pub const COLOR_POS_ALT: Color = Color {
        r: 1.0,
        g: 1.0,
        b: 0.0,
        a: 1.0,
    };
    pub const COLOR_STROKE_HITBOX: Color = Color {
        r: 0.0,
        g: 0.8,
        b: 0.2,
        a: 0.5,
    };
    pub const COLOR_STROKE_BOUNDS: Color = Color {
        r: 0.0,
        g: 0.8,
        b: 0.8,
        a: 1.0,
    };
    pub const COLOR_IMAGE_BOUNDS: Color = Color {
        r: 0.0,
        g: 0.5,
        b: 1.0,
        a: 1.0,
    };
    pub const COLOR_STROKE_RENDERING_DIRTY: Color = Color {
        r: 0.9,
        g: 0.0,
        b: 0.8,
        a: 0.10,
    };
    pub const COLOR_STROKE_RENDERING_BUSY: Color = Color {
        r: 0.0,
        g: 0.8,
        b: 1.0,
        a: 0.10,
    };
    pub const COLOR_SELECTOR_BOUNDS: Color = Color {
        r: 1.0,
        g: 0.0,
        b: 0.8,
        a: 1.0,
    };
    pub const COLOR_DOC_BOUNDS: Color = Color {
        r: 0.8,
        g: 0.0,
        b: 0.8,
        a: 1.0,
    };

    pub fn draw_bounds(bounds: AABB, color: Color, snapshot: &Snapshot, width: f64) {
        let bounds = graphene::Rect::new(
            bounds.mins[0] as f32,
            bounds.mins[1] as f32,
            (bounds.extents()[0]) as f32,
            (bounds.extents()[1]) as f32,
        );

        let rounded_rect = gsk::RoundedRect::new(
            bounds,
            graphene::Size::zero(),
            graphene::Size::zero(),
            graphene::Size::zero(),
            graphene::Size::zero(),
        );

        snapshot.append_border(
            &rounded_rect,
            &[width as f32, width as f32, width as f32, width as f32],
            &[
                gdk::RGBA::from_compose_color(color),
                gdk::RGBA::from_compose_color(color),
                gdk::RGBA::from_compose_color(color),
                gdk::RGBA::from_compose_color(color),
            ],
        )
    }

    pub fn draw_pos(pos: na::Vector2<f64>, color: Color, snapshot: &Snapshot, width: f64) {
        snapshot.append_color(
            &gdk::RGBA::from_compose_color(color),
            &graphene::Rect::new(
                (pos[0] - 0.5 * width) as f32,
                (pos[1] - 0.5 * width) as f32,
                width as f32,
                width as f32,
            ),
        );
    }

    pub fn draw_fill(rect: AABB, color: Color, snapshot: &Snapshot) {
        snapshot.append_color(
            &gdk::RGBA::from_compose_color(color),
            &graphene::Rect::from_p2d_aabb(rect),
        );
    }

    // Draw bounds, positions, .. for visual debugging purposes
    // Expects snapshot in surface coords
    pub fn draw_statistics_overlay(
        snapshot: &Snapshot,
        engine: &RnoteEngine,
        surface_bounds: AABB,
    ) -> anyhow::Result<()> {
        // A statistics overlay
        {
            let text_bounds = AABB::new(
                na::point![
                    surface_bounds.maxs[0] - 320.0,
                    surface_bounds.mins[1] + 20.0
                ],
                na::point![
                    surface_bounds.maxs[0] - 20.0,
                    surface_bounds.mins[1] + 100.0
                ],
            );
            let cairo_cx = snapshot.append_cairo(&graphene::Rect::from_p2d_aabb(text_bounds));
            let mut piet_cx = piet_cairo::CairoRenderContext::new(&cairo_cx);

            // Gather statistics
            let strokes_total = engine.store.keys_unordered();
            let strokes_in_viewport = engine
                .store
                .keys_unordered_intersecting_bounds(engine.camera.viewport());
            let selected_strokes = engine.store.selection_keys_unordered();

            let statistics_text_string = format!(
                "strokes in store:   {}\nstrokes in current viewport:   {}\nstrokes selected: {}",
                strokes_total.len(),
                strokes_in_viewport.len(),
                selected_strokes.len()
            );

            let text_layout = piet_cx
                .text()
                .new_text_layout(statistics_text_string)
                .text_color(piet::Color::rgba(0.8, 1.0, 1.0, 1.0))
                .max_width(500.0)
                .alignment(piet::TextAlignment::End)
                .font(piet::FontFamily::MONOSPACE, 10.0)
                .build()
                .map_err(|e| anyhow::anyhow!("{}", e))?;

            piet_cx.fill(
                Rectangle::from_p2d_aabb(text_bounds).to_kurbo(),
                &piet::Color::rgba(0.1, 0.1, 0.1, 0.9),
            );

            piet_cx.draw_text(
                &text_layout,
                (text_bounds.mins.coords + na::vector![20.0, 10.0]).to_kurbo_point(),
            );
            piet_cx.finish().map_err(|e| anyhow::anyhow!("{}", e))?;
        }
        Ok(())
    }

    // Draw bounds, positions, .. for visual debugging purposes
    pub fn draw_debug(
        snapshot: &Snapshot,
        engine: &RnoteEngine,
        surface_bounds: AABB,
    ) -> anyhow::Result<()> {
        let viewport = engine.camera.viewport();
        let total_zoom = engine.camera.total_zoom();
        let doc_bounds = engine.document.bounds();
        let border_widths = 1.0 / total_zoom;

        draw_bounds(doc_bounds, COLOR_DOC_BOUNDS, snapshot, border_widths);

        let tightened_viewport = viewport.tightened(2.0 / total_zoom);
        draw_bounds(
            tightened_viewport,
            COLOR_STROKE_BOUNDS,
            snapshot,
            border_widths,
        );

        // Draw the strokes and selection
        engine.store.draw_debug(snapshot, engine, surface_bounds)?;

        // Draw the pens
        let current_pen_style = engine.penholder.current_style_w_override();

        match current_pen_style {
            PenStyle::Eraser => {
                if let EraserState::Down(current_element) = engine.penholder.eraser.state {
                    draw_pos(
                        current_element.pos,
                        COLOR_POS_ALT,
                        snapshot,
                        border_widths * 4.0,
                    );
                }
            }
            PenStyle::Selector => {
                if let Some(bounds) = engine.penholder.selector.bounds_on_doc(&EngineView {
                    tasks_tx: engine.tasks_tx(),
                    doc: &engine.document,
                    store: &engine.store,
                    camera: &engine.camera,
                    audioplayer: &engine.audioplayer,
                }) {
                    draw_bounds(bounds, COLOR_SELECTOR_BOUNDS, snapshot, border_widths);
                }
            }
            PenStyle::Brush | PenStyle::Shaper | PenStyle::Typewriter | PenStyle::Tools => {}
        }

        Ok(())
    }
}
