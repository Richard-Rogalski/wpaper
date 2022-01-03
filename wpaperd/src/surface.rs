use std::cell::Cell;
use std::io::{BufWriter, Write};
use std::os::unix::prelude::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use color_eyre::Result;
use dowser::Dowser;
use image::imageops::FilterType;
use image::open;
use smithay_client_toolkit::{
    output::OutputInfo,
    reexports::{
        client::protocol::{wl_output, wl_shm, wl_surface},
        client::{Attached, Main},
        protocols::wlr::unstable::layer_shell::v1::client::{
            zwlr_layer_shell_v1, zwlr_layer_surface_v1,
        },
    },
    shm::AutoMemPool,
};

use crate::output::Output;
use crate::output_timer::OutputTimer;

#[derive(PartialEq, Copy, Clone)]
enum RenderEvent {
    Configure { width: u32, height: u32 },
    Closed,
}

pub struct Surface {
    surface: wl_surface::WlSurface,
    layer_surface: Main<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    next_render_event: Rc<Cell<Option<RenderEvent>>>,
    pub info: OutputInfo,
    pool: AutoMemPool,
    dimensions: (u32, u32),
    output: Arc<Output>,
    need_redraw: bool,
    pub timer: Arc<Mutex<OutputTimer>>,
}

impl Surface {
    pub fn new(
        wl_output: &wl_output::WlOutput,
        surface: wl_surface::WlSurface,
        layer_shell: &Attached<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
        info: OutputInfo,
        pool: AutoMemPool,
        output: Arc<Output>,
    ) -> Self {
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            Some(wl_output),
            zwlr_layer_shell_v1::Layer::Background,
            "example".to_owned(),
        );

        layer_surface.set_size(0, 0);
        layer_surface.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right
                | zwlr_layer_surface_v1::Anchor::Bottom,
        );
        layer_surface.set_exclusive_zone(-1);

        let next_render_event = Rc::new(Cell::new(None::<RenderEvent>));
        let next_render_event_handle = Rc::clone(&next_render_event);
        layer_surface.quick_assign(move |layer_surface, event, _| {
            match (event, next_render_event_handle.get()) {
                (zwlr_layer_surface_v1::Event::Closed, _) => {
                    next_render_event_handle.set(Some(RenderEvent::Closed));
                }
                (
                    zwlr_layer_surface_v1::Event::Configure {
                        serial,
                        width,
                        height,
                    },
                    next,
                ) if next != Some(RenderEvent::Closed) => {
                    layer_surface.ack_configure(serial);
                    next_render_event_handle.set(Some(RenderEvent::Configure { width, height }));
                }
                (_, _) => {}
            }
        });

        // Commit so that the server will send a configure event
        surface.commit();

        Self {
            surface,
            layer_surface,
            next_render_event,
            info,
            pool,
            dimensions: (0, 0),
            need_redraw: false,
            output: output.clone(),
            timer: Arc::new(Mutex::new(OutputTimer::new(output))),
        }
    }

    /// Handles any events that have occurred since the last call, redrawing if needed.
    /// Returns true if the surface should be dropped.
    pub fn handle_events(&mut self) -> bool {
        match self.next_render_event.take() {
            Some(RenderEvent::Closed) => true,
            Some(RenderEvent::Configure { width, height }) => {
                self.dimensions = (width, height);
                self.need_redraw = true;
                false
            }
            None => false,
        }
    }

    pub fn draw(&mut self) -> Result<Option<u32>> {
        {
            let mut output_timer = self.timer.lock().unwrap();
            if !(self.need_redraw || output_timer.expired) || self.dimensions.0 == 0 {
                return Ok(None);
            }
            output_timer.expired = false;
            self.need_redraw = false;
        }

        let path = self.output.path.as_ref().unwrap();

        let stride = 4 * self.dimensions.0 as i32;
        let width = self.dimensions.0 as i32;
        let height = self.dimensions.1 as i32;

        self.pool.resize((stride * height) as usize).unwrap();

        let (canvas, buffer) = self
            .pool
            .buffer(width, height, stride, wl_shm::Format::Abgr8888)
            .unwrap();

        let img_path = if path.is_dir() {
            let files = Vec::<PathBuf>::try_from(
                Dowser::filtered(|p: &Path| {
                    p.extension()
                        .map_or(false, |e| e.as_bytes().eq_ignore_ascii_case(b"jpg"))
                })
                .with_path(path),
            )
            .unwrap();
            files[rand::random::<usize>() % files.len()].clone()
        } else {
            path.to_path_buf()
        };

        let image = open(img_path).unwrap();
        let image = image
            .resize_to_fill(
                width.try_into().unwrap(),
                height.try_into().unwrap(),
                FilterType::Lanczos3,
            )
            .into_rgba8();

        let mut writer = BufWriter::new(canvas);
        writer.write_all(image.as_raw()).unwrap();
        writer.flush().unwrap();

        // Attach the buffer to the surface and mark the entire surface as damaged
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface
            .damage_buffer(0, 0, width as i32, height as i32);

        // Finally, commit the surface
        self.surface.commit();

        Ok(self.output.time)
    }

    pub fn update_output(&mut self, output: Arc<Output>) {
        self.output = output;
        self.timer
            .lock()
            .unwrap()
            .update_output(self.output.clone());

        self.need_redraw = true;
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.layer_surface.destroy();
        self.surface.destroy();
    }
}