//! 系统 OCR：把一张截图读成带坐标的文本行。
//!
//! # 为什么需要它
//!
//! 元素树（[`crate::element`]）是首选，但对**原生自绘应用**必然失效：微信
//! v4.1.15 实测 5×5 网格 hit-test 25/25 返回 `kAXErrorNotImplemented`，
//! canvas 渲染的图表、视频字幕、图片里的文字同理——这些内容在系统里**根本
//! 没有文本对象**，只能从像素里认。
//!
//! 社区实测数据也支持这个分层：Screen2AX（arXiv 2507.16704，2025-07）统计
//! 99 个主流 macOS 应用中只有 36% 提供高质量 AX 元数据，18% 完全没有。
//! 所以「AX 优先 + 视觉兜底」不是退让，是必要结构。
//!
//! # 为什么是精确档 + 显式触发
//!
//! 实测（本机 macOS 27.0.1，3840×2160 截屏）：
//!
//! | 档位 | 语言 | 行数 | 耗时（热态） | 平均置信度 |
//! |---|---|---|---|---|
//! | accurate | zh-Hans, en-US | 114 | **0.91s** | 0.48 |
//! | fast | zh-Hans, en-US | 131 | 0.31s | 0.45（大量乱码） |
//! | accurate | 1470×832 | 116 | 0.91s | — |
//! | fast | 1470×832 | **26** | 0.08s | —（漏检 4/5） |
//!
//! 两条结论：① `fast` 在缩放后的图上漏检严重，不能当默认档；② `accurate`
//! 有约 0.9s 的**固定底价**，与图像大小几乎无关。因此它绝不可能是「每次
//! 动作后自动跑」的东西——那会让每一步都多付一秒。工具层把它做成**显式
//! action**，由模型自己判断「我现在需要读字」。
//!
//! 同一个表还说明：**不要**为了 OCR 去缩放截图。1280 宽下的识别质量明显
//! 差于原始像素（`Fille`、`seripts`、`pubLish` 这类错字），而耗时不变。
//! 调用方应传原始像素图。
//!
//! # 冷启动
//!
//! 独立进程首次调用要 25 秒（加载识别模型）。因此本模块提供
//! [`warm_up`]，由工具注册点调一次——与 `accessibility::set_messaging_timeout`
//! 同一模式：进程级初始化只做一次，之后每次识别都是热态 0.9s。

use anyhow::{anyhow, Result};
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
};
use serde::Serialize;

/// 一次识别最多返回多少行。
///
/// 全屏 4K 实测 114 行，普通窗口 20~60 行。上限的作用是防止一张密集表格
/// 把上下文灌满：200 行 × 60 字符 ≈ 12KB，已经是单次工具结果的上限量级。
pub const MAX_LINES: usize = 200;

/// 一行识别结果。
#[derive(Debug, Clone, Serialize)]
pub struct RecognizedLine {
    /// 识别出的文本（已 trim，空行不返回）。
    pub text: String,
    /// Vision 给的置信度，0~1。实测中文常在 0.3~0.5 区间，**不能当成事实**。
    pub confidence: f32,
    /// 在**输入图像像素坐标**里的中心点（左上原点，与截图同向）。
    pub center: (i32, i32),
    /// 在输入图像像素坐标里的外接矩形 `[x, y, width, height]`（左上原点）。
    pub rect: (i32, i32, i32, i32),
}

/// 一次识别的结果。
#[derive(Debug, Clone, Serialize)]
pub struct OcrResult {
    /// 输入图像的像素尺寸。
    pub image_width: u32,
    pub image_height: u32,
    pub lines: Vec<RecognizedLine>,
    /// 识别出的总行数超过 [`MAX_LINES`] 而被截断。
    pub truncated: bool,
    /// 实际耗时（毫秒），供模型判断是否值得再来一次。
    pub elapsed_ms: u64,
}

