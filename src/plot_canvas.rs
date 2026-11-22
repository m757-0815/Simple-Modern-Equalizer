use crate::theme::PLOT_BACKGROUND;
use slint::{Rgba8Pixel, SharedPixelBuffer};
use tiny_skia::{Color, FillRule, LineCap, LineJoin, Paint, Path, Pixmap, Rect, Stroke, Transform};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RasterSize {
    pub width: u32,
    pub height: u32,
}

impl RasterSize {
    pub fn from_logical(width: f32, height: f32, scale: f32) -> Option<Self> {
        if ![width, height, scale].into_iter().all(|value| value.is_finite() && value > 0.0) {
            return None;
        }
        let width = f64::from(width) * f64::from(scale);
        let height = f64::from(height) * f64::from(scale);
        // Keep pathological sizes bounded without changing the plot's aspect ratio.
        let shrink = (4_194_304.0 / (width * height)).sqrt()
            .min(8192.0 / width.max(height)).min(1.0);
        Some(Self {
            width: (width * shrink).round().max(1.0) as u32,
            height: (height * shrink).round().max(1.0) as u32,
        })
    }
}

pub struct Canvas {
    pixels: Pixmap,
    transform: Transform,
}

impl Canvas {
    pub fn new(width: u32, height: u32, size: RasterSize) -> Self {
        let mut pixels = Pixmap::new(size.width, size.height).expect("valid plot dimensions");
        let [red, green, blue] = PLOT_BACKGROUND;
        pixels.fill(Color::from_rgba8(red, green, blue, 255));
        let transform = Transform::from_scale(size.width as f32 / width as f32, size.height as f32 / height as f32);
        Self { pixels, transform }
    }

    fn paint([red, green, blue]: [u8; 3]) -> Paint<'static> {
        let mut paint = Paint::default();
        paint.set_color_rgba8(red, green, blue, 255);
        paint.anti_alias = true;
        paint
    }

    pub fn rect(&mut self, x: f32, y: f32, width: f32, height: f32, color: [u8; 3]) {
        if let Some(rect) = Rect::from_xywh(x, y, width, height) {
            self.pixels.fill_rect(rect, &Self::paint(color), self.transform, None);
        }
    }

    pub fn fill(&mut self, path: &Path, color: [u8; 3]) {
        self.pixels.fill_path(path, &Self::paint(color), FillRule::Winding, self.transform, None);
    }

    pub fn stroke(&mut self, path: &Path, color: [u8; 3], width: f32) {
        let stroke = Stroke { width, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Stroke::default() };
        self.pixels.stroke_path(path, &Self::paint(color), &stroke, self.transform, None);
    }

    pub fn finish(self) -> SharedPixelBuffer<Rgba8Pixel> {
        let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(self.pixels.width(), self.pixels.height());
        buffer.make_mut_bytes().copy_from_slice(self.pixels.data());
        buffer
    }
}
