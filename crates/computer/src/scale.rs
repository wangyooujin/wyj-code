//! 坐标缩放数学 — 平台无关，任意目标都会编译并测试。
//!
//! computer-use 截图以物理像素捕获，但发给模型的图片会下采样到一个更小的
//! 目标分辨率（省 token、避开供应商单图尺寸上限）。模型据此目标分辨率坐标
//! 空间返回点击坐标，必须换算回物理像素才能正确合成鼠标事件，否则点击位置
//! 会随显示器分辨率/下采样比例产生偏移。

/// 按最长边不超过 `max_dim` 等比缩放；已经足够小则原样返回（不放大）。
/// 宽或高为 0 时原样返回，避免除零。
pub fn fit_within(width: u32, height: u32, max_dim: u32) -> (u32, u32) {
    if width == 0 || height == 0 || max_dim == 0 {
        return (width, height);
    }
    let longest = width.max(height);
    if longest <= max_dim {
        return (width, height);
    }
    let scale = max_dim as f64 / longest as f64;
    let w = ((width as f64) * scale).round().max(1.0) as u32;
    let h = ((height as f64) * scale).round().max(1.0) as u32;
    (w, h)
}

/// 把一个可能越界/坐标颠倒的矩形 `[x0, y0, x1, y1)` 钳制到 `width x height`
/// 的屏幕边界内，返回 `(clamped_x0, clamped_y0, crop_width, crop_height)`；
/// 裁剪结果至少 1x1，避免退化成空矩形。`capture_region`（zoom 动作）用它
/// 决定实际要截取的物理像素范围。
pub fn clamp_region(
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    width: u32,
    height: u32,
) -> (u32, u32, u32, u32) {
    let cx0 = x0.clamp(0, width.saturating_sub(1) as i32) as u32;
    let cy0 = y0.clamp(0, height.saturating_sub(1) as i32) as u32;
    let cx1 = x1.clamp(cx0 as i32 + 1, width as i32) as u32;
    let cy1 = y1.clamp(cy0 as i32 + 1, height as i32) as u32;
    (cx0, cy0, cx1 - cx0, cy1 - cy0)
}

/// 把一个已经在"点"（逻辑坐标系，即 `Monitor::width()/height()` 这类 macOS
/// API 报告的单位）下钳制好的裁剪矩形，按 `logical_dim -> pixel_dim` 的比例
/// 换算成"像素"坐标系（即截图实际拿到的原生 `RgbaImage` 尺寸）下的裁剪矩形，
/// 并再钳一次到真实像素图边界内（防止四舍五入把裁剪框推出边界）。
///
/// **背景**：Retina/HiDPI 显示器上 macOS 的 `CGDisplayBounds`（`Monitor::
/// width()/height()`）报告的是"点"，通常是原生像素的一半（2x scale）；而
/// `CGWindowListCreateImage` 截到的图像是原生像素分辨率。点击坐标系统一
/// 用"点"（`enigo`/`CGEvent` 全局光标坐标系本就是"点"，这部分是对的，不用
/// 改），但如果直接拿"点"坐标去裁剪"像素"图，只会裁到真实区域的一部分
/// （scale_factor=2 时仅裁到 1/4 面积），造成截图内容缺失——这正是
/// `capture_region`（zoom 动作）需要这层换算的原因。
pub fn scale_region_to_pixels(
    region: (u32, u32, u32, u32),
    logical_size: (u32, u32),
    pixel_size: (u32, u32),
) -> (u32, u32, u32, u32) {
    let (cx0, cy0, crop_w, crop_h) = region;
    let (logical_width, logical_height) = logical_size;
    let (pixel_width, pixel_height) = pixel_size;
    if logical_width == 0 || logical_height == 0 || pixel_width == 0 || pixel_height == 0 {
        // 无法换算比例：原样返回，调用方对真实图像再钳一次边界，不会 panic。
        return (cx0, cy0, crop_w.max(1), crop_h.max(1));
    }
    let sx = pixel_width as f64 / logical_width as f64;
    let sy = pixel_height as f64 / logical_height as f64;
    let px_x0 = ((cx0 as f64 * sx).round() as u32).min(pixel_width.saturating_sub(1));
    let px_y0 = ((cy0 as f64 * sy).round() as u32).min(pixel_height.saturating_sub(1));
    let px_w = ((crop_w as f64 * sx).round().max(1.0) as u32)
        .min(pixel_width.saturating_sub(px_x0))
        .max(1);
    let px_h = ((crop_h as f64 * sy).round().max(1.0) as u32)
        .min(pixel_height.saturating_sub(px_y0))
        .max(1);
    (px_x0, px_y0, px_w, px_h)
}

