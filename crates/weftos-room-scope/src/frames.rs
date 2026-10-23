//! Camera frames: JPEGs pre-extracted by the preprocessing (the app never decodes video). A worker
//! thread decodes the most recently requested frame; stale requests are skipped.

use eframe::egui::{self, ColorImage};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::model::FramesIndex;

pub struct FrameSource {
    pub dir: PathBuf,
    pub index: FramesIndex,
    req: Sender<(usize, PathBuf)>,
    rx: Receiver<(usize, Result<ColorImage, String>)>,
    pub shown: Option<usize>,
    pending: Option<usize>,
    pub texture: Option<egui::TextureHandle>,
    pub error: Option<String>,
}

pub fn decode_jpeg(path: &std::path::Path) -> Result<ColorImage, String> {
    let img = image::open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .to_rgb8();
    let (w, h) = img.dimensions();
    Ok(ColorImage::from_rgb([w as usize, h as usize], img.as_raw()))
}

impl FrameSource {
    pub fn new(dir: PathBuf, index: FramesIndex, ctx: &egui::Context) -> Self {
        let (req, req_rx) = channel::<(usize, PathBuf)>();
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            while let Ok(mut job) = req_rx.recv() {
                // only the newest request matters while scrubbing
                while let Ok(newer) = req_rx.try_recv() {
                    job = newer;
                }
                let out = decode_jpeg(&job.1);
                if tx.send((job.0, out)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Self {
            dir,
            index,
            req,
            rx,
            shown: None,
            pending: None,
            texture: None,
            error: None,
        }
    }

    /// Ask for the frame at video time `t`; upload any finished decode to the texture.
    pub fn want(&mut self, ctx: &egui::Context, t: f64) {
        while let Ok((i, res)) = self.rx.try_recv() {
            match res {
                Ok(img) => {
                    match &mut self.texture {
                        Some(tex) => tex.set(img, egui::TextureOptions::LINEAR),
                        None => {
                            self.texture = Some(ctx.load_texture(
                                "room-scope-frame",
                                img,
                                egui::TextureOptions::LINEAR,
                            ))
                        }
                    }
                    self.shown = Some(i);
                    self.error = None;
                }
                Err(e) => self.error = Some(e),
            }
            if self.pending == Some(i) {
                self.pending = None;
            }
        }
        let i = self.index.index_at(t);
        if self.shown != Some(i) && self.pending != Some(i) {
            self.pending = Some(i);
            let _ = self.req.send((i, self.dir.join(self.index.file_name(i))));
        }
    }
}