/// 对一张 PNG 做文本识别。
///
/// 传 PNG 字节而不是 `CGImage`：上游（`xcap`）拿到的就是 PNG 编码后的缓冲，
/// 而 Vision 的 `initWithData:` 支持任何 ImageIO 能解的格式，省掉一次
/// 位图重建，也避免手工拼 `CGBitmapInfo`（那是视网膜屏上最容易错的一步）。
///
/// # 坐标系
///
/// Vision 的 `boundingBox` 是**归一化 + 左下原点**，与本 crate 其余部分
/// （左上原点、像素）不同。转换在 [`to_pixel_rect`] 里一次性做完并单测覆盖，
/// 调用方拿到的一律是图像像素坐标。
pub fn recognize_png(png: &[u8]) -> Result<OcrResult> {
    if png.is_empty() {
        return Err(anyhow!("ocr: empty image"));
    }
    let started = std::time::Instant::now();

    let data = NSData::with_bytes(png);
    let options = NSDictionary::new();
    // `initWithData:` 是安全方法（objc2 只把它标成非 unsafe，因为数据格式由
    // ImageIO 决定、失败时请求会报错而不是 UB）。
    let handler = VNImageRequestHandler::initWithData_options(
        VNImageRequestHandler::alloc(),
        &data,
        &options,
    );

    // SAFETY: `init` 创建一个不带 completion handler 的请求（这是文档里的
    // 标准用法），返回值由 `Retained` 管理。
    let request = unsafe { VNRecognizeTextRequest::init(VNRecognizeTextRequest::alloc()) };
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    // 语言顺序即优先级。中文放前面、英文兜底覆盖代码与 URL；同时打开自动
    // 检测，让纯英文界面（如终端）不被中文模型拖累。
    //
    // 实测：只给 `en-US` 时中文行会退化成形近英文（「标普」→「500ETF」丢失），
    // 只给 `zh-Hans` 时英文单词错误率上升。两者都给是最稳的形态。
    let languages =
        NSArray::from_retained_slice(&[NSString::from_str("zh-Hans"), NSString::from_str("en-US")]);
    request.setRecognitionLanguages(&languages);
    // 语言纠正在本机实测对代码/数字有害（会把 `pubLish` 纠正成 `publish` 之外
    // 的东西，也会把行情数字改成形近字），关掉。
    request.setUsesLanguageCorrection(false);

    // `performRequests` 要的是 `NSArray<VNRequest>`，而 `VNRecognizeTextRequest`
    // 是它的两级子类，必须先 `into_super()` 两次再装箱。
    let requests: Retained<NSArray<VNRequest>> =
        NSArray::from_retained_slice(&[request.clone().into_super().into_super()]);
    handler
        .performRequests_error(&requests)
        .map_err(|error| anyhow!("ocr: Vision request failed: {error}"))?;

    let observations = request
        .results()
        .ok_or_else(|| anyhow!("ocr: Vision returned no observations"))?;

    // 图像尺寸从观察到的归一化框反推不可靠（无文本时无从得知），因此由调用方
    // 传入的那一侧已知——这里用 PNG 头部解析。
    let (image_width, image_height) =
        png_dimensions(png).ok_or_else(|| anyhow!("ocr: could not read the PNG dimensions"))?;

    let mut lines = Vec::new();
    let mut truncated = false;
    for observation in observations.iter() {
        let Some(best) = observation.topCandidates(1).iter().next() else {
            continue;
        };
        let text = best.string().to_string();
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if lines.len() >= MAX_LINES {
            truncated = true;
            break;
        }
        let confidence = best.confidence();
        // SAFETY: `boundingBox` 是只读属性。
        let bounding_box = unsafe { observation.boundingBox() };
        let rect = to_pixel_rect(bounding_box, image_width, image_height);
        lines.push(RecognizedLine {
            text: text.to_string(),
            confidence,
            center: (rect.0 + rect.2 / 2, rect.1 + rect.3 / 2),
            rect,
        });
    }

    Ok(OcrResult {
        image_width,
        image_height,
        lines,
        truncated,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// 把 Vision 的归一化矩形（原点左下）换算成图像像素矩形（原点左上）。
///
/// 三个必须同时对的点：
///
/// 1. **翻转 y**：Vision 用左下原点，本 crate 其余部分（`xcap` 截图、AX 坐标）
///    都是左上原点。不翻转会让每一行都上下镜像，模型据此点击会点到反方向的
///    位置——这类错误看起来像「点歪了」，很难追到坐标系。
/// 2. **用 `maxY` 算上边界**：`boundingBox` 的 `minY` 是**距底边**的距离，
///    `maxY` 才是距顶边的距离（翻转后）。
/// 3. **宽高按比例缩放到像素**：归一化值是 0~1 的比例，与分辨率无关。
///
/// 返回值 `(x, y, width, height)`，全部为像素整数，越界已钳制。
fn to_pixel_rect(bounding_box: CGRect, width: u32, height: u32) -> (i32, i32, i32, i32) {
    let w = f64::from(width);
    let h = f64::from(height);
    let CGPoint { x, y } = bounding_box.origin;
    let CGSize {
        width: bw,
        height: bh,
    } = bounding_box.size;
    let left = x * w;
    let top = (1.0 - (y + bh)) * h;
    let right = (x + bw) * w;
    let bottom = (1.0 - y) * h;
    // 四舍五入而不是截断：`(1.0 - 0.9) * 500` 在浮点下是 49.999…，截断会
    // 让整块文本少一个像素——累积到 `center` 上就是模型点偏。先 round 再
    // clamp，两个方向都不会越界。
    let clamp = |value: f64, limit: f64| -> i32 {
        if !value.is_finite() {
            return 0;
        }
        value.round().clamp(0.0, limit) as i32
    };
    let x0 = clamp(left, w);
    let y0 = clamp(top, h);
    let x1 = clamp(right, w);
    let y1 = clamp(bottom, h);
    (x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
}

/// 从 PNG 头部读像素尺寸（IHDR 固定在第 16~24 字节）。
///
/// 不引入图像解码依赖：只需要宽高，而 IHDR 是 PNG 规范里**必须**是第一个
/// 块的位置。返回 `None` 说明这不是一个合法的 PNG。
fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE_LEN: usize = 8;
    if png.len() < SIGNATURE_LEN + 16 || &png[..SIGNATURE_LEN] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    // IHDR 长度(4) + 类型(4) + 宽(4) + 高(4)
    let width = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(png[20..24].try_into().ok()?);
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// 预热识别模型（阻塞，约 20~25 秒）。
///
/// 独立进程首次识别的代价是 **20~25 秒**（加载识别模型与语言资源），之后
/// 降到 0.6~0.9 秒。**不要在主线程直接调它**——TUI 会卡 20 秒。正常路径用
/// [`warm_up_in_background`]。
///
/// # 预热图必须真的含文字
///
/// 实测（本机 macOS 27.0.1）：拿一张 2×2 的空白 PNG 预热只花 248ms，**完全
/// 不触发模型加载**——`VNRecognizeTextRequest` 在图上找不到任何文本时不会
/// 初始化识别器，于是「预热过了」的第一次真实调用仍然要 22 秒。所以这里用
/// 一张 640×160、含中英文字的合成图（`assets/ocr-warmup.png`）。
///
/// 预热失败只记日志、不上抛——它只影响第一次调用的延迟，不影响功能正确性。
pub fn warm_up() {
    if let Err(error) = recognize_png(include_bytes!("../assets/ocr-warmup.png")) {
        tracing::debug!(%error, "ocr warm-up failed; the first real call will be slower");
    }
}

/// 在后台线程预热一次，**立即返回**。同一进程只跑一次。
///
/// 注册点调用它：`/model` 重建工具注册表时会再走一遍注册，`Once` 保证那次
/// 不会重复付出 20 秒。线程是 detached 的——预热纯粹是延迟优化，进程退出时
/// 没必要等它。
pub fn warm_up_in_background() {
    static WARMED: std::sync::Once = std::sync::Once::new();
    WARMED.call_once(|| {
        // 线程 spawn 失败（极端资源耗尽）只意味着第一次真实调用慢一些，
        // 不影响功能，因此忽略错误。
        let _ = std::thread::Builder::new()
            .name("wyj-ocr-warmup".to_string())
            .spawn(warm_up);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vision 的归一化矩形（左下原点）到像素矩形（左上原点）的换算。
    ///
    /// 这是整个 OCR 唯一一处坐标系转换，错了就是「每个文本行都被上下镜像，
    /// 模型照着点会点到屏幕另一头」——而且看起来完全像坐标算错。
    #[test]
    fn normalized_boxes_flip_to_top_left_pixel_space() {
        // 图像 1000x500。左下角的一个 100x50（归一化 0.1x0.1）的框。
        let bottom_left = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: 0.1,
                height: 0.1,
            },
        };
        let (x, y, w, h) = to_pixel_rect(bottom_left, 1000, 500);
        // 水平不变；垂直翻转后应贴在**下边**：top = (1 - 0.1) * 500 = 450。
        assert_eq!((x, y, w, h), (0, 450, 100, 50));

        // 左上角：归一化 y = 0.9、高 0.1 → 翻转后 top = (1 - 1.0) * 500 = 0。
        let top_left = CGRect {
            origin: CGPoint { x: 0.0, y: 0.9 },
            size: CGSize {
                width: 0.1,
                height: 0.1,
            },
        };
        assert_eq!(to_pixel_rect(top_left, 1000, 500), (0, 0, 100, 50));

        // 居中的框：x=0.4,y=0.4,w=0.2,h=0.2 → 像素 (400, 200, 200, 100)。
        let middle = CGRect {
            origin: CGPoint { x: 0.4, y: 0.4 },
            size: CGSize {
                width: 0.2,
                height: 0.2,
            },
        };
        assert_eq!(to_pixel_rect(middle, 1000, 500), (400, 200, 200, 100));
    }

    /// 越界与退化的归一化框必须钳制到图像内，而不是产出负坐标。
    #[test]
    fn out_of_range_boxes_are_clamped() {
        let beyond = CGRect {
            origin: CGPoint { x: -0.5, y: -0.5 },
            size: CGSize {
                width: 3.0,
                height: 3.0,
            },
        };
        let (x, y, w, h) = to_pixel_rect(beyond, 100, 100);
        assert_eq!((x, y), (0, 0));
        assert_eq!((w, h), (100, 100));
        // 负宽高的「框」不产生负尺寸。
        let inverted = CGRect {
            origin: CGPoint { x: 0.9, y: 0.9 },
            size: CGSize {
                width: -0.5,
                height: -0.5,
            },
        };
        let (_, _, w, h) = to_pixel_rect(inverted, 100, 100);
        assert!(w >= 0 && h >= 0, "尺寸不得为负：{w}x{h}");
    }

    #[test]
    fn png_dimensions_reads_the_ihdr_header() {
        // 真·PNG（预热图，640x160）。
        let png = include_bytes!("../assets/ocr-warmup.png");
        assert_eq!(png_dimensions(png), Some((640, 160)));
        // 非 PNG / 截断数据都要老实返回 None，不能越界读。
        assert_eq!(png_dimensions(b""), None);
        assert_eq!(png_dimensions(b"not a png"), None);
        assert_eq!(png_dimensions(&png[..20]), None);
    }
}
