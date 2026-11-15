//! Attention-based crop placement, modeled on libvips' `attention` strategy:
//! score edges, saturated color and skin tones, then keep the crop window
//! that captures the most interest.

use super::{Layout, Raster};

/// Longest side of the analysis grid. Interest maps are coarse by design.
const ANALYSIS_SIZE: u32 = 128;
const EDGE_WEIGHT: f32 = 1.0;
const SATURATION_WEIGHT: f32 = 0.6;
const SKIN_WEIGHT: f32 = 1.5;

/// Returns the top-left offset of a `crop_width` x `crop_height` window.
pub(super) fn attention_offset(image: &Raster, crop_width: u32, crop_height: u32) -> (u32, u32) {
    let max_left = image.width.saturating_sub(crop_width);
    let max_top = image.height.saturating_sub(crop_height);
    if max_left == 0 && max_top == 0 {
        return (0, 0);
    }

    let cell = image.width.max(image.height).div_ceil(ANALYSIS_SIZE).max(1);
    let grid = Grid::sample(image, cell);
    let interest = grid.interest();
    let integral = Integral::new(&interest, grid.width, grid.height);

    // Window size and travel in grid cells.
    let window_width = (crop_width as f32 / cell as f32)
        .round()
        .clamp(1.0, grid.width as f32) as usize;
    let window_height = (crop_height as f32 / cell as f32)
        .round()
        .clamp(1.0, grid.height as f32) as usize;
    let steps_x = grid.width - window_width;
    let steps_y = grid.height - window_height;

    // Prefer the most interesting window; break ties toward the center.
    let center_x = steps_x as f32 / 2.0;
    let center_y = steps_y as f32 / 2.0;
    let mut best = (f32::MIN, f32::MAX, 0usize, 0usize);
    for y in 0..=steps_y {
        for x in 0..=steps_x {
            let score = integral.sum(x, y, window_width, window_height);
            let distance = (x as f32 - center_x).abs() + (y as f32 - center_y).abs();
            if score > best.0 + f32::EPSILON
                || ((score - best.0).abs() <= f32::EPSILON && distance < best.1)
            {
                best = (score, distance, x, y);
            }
        }
    }

    if best.0 <= f32::EPSILON {
        // Nothing stands out; fall back to a center crop.
        return (max_left / 2, max_top / 2);
    }

    let to_pixels = |cells: usize, travel: usize, max: u32| -> u32 {
        if travel == 0 {
            return max / 2;
        }
        // Map the grid position onto the full-resolution travel range.
        let fraction = cells as f64 / travel as f64;
        ((f64::from(max) * fraction).round() as u32).min(max)
    };
    (
        to_pixels(best.2, steps_x, max_left),
        to_pixels(best.3, steps_y, max_top),
    )
}

struct Grid {
    width: usize,
    height: usize,
    /// Mean premultiplied RGB and alpha per cell, 0..=255.
    cells: Vec<[f32; 4]>,
}

impl Grid {
    fn sample(image: &Raster, cell: u32) -> Self {
        let width = image.width.div_ceil(cell) as usize;
        let height = image.height.div_ceil(cell) as usize;
        let channels = image.layout.channels();
        let mut sums = vec![[0f32; 5]; width * height];
        for y in 0..image.height as usize {
            let row = &image.data[y * image.row_bytes()..(y + 1) * image.row_bytes()];
            let grid_row = (y / cell as usize) * width;
            for (x, pixel) in row.chunks_exact(channels).enumerate() {
                let alpha = if image.layout == Layout::Rgba {
                    f32::from(pixel[3])
                } else {
                    255.0
                };
                let weight = alpha / 255.0;
                let sum = &mut sums[grid_row + x / cell as usize];
                sum[0] += f32::from(pixel[0]) * weight;
                sum[1] += f32::from(pixel[1]) * weight;
                sum[2] += f32::from(pixel[2]) * weight;
                sum[3] += alpha;
                sum[4] += 1.0;
            }
        }
        let cells = sums
            .into_iter()
            .map(|[r, g, b, a, n]| {
                let n = n.max(1.0);
                [r / n, g / n, b / n, a / n]
            })
            .collect();
        Self {
            width,
            height,
            cells,
        }
    }

