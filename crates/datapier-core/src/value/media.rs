//! 有预算的静态图片解码和离线 SVG 栅格化。
//!
//! 输入、嵌入资源及最终 PNG 按实际字节计入预留；解析树按输入长度估算，
//! 解码与渲染工作区按像素数估算。第三方库的临时分配不提供严格上界，
//! 本预算不代表进程 RSS 上限；并发额度由调用方控制。

use super::{limit, structured::parse_xml};
use crate::model::{AppError, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{ImageDecoder, ImageEncoder, ImageFormat, ImageReader};
use std::io::{self, Cursor, Write};
use std::sync::{Arc, LazyLock};

const MAX_SIDE: u32 = 4096;
const MAX_PIXELS: u64 = 4_194_304;
const MAX_PNG: usize = 16 * 1024 * 1024;
const MAX_SVG_DEPTH: usize = 4;

/// 仅包含内存中的派生 PNG；原始值不受影响。
#[derive(Debug)]
pub struct Preview {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub(super) struct BoundedWriter {
    data: Vec<u8>,
    max: usize,
    exceeded: bool,
}
impl BoundedWriter {
    pub(super) fn new(max: usize) -> Self {
        Self {
            data: Vec::new(),
            max,
            exceeded: false,
        }
    }
    pub(super) fn exceeded(&self) -> bool {
        self.exceeded
    }
    pub(super) fn into_inner(self) -> Vec<u8> {
        self.data
    }
}
impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.max.saturating_sub(self.data.len()) {
            self.exceeded = true;
            return Err(io::Error::other("输出超过预算"));
        }
        self.data.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn image_error(e: image::ImageError) -> AppError {
    match e {
        image::ImageError::Limits(_) => limit(),
        image::ImageError::Unsupported(_) => AppError::new("unsupported_format", e.to_string()),
        _ => AppError::new("invalid_image", e.to_string()),
    }
}
fn dimensions(width: u32, height: u32, budget: usize) -> Result<usize> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || pixels > MAX_PIXELS {
        return Err(limit());
    }
    // 为解码、RGBA 转换及渲染中间图保守预留，不代表依赖内部的分配硬上界。
    let reserve = usize::try_from(pixels)
        .ok()
        .and_then(|n| n.checked_mul(32))
        .ok_or_else(limit)?;
    if reserve > budget {
        return Err(limit());
    }
    Ok(reserve)
}
fn raster_decoder(data: &[u8], budget: usize) -> Result<impl ImageDecoder + '_> {
    let format = image::guess_format(data).map_err(image_error)?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
    ) {
        return Err(AppError::new("unsupported_format", "不支持该图片格式"));
    }
    let mut reader = ImageReader::with_format(Cursor::new(data), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(budget as u64);
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(image_error)?;
    let (w, h) = decoder.dimensions();
    dimensions(w, h, budget)?;
    Ok(decoder)
}
fn encode(pixels: &[u8], width: u32, height: u32, budget: usize) -> Result<Preview> {
    let mut writer = BoundedWriter::new(MAX_PNG.min(budget));
    let result = image::codecs::png::PngEncoder::new(&mut writer).write_image(
        pixels,
        width,
        height,
        image::ExtendedColorType::Rgba8,
    );
    if let Err(e) = result {
        return Err(if writer.exceeded() {
            limit()
        } else {
            image_error(e)
        });
    }
    Ok(Preview {
        png: writer.into_inner(),
        width,
        height,
    })
}

/// 仅供显式预览调用；budget 是调用方为整个派生操作保留的剩余字节。
/// 本函数同步执行，应在 Engine 有界后台 worker 上调用。
pub fn preview_image(data: &[u8], budget: usize) -> Result<Preview> {
    if data.len() > budget {
        return Err(limit());
    }
    if image::guess_format(data).is_ok() {
        let remaining = budget - data.len();
        let decoder = raster_decoder(data, remaining)?;
        let (width, height) = decoder.dimensions();
        let reserve = dimensions(width, height, remaining)?;
        let image = image::DynamicImage::from_decoder(decoder)
            .map_err(image_error)?
            .into_rgba8();
        encode(
            image.as_raw(),
            width,
            height,
            remaining.saturating_sub(reserve),
        )
    } else {
        preview_svg(data, budget)
    }
}