/// 物理像素坐标 ↔ 目标（下采样后，发给模型的）坐标空间换算器。
#[derive(Debug, Clone, Copy)]
pub struct CoordScaler {
    physical_width: u32,
    physical_height: u32,
    target_width: u32,
    target_height: u32,
}

impl CoordScaler {
    pub fn new(
        physical_width: u32,
        physical_height: u32,
        target_width: u32,
        target_height: u32,
    ) -> Self {
        Self {
            physical_width,
            physical_height,
            target_width,
            target_height,
        }
    }

    /// 把模型基于目标分辨率坐标空间给出的坐标，映射回物理像素坐标（供输入
    /// 合成使用）。结果钳制在 `[0, physical_dim - 1]`，防止模型给出的越界
    /// 坐标（如目标分辨率边缘取整误差）传给系统 API 时出错或落到屏幕外。
    pub fn to_physical(&self, x: i32, y: i32) -> (i32, i32) {
        if self.target_width == 0 || self.target_height == 0 {
            return (x, y);
        }
        let sx = self.physical_width as f64 / self.target_width as f64;
        let sy = self.physical_height as f64 / self.target_height as f64;
        let px = (x as f64 * sx).round() as i32;
        let py = (y as f64 * sy).round() as i32;
        (
            px.clamp(0, self.physical_width.saturating_sub(1) as i32),
            py.clamp(0, self.physical_height.saturating_sub(1) as i32),
        )
    }

