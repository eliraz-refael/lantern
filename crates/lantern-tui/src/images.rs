//! Image previews. Decoding happens on a background thread so scrolling past large photos stays smooth.

use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use image::{DynamicImage, ImageError};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;

/// Images are scaled down to at most this many pixels per side before display.
const MAX_SIDE: u32 = 1200;

/// Name of the decoder thread, so its panics can be told apart from the UI thread's.
const WORKER: &str = "image-decoder";

type Decoded = (PathBuf, Result<DynamicImage, String>);

pub enum Preview {
    Loading,
    Ready(StatefulProtocol),
    Failed(String),
}

/// Holds the preview for one image at a time: the one under the cursor.
pub struct Images {
    picker: Picker,
    requests: Sender<PathBuf>,
    results: Receiver<Decoded>,
    current: Option<(PathBuf, Preview)>,
}

impl Images {
    pub fn new(picker: Picker) -> Self {
        let (requests, worker_requests) = mpsc::channel::<PathBuf>();
        let (worker_results, results) = mpsc::channel::<Decoded>();
        // A decoder panic on a malformed file must not run the terminal-restoring panic hook
        // while the UI keeps going; it is caught below and shown as a failed preview instead.
        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if thread::current().name() != Some(WORKER) {
                previous_hook(info);
            }
        }));
        thread::Builder::new()
            .name(WORKER.into())
            .spawn(move || {
                while let Ok(mut path) = worker_requests.recv() {
                    // When scrolling quickly, only the latest request matters.
                    while let Ok(newer) = worker_requests.try_recv() {
                        path = newer;
                    }
                    let result = panic::catch_unwind(AssertUnwindSafe(|| decode(&path)))
                        .unwrap_or_else(|_| Err("can't preview: the decoder crashed".to_string()));
                    if worker_results.send((path, result)).is_err() {
                        break;
                    }
                }
            })
            .expect("could not start the image decoder thread");
        Self {
            picker,
            requests,
            results,
            current: None,
        }
    }

    /// How images reach the terminal. Half-blocks means the terminal didn't report image support.
    pub fn protocol_name(&self) -> &'static str {
        match self.picker.protocol_type() {
            ProtocolType::Kitty => "kitty",
            ProtocolType::Sixel => "sixel",
            ProtocolType::Iterm2 => "iterm2",
            ProtocolType::Halfblocks => "half-blocks",
        }
    }

    /// Starts loading `path` unless it's already shown or loading.
    pub fn request(&mut self, path: &Path) {
        if self.current.as_ref().is_some_and(|(p, _)| p == path) {
            return;
        }
        self.current = Some((path.to_owned(), Preview::Loading));
        // The worker only stops when `self` is dropped, so this can't fail while we're alive.
        let _ = self.requests.send(path.to_owned());
    }

    /// Whether the current preview is still being decoded.
    pub fn is_loading(&self) -> bool {
        matches!(self.current, Some((_, Preview::Loading)))
    }

    /// Picks up finished decodes. Returns true if the current preview changed.
    pub fn receive(&mut self) -> bool {
        let mut changed = false;
        while let Ok((path, result)) = self.results.try_recv() {
            let Some((current, preview)) = &mut self.current else {
                continue;
            };
            if *current != path {
                continue;
            }
            *preview = match result {
                Ok(image) => Preview::Ready(self.picker.new_resize_protocol(image)),
                Err(message) => Preview::Failed(message),
            };
            changed = true;
        }
        changed
    }

    pub fn get(&mut self, path: &Path) -> Option<&mut Preview> {
        self.current
            .as_mut()
            .filter(|(p, _)| p == path)
            .map(|(_, preview)| preview)
    }
}

fn decode(path: &Path) -> Result<DynamicImage, String> {
    lantern_core::image::load(path, MAX_SIDE).map_err(|e| match e {
        ImageError::Unsupported(_) => "no preview for this format".to_string(),
        e => format!("can't preview: {e}"),
    })
}