// 检查所有资源再交给 usvg；默认 string resolver 会读取本地文件，必须禁用。
// 递归 data URI 的实际字节和解析树、解码空间估算从同一剩余预算扣除。
fn inspect_svg(data: &[u8], depth: usize, remaining: &mut usize) -> Result<bool> {
    if depth > MAX_SVG_DEPTH {
        return Err(limit());
    }
    let text = std::str::from_utf8(data)
        .map_err(|_| AppError::new("unsupported_format", "图片不是支持的格式或 UTF-8 SVG"))?;
    let tree_cost = data.len().checked_mul(64).ok_or_else(limit)?;
    *remaining = remaining.checked_sub(tree_cost).ok_or_else(limit)?;
    let doc = parse_xml(text)?;
    let root = doc.root_element();
    if root.tag_name().name() != "svg"
        || root.tag_name().namespace() != Some("http://www.w3.org/2000/svg")
    {
        return Err(AppError::new("unsupported_format", "不是 SVG 文档"));
    }
    let mut has_text = false;
    for node in doc.descendants().filter(|n| n.is_element()) {
        let name = node.tag_name().name();
        has_text |=
            name == "text" && node.tag_name().namespace() == Some("http://www.w3.org/2000/svg");
        if name.eq_ignore_ascii_case("script") || name.eq_ignore_ascii_case("foreignObject") {
            return Err(AppError::new(
                "unsupported_format",
                "SVG 包含脚本或 foreignObject",
            ));
        }
        // resvg 的滤镜中间图没有公开的内存限额，拒绝不能可靠计费的路径。
        if name.eq_ignore_ascii_case("filter") {
            return Err(AppError::new(
                "resource_limit",
                "SVG 滤镜的中间图无法按预算限制",
            ));
        }
        if name.eq_ignore_ascii_case("style") {
            let css = node.text().unwrap_or("").to_ascii_lowercase();
            if css.contains("@import") || css.contains("url(") {
                return Err(AppError::new(
                    "unsupported_format",
                    "SVG 样式中的资源引用不支持",
                ));
            }
        }
        for attr in node.attributes() {
            let value = attr.value().trim();
            let lower = value.to_ascii_lowercase();
            // CSS 可以包含多个 url()，逐个检查所有引用。
            let mut css = lower.as_str();
            while let Some(pos) = css.find("url(") {
                css = &css[pos + 4..];
                let Some(end) = css.find(')') else {
                    return Err(AppError::new("unsupported_format", "无效 SVG 资源引用"));
                };
                let target = css[..end].trim().trim_matches(['\'', '"']);
                if !target.starts_with('#') || target.contains('\\') {
                    return Err(AppError::new("unsupported_format", "SVG 包含外部资源引用"));
                }
                css = &css[end + 1..];
            }
            if lower.contains('\\') && (attr.name() == "style" || lower.contains("url")) {
                return Err(AppError::new(
                    "unsupported_format",
                    "SVG 样式中的转义资源引用不支持",
                ));
            }
            if attr.name().eq_ignore_ascii_case("href") {
                if value.starts_with('#') {
                    continue;
                }
                if !name.eq_ignore_ascii_case("image") || !value.starts_with("data:") {
                    return Err(AppError::new("unsupported_format", "SVG 包含外部资源引用"));
                }
                let (header, encoded) = value
                    .split_once(',')
                    .ok_or_else(|| AppError::invalid("无效 data image"))?;
                if !header.ends_with(";base64") {
                    return Err(AppError::new(
                        "unsupported_format",
                        "仅支持 Base64 data image",
                    ));
                }
                if !matches!(
                    header,
                    "data:image/svg+xml;base64"
                        | "data:image/png;base64"
                        | "data:image/jpeg;base64"
                        | "data:image/jpg;base64"
                        | "data:image/gif;base64"
                        | "data:image/webp;base64"
                ) {
                    return Err(AppError::new(
                        "unsupported_format",
                        "不支持的 data image 类型",
                    ));
                }
                let estimate = encoded.len().checked_mul(3).ok_or_else(limit)? / 4;
                if estimate > *remaining {
                    return Err(limit());
                }
                let bytes = STANDARD
                    .decode(encoded)
                    .map_err(|e| AppError::invalid(format!("无效 data image: {e}")))?;
                *remaining = remaining.checked_sub(bytes.len()).ok_or_else(limit)?;
                if header.starts_with("data:image/svg+xml;") {
                    has_text |= inspect_svg(&bytes, depth + 1, remaining)?;
                } else {
                    let actual = image::guess_format(&bytes).map_err(image_error)?;
                    let matches_type = match header {
                        "data:image/png;base64" => actual == ImageFormat::Png,
                        "data:image/jpeg;base64" | "data:image/jpg;base64" => {
                            actual == ImageFormat::Jpeg
                        }
                        "data:image/gif;base64" => actual == ImageFormat::Gif,
                        "data:image/webp;base64" => actual == ImageFormat::WebP,
                        _ => false,
                    };
                    if !matches_type {
                        return Err(AppError::new(
                            "invalid_image",
                            "data image 类型与内容不一致",
                        ));
                    }
                    let decoder = raster_decoder(&bytes, *remaining)?;
                    let (w, h) = decoder.dimensions();
                    let reserve = dimensions(w, h, *remaining)?;
                    // 实际解码以验证损坏数据，避免渲染器静默丢弃嵌入图像。
                    image::DynamicImage::from_decoder(decoder).map_err(image_error)?;
                    *remaining = remaining.checked_sub(reserve).ok_or_else(limit)?;
                }
            }
        }
    }
    Ok(has_text)
}

