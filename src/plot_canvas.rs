use crate::theme::PLOT_BACKGROUND;
use slint::{Rgba8Pixel, SharedPixelBuffer};
use tiny_skia::{Color, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, PixmapMut, Rect, Stroke, Transform};

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
    pixels: SharedPixelBuffer<Rgba8Pixel>,
    transform: Transform,
}

impl Canvas {
    pub fn new(width: u32, height: u32, size: RasterSize) -> Self {
        let pixels = SharedPixelBuffer::new(size.width, size.height);
        let [red, green, blue] = PLOT_BACKGROUND;
        let transform = Transform::from_scale(size.width as f32 / width as f32, size.height as f32 / height as f32);
        let mut canvas = Self { pixels, transform };
        canvas.pixmap().fill(Color::from_rgba8(red, green, blue, 255));
        canvas
    }

    fn pixmap(&mut self) -> PixmapMut<'_> {
        let (width, height) = (self.pixels.width(), self.pixels.height());
        PixmapMut::from_bytes(self.pixels.make_mut_bytes(), width, height).expect("valid plot dimensions")
    }

    fn paint([red, green, blue]: [u8; 3]) -> Paint<'static> {
        let mut paint = Paint::default();
        paint.set_color_rgba8(red, green, blue, 255);
        paint.anti_alias = true;
        paint
    }

    pub fn rect(&mut self, x: f32, y: f32, width: f32, height: f32, color: [u8; 3]) {
        // Axis-aligned rectangles can use the rasterizer's fast path in pixel coordinates.
        let Transform { sx, sy, .. } = self.transform;
        if let Some(rect) = Rect::from_xywh(x * sx, y * sy, width * sx, height * sy) {
            self.pixmap().fill_rect(rect, &Self::paint(color), Transform::identity(), None);
        }
    }

    fn fill_path(&mut self, path: &Path, color: [u8; 3]) {
        let transform = self.transform;
        self.pixmap().fill_path(path, &Self::paint(color), FillRule::Winding, transform, None);
    }

    pub fn stroke(&mut self, path: &Path, color: [u8; 3], width: f32) {
        let stroke = Stroke { width, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Stroke::default() };
        let transform = self.transform;
        self.pixmap().stroke_path(path, &Self::paint(color), &stroke, transform, None);
    }

    pub fn finish(self) -> SharedPixelBuffer<Rgba8Pixel> {
        self.pixels
    }
}

pub fn app_icon(accent: [u8; 3]) -> slint::Image {
    let mut canvas = Canvas::new(64, 64, RasterSize { width: 128, height: 128 });
    canvas.pixmap().fill(Color::TRANSPARENT);
    let mut background = PathBuilder::new();
    background.move_to(17.0, 5.0);
    background.line_to(47.0, 5.0);
    background.quad_to(59.0, 5.0, 59.0, 17.0);
    background.line_to(59.0, 47.0);
    background.quad_to(59.0, 59.0, 47.0, 59.0);
    background.line_to(17.0, 59.0);
    background.quad_to(5.0, 59.0, 5.0, 47.0);
    background.line_to(5.0, 17.0);
    background.quad_to(5.0, 5.0, 17.0, 5.0);
    background.close();
    let background = background.finish().unwrap();
    canvas.fill_path(&background, PLOT_BACKGROUND);
    canvas.stroke(&background, crate::theme::PLOT_GRID, 1.0);

    let mut waveform = PathBuilder::new();
    waveform.move_to(13.0, 36.0);
    waveform.cubic_to(17.0, 28.0, 20.0, 22.0, 25.0, 22.0);
    waveform.cubic_to(32.0, 22.0, 35.0, 42.0, 42.0, 42.0);
    waveform.cubic_to(47.0, 42.0, 49.0, 33.0, 53.0, 33.0);
    canvas.stroke(&waveform.finish().unwrap(), accent, 2.8);
    for (x, y) in [(13.0, 36.0), (25.0, 22.0), (42.0, 42.0)] {
        let node = PathBuilder::from_circle(x, y, 2.7).unwrap();
        canvas.fill_path(&node, [242, 242, 244]);
    }
    slint::Image::from_rgba8_premultiplied(canvas.finish())
}