    fn interest(&self) -> Vec<f32> {
        let luma: Vec<f32> = self
            .cells
            .iter()
            .map(|[r, g, b, _]| 0.299 * r + 0.587 * g + 0.114 * b)
            .collect();
        let at = |x: usize, y: usize| luma[y * self.width + x];

        let mut interest = Vec::with_capacity(self.cells.len());
        for y in 0..self.height {
            for x in 0..self.width {
                let center = at(x, y);
                let left = at(x.saturating_sub(1), y);
                let right = at((x + 1).min(self.width - 1), y);
                let up = at(x, y.saturating_sub(1));
                let down = at(x, (y + 1).min(self.height - 1));
                let edge = (4.0 * center - left - right - up - down).abs() / 255.0;

                let [r, g, b, alpha] = self.cells[y * self.width + x];
                let visible = alpha / 255.0;
                let (r, g, b) = if visible > 0.0 {
                    (r / visible, g / visible, b / visible)
                } else {
                    (0.0, 0.0, 0.0)
                };
                let max = r.max(g).max(b);
                let min = r.min(g).min(b);
                let saturation = if max > 0.0 { (max - min) / max } else { 0.0 };
                // Only strongly saturated color draws attention, as in libvips.
                let saturation = ((saturation - 0.25) / 0.75).max(0.0) * (max / 255.0);
                let skin = if is_skin(r, g, b) { 1.0 } else { 0.0 };

                interest.push(
                    visible
                        * (EDGE_WEIGHT * edge
                            + SATURATION_WEIGHT * saturation
                            + SKIN_WEIGHT * skin),
                );
            }
        }
        interest
    }
}

/// A conservative RGB skin-tone classifier (Kovac et al.).
fn is_skin(r: f32, g: f32, b: f32) -> bool {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    r > 95.0 && g > 40.0 && b > 20.0 && max - min > 15.0 && (r - g).abs() > 15.0 && r > g && r > b
}

/// Summed-area table for constant-time window sums.
struct Integral {
    width: usize,
    sums: Vec<f32>,
}

impl Integral {
    fn new(values: &[f32], width: usize, height: usize) -> Self {
        let stride = width + 1;
        let mut sums = vec![0f32; stride * (height + 1)];
        for y in 0..height {
            let mut row = 0f32;
            for x in 0..width {
                row += values[y * width + x];
                sums[(y + 1) * stride + x + 1] = sums[y * stride + x + 1] + row;
            }
        }
        Self { width, sums }
    }

    fn sum(&self, x: usize, y: usize, width: usize, height: usize) -> f32 {
        let stride = self.width + 1;
        let at = |x: usize, y: usize| self.sums[y * stride + x];
        at(x + width, y + height) - at(x, y + height) - at(x + width, y) + at(x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_with_patch(width: u32, height: u32, patch_left: u32, patch_width: u32) -> Raster {
        let mut data = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                if (patch_left..patch_left + patch_width).contains(&x) {
                    // High-contrast, saturated checkerboard.
                    if (x / 2 + y / 2) % 2 == 0 {
                        data.extend_from_slice(&[255, 0, 0]);
                    } else {
                        data.extend_from_slice(&[0, 0, 255]);
                    }
                } else {
                    data.extend_from_slice(&[128, 128, 128]);
                }
            }
        }
        Raster::new(width, height, Layout::Rgb, data).expect("raster")
    }

    #[test]
    fn keeps_the_detailed_region_in_frame() {
        let image = flat_with_patch(200, 50, 150, 40);
        let (left, top) = attention_offset(&image, 50, 50);
        assert_eq!(top, 0);
        assert!((140..=150).contains(&left), "left = {left}");
    }

    #[test]
    fn centers_featureless_images() {
        let image = flat_with_patch(200, 50, 0, 0);
        assert_eq!(attention_offset(&image, 50, 50), (75, 0));
    }

    #[test]
    fn ignores_detail_hidden_by_transparency() {
        let mut data = Vec::new();
        for y in 0..50u32 {
            for x in 0..200u32 {
                let checker = if (x / 2 + y / 2) % 2 == 0 { 255 } else { 0 };
                let alpha = if x >= 150 { 0 } else { 255 };
                let visible_detail = (20..60).contains(&x);
                if visible_detail || alpha == 0 {
                    data.extend_from_slice(&[checker, 0, 255 - checker, alpha]);
                } else {
                    data.extend_from_slice(&[128, 128, 128, 255]);
                }
            }
        }
        let image = Raster::new(200, 50, Layout::Rgba, data).expect("raster");
        let (left, _) = attention_offset(&image, 50, 50);
        assert!(left <= 20, "left = {left}");
    }
}