// 字体只在 SVG 含文字时扫描一次，并由各预览共享；不把系统字体复制进每个值预算。
static SVG_FONTS: LazyLock<Arc<resvg::usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut fonts = resvg::usvg::fontdb::Database::new();
    fonts.load_system_fonts();
    Arc::new(fonts)
});
fn preview_svg(data: &[u8], budget: usize) -> Result<Preview> {
    let mut remaining = budget.checked_sub(data.len()).ok_or_else(limit)?;
    let has_text = inspect_svg(data, 0, &mut remaining)?;
    let text = std::str::from_utf8(data).map_err(|e| AppError::invalid(e.to_string()))?;
    let mut options = resvg::usvg::Options::default();
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    if has_text {
        options.fontdb = Arc::clone(&SVG_FONTS);
        if options.fontdb.faces().next().is_none() {
            return Err(AppError::new(
                "unsupported_format",
                "SVG 文字预览需要可用的系统字体",
            ));
        }
    }
    let tree = resvg::usvg::Tree::from_str(text, &options)
        .map_err(|e| AppError::new("invalid_image", e.to_string()))?;
    let size = tree.size();
    let width = size.width().ceil() as u32;
    let height = size.height().ceil() as u32;
    let reserve = dimensions(width, height, remaining)?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).ok_or_else(limit)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    // tiny-skia 的像素是 premultiplied，PNG 需要 straight alpha。
    let mut pixels = pixmap.take();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let a = u32::from(pixel[3]);
        for channel in &mut pixel[..3] {
            if let Some(straight) = (u32::from(*channel) * 255 + a / 2).checked_div(a) {
                *channel = straight.min(255) as u8;
            }
        }
    }
    encode(&pixels, width, height, remaining.saturating_sub(reserve))
}
