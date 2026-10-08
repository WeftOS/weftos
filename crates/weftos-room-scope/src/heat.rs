//! Naive link-weighted heat on the camera image: a Gaussian band along each router->spot link, weighted
//! by the summed max(z, 0) of that spot's ungated nodes. Not a localisation.

use eframe::egui::{self, Color32};

use crate::model::{self, Csi, IMG_H, IMG_W, Meta};
use crate::view::Proj;

/// Band weights per spot for each sample of a grid of image px (inside the room only).
pub struct HeatGrid {
    pub w: usize,
    pub h: usize,
    pub bands: Vec<(String, Vec<f32>)>,
    pub inside: Vec<bool>,
}

impl HeatGrid {
    pub fn new(pr: &Proj, meta: &Meta, sigma: f64) -> Self {
        let (w, h) = (160usize, 90usize);
        let router = meta.router().unwrap_or([0.0, 0.0]);
        let spots = meta.spots();
        let [rx, ry] = meta.room_max;
        let mut inside = vec![false; w * h];
        let mut bands: Vec<(String, Vec<f32>)> = spots
            .iter()
            .map(|(id, _, _)| (id.clone(), vec![0.0; w * h]))
            .collect();
        for j in 0..h {
            for i in 0..w {
                let px = [
                    (i as f64 + 0.5) * IMG_W / w as f64,
                    (j as f64 + 0.5) * IMG_H / h as f64,
                ];
                let (x, y, ww) = model::apply(&pr.h, px[0], px[1]);
                let k = j * w + i;
                if ww <= 0.0 || !(0.0..=rx).contains(&x) || !(0.0..=ry).contains(&y) {
                    continue;
                }
                inside[k] = true;
                for (bi, (_, pos, _)) in spots.iter().enumerate() {
                    let d = model::seg_dist([x, y], router, *pos);
                    bands[bi].1[k] = (-d * d / (2.0 * sigma * sigma)).exp() as f32;
                }
            }
        }
        Self {
            w,
            h,
            bands,
            inside,
        }
    }

    /// Naive link-weighted heat: per spot, sum of max(z, 0) over its ungated nodes times the band.
    pub fn image(&self, meta: &Meta, csi: &Csi, k: usize) -> egui::ColorImage {
        let weights: Vec<f32> = meta
            .spots()
            .iter()
            .map(|(_, _, ids)| {
                ids.iter()
                    .filter_map(|n| csi.live_z(n, k))
                    .map(|z| z.max(0.0) as f32)
                    .sum()
            })
            .collect();
        let mut img = egui::ColorImage::filled([self.w, self.h], Color32::TRANSPARENT);
        for (i, px) in img.pixels.iter_mut().enumerate() {
            if !self.inside[i] {
                continue;
            }
            let heat: f32 = self
                .bands
                .iter()
                .zip(&weights)
                .map(|((_, b), w)| b[i] * w)
                .sum();
            let u = (heat / 8.0).min(1.0);
            *px = Color32::from_rgba_unmultiplied(
                255,
                (200.0 * (1.0 - u)) as u8,
                40,
                (150.0 * u) as u8,
            );
        }
        img
    }
}
