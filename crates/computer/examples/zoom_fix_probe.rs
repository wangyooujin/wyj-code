//! 临时真机验证脚本（非交付物）：验证 capture_region（zoom 动作的底层实现）
//! 在 Retina/HiDPI 屏幕上的"点坐标 -> 像素坐标"换算修复是否生效。只检查
//! 返回的 Capture 尺寸元数据（不查看/不落盘实际截图内容，避免不必要地
//! 读取屏幕真实画面）。
//! 用法：cargo run -p wyj-computer --example zoom_fix_probe

fn main() -> anyhow::Result<()> {
    let logical = wyj_computer::primary_display_size()?;
    println!(
        "primary_display_size()（点坐标系，用于点击）: {}x{}",
        logical.physical_width, logical.physical_height
    );

    // 全屏截图（max_dim 给一个足够大的值，避免下采样掩盖真实像素尺寸）。
    let full = wyj_computer::capture_primary(100_000)?;
    println!(
        "capture_primary（应为原生像素分辨率）: physical={}x{} target={}x{}",
        full.physical_width, full.physical_height, full.target_width, full.target_height
    );

    // 用 capture_region 请求"整个点坐标系范围"的区域（等价于 zoom 一整屏），
    // max_dim 同样给大，避免下采样掩盖真实裁剪结果。修复前：这里会因为直接
    // 拿"点"坐标当"像素"坐标裁剪，只裁到 full 的 1/4 面积（2x Retina 下）；
    // 修复后：裁剪结果应和 full 的像素尺寸一致（因为请求的就是整个屏幕）。
    let zoomed = wyj_computer::capture_region(
        0,
        0,
        logical.physical_width as i32,
        logical.physical_height as i32,
        100_000,
    )?;
    println!(
        "capture_region(整屏范围)（应与上面 capture_primary 的像素尺寸一致): physical={}x{} target={}x{}",
        zoomed.physical_width, zoomed.physical_height, zoomed.target_width, zoomed.target_height
    );

    let scale_factor = full.physical_width as f64 / logical.physical_width as f64;
    println!("推算 Retina scale_factor: {scale_factor:.2}x");

    if zoomed.physical_width == full.physical_width
        && zoomed.physical_height == full.physical_height
    {
        println!(
            "=> 修复生效：capture_region 请求整屏时拿到了完整的原生像素分辨率，没有丢失内容。"
        );
    } else {
        let area_ratio = (zoomed.physical_width as f64 * zoomed.physical_height as f64)
            / (full.physical_width as f64 * full.physical_height as f64);
        println!(
            "=> 仍有偏差：capture_region 实际拿到的面积只有全屏的 {:.1}%（应接近 100%）。",
            area_ratio * 100.0
        );
    }

    Ok(())
}