    /// `to_physical` 的逆映射：把物理像素坐标（如实际光标位置）换算回模型
    /// 所在的目标分辨率坐标空间（如 `cursor_position` 动作的返回值）。
    pub fn to_target(&self, x: i32, y: i32) -> (i32, i32) {
        if self.physical_width == 0 || self.physical_height == 0 {
            return (x, y);
        }
        let sx = self.target_width as f64 / self.physical_width as f64;
        let sy = self.target_height as f64 / self.physical_height as f64;
        let tx = (x as f64 * sx).round() as i32;
        let ty = (y as f64 * sy).round() as i32;
        (
            tx.clamp(0, self.target_width.saturating_sub(1) as i32),
            ty.clamp(0, self.target_height.saturating_sub(1) as i32),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_within_leaves_small_image_unscaled() {
        assert_eq!(fit_within(800, 600, 1280), (800, 600));
    }

    #[test]
    fn fit_within_scales_landscape_by_longest_edge() {
        // 3840x2160 (4K, 16:9) 缩到最长边 1280 -> 1280x720
        assert_eq!(fit_within(3840, 2160, 1280), (1280, 720));
    }

    #[test]
    fn fit_within_scales_portrait_by_longest_edge() {
        // 竖屏：最长边是高
        assert_eq!(fit_within(1080, 1920, 960), (540, 960));
    }

    #[test]
    fn fit_within_never_upscales() {
        assert_eq!(fit_within(400, 300, 1280), (400, 300));
    }

    #[test]
    fn fit_within_handles_zero_dims_without_panic() {
        assert_eq!(fit_within(0, 600, 1280), (0, 600));
        assert_eq!(fit_within(800, 0, 1280), (800, 0));
        assert_eq!(fit_within(800, 600, 0), (800, 600));
    }

    #[test]
    fn scale_region_to_pixels_is_identity_at_scale_factor_one() {
        // 非 Retina 外接显示器（点=像素，scale_factor=1）：坐标原样通过。
        assert_eq!(
            scale_region_to_pixels((100, 200, 300, 300), (1920, 1080), (1920, 1080)),
            (100, 200, 300, 300)
        );
    }

    #[test]
    fn scale_region_to_pixels_doubles_at_retina_scale_factor() {
        // 复现真实 bug 场景：2x Retina 屏，点分辨率 1512x982，像素分辨率
        // 3024x1964。请求"全屏"区域（点坐标系下 0,0 到 1512,982）必须换算
        // 到完整的像素图范围 3024x1964，而不是被误当成像素坐标直接使用
        // （那样只会裁到左上角 1512x982 像素，即真实区域的 1/4 面积）。
        assert_eq!(
            scale_region_to_pixels((0, 0, 1512, 982), (1512, 982), (3024, 1964)),
            (0, 0, 3024, 1964)
        );
    }

    #[test]
    fn scale_region_to_pixels_scales_a_partial_region_proportionally() {
        // 点坐标系下的一个 200x150 局部区域，起点 (100,100)，2x 缩放后应变成
        // 像素坐标系下起点 (200,200)、尺寸 400x300——而不是原样的 200x150
        // （原样使用正是 bug：只截到应有面积的 1/4）。
        assert_eq!(
            scale_region_to_pixels((100, 100, 200, 150), (1512, 982), (3024, 1964)),
            (200, 200, 400, 300)
        );
    }

    #[test]
    fn scale_region_to_pixels_clamps_rounding_overshoot_to_pixel_bounds() {
        // 起点贴着点坐标系右下边界时，四舍五入换算不能把裁剪框推出真实像素
        // 图边界（会导致 crop_imm 越界 panic）。
        let (x, y, w, h) = scale_region_to_pixels((1511, 981, 1, 1), (1512, 982), (3024, 1964));
        assert!(x + w <= 3024);
        assert!(y + h <= 1964);
    }

    #[test]
    fn scale_region_to_pixels_falls_back_to_input_when_logical_dim_is_zero() {
        // 无法换算比例（点分辨率读取失败/为 0）时原样返回，调用方对真实像素图
        // 再钳一次边界即可安全兜底，不应在这里 panic 或产生 0 宽/高矩形。
        assert_eq!(
            scale_region_to_pixels((10, 10, 50, 50), (0, 0), (3024, 1964)),
            (10, 10, 50, 50)
        );
    }

    #[test]
    fn clamp_region_passes_through_a_valid_rect() {
        assert_eq!(
            clamp_region(100, 200, 400, 500, 1920, 1080),
            (100, 200, 300, 300)
        );
    }

    #[test]
    fn clamp_region_clamps_negative_and_overflowing_corners() {
        // 左上角越界到负数、右下角越界超出屏幕
        assert_eq!(
            clamp_region(-50, -50, 3000, 3000, 1920, 1080),
            (0, 0, 1920, 1080)
        );
    }

    #[test]
    fn clamp_region_never_degenerates_to_empty_rect() {
        // 两角重合、或颠倒（x1<x0/y1<y0），仍应给出锚定在 (x0,y0) 的至少 1x1
        // 裁剪区域，不会因为反向区间导致 panic 或空矩形
        assert_eq!(
            clamp_region(500, 500, 500, 500, 1920, 1080),
            (500, 500, 1, 1)
        );
        assert_eq!(
            clamp_region(500, 500, 100, 100, 1920, 1080),
            (500, 500, 1, 1)
        );
    }

    #[test]
    fn clamp_region_handles_corner_at_screen_edge() {
        assert_eq!(
            clamp_region(1900, 1060, 1920, 1080, 1920, 1080),
            (1900, 1060, 20, 20)
        );
    }

    #[test]
    fn coord_scaler_identity_when_physical_equals_target() {
        let s = CoordScaler::new(1280, 800, 1280, 800);
        assert_eq!(s.to_physical(100, 200), (100, 200));
    }

    #[test]
    fn coord_scaler_maps_downsampled_coords_back_to_physical() {
        // 物理 3840x2160，目标 1280x720（4K 缩到 1280 长边，比例 3x）
        let s = CoordScaler::new(3840, 2160, 1280, 720);
        assert_eq!(s.to_physical(640, 360), (1920, 1080));
    }

    #[test]
    fn coord_scaler_clamps_out_of_range_coords() {
        let s = CoordScaler::new(1000, 500, 500, 250);
        // 目标坐标越界（模型给出 999,999），换算后应钳制到物理边界内
        assert_eq!(s.to_physical(999, 999), (999, 499));
        assert_eq!(s.to_physical(-10, -10), (0, 0));
    }

    #[test]
    fn coord_scaler_to_target_is_inverse_of_to_physical() {
        let s = CoordScaler::new(3840, 2160, 1280, 720);
        assert_eq!(s.to_target(1920, 1080), (640, 360));
    }

    #[test]
    fn coord_scaler_to_target_clamps_out_of_range_coords() {
        let s = CoordScaler::new(500, 250, 1000, 500);
        assert_eq!(s.to_target(999, 999), (999, 499));
        assert_eq!(s.to_target(-10, -10), (0, 0));
    }
}
