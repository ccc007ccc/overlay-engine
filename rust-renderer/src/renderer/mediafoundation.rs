//! v0.7 phase 3：本地视频解码（Media Foundation）
//!
//! ## 设计（参考 spec §4.1）
//!
//! - 一个 `VideoSource` = 一个 `IMFSourceReader` + 一份共享 `BitmapHandle`
//! - `open_file` → 内部 `MFCreateSourceReaderFromURL` + 配置输出 `MFVideoFormat_RGB32`
//!   （让 MF source reader 自动从 NV12/H.264 输出 BGRA32，省自写 NV12→BGRA shader 的活）
//! - `present_frame` → `ReadSample` → CPU memcpy → `update_texture` 走 phase 2 通路
//! - `close` → 走 RAII drop，IMFSourceReader / IMFMediaType 全部 COM Release
//!
//! ## 路径选择：CPU readback vs GPU 共享
//!
//! v0.7 phase 3 通过判据是「30s 不崩」，性能不强求。CPU readback 方案：
//! - 1080p BGRA = 8MB，CPU memcpy ~3ms + GPU 上传 ~5ms = ~8ms/帧
//! - 30fps mp4 平均 33ms/帧，富余 25ms
//! - 复用 phase 2 现成 `update_texture`，不碰 D3D11 共享纹理 / D2D YCbCr effect
//!
//! 零拷贝（GPU 直拷 + D2D YCbCr effect）留给 v1.0 server 化重构时一起做，
//! 那时 ID2D1Bitmap1 已经持久绑底层 ID3D11Texture2D，可 `CopyResource` 直拷。
//!
//! ## MFStartup 生命周期
//!
//! 全局 OnceCell：进程内首个 `open_file` 触发 `MFStartup(MF_VERSION, 0)`。
//! `MFShutdown` 不主动调 —— OS 进程退出自动清，保证 cargo test 跑多个 case 时
//! 不会出现 startup/shutdown count 不平衡。

use std::sync::OnceLock;

use windows::core::{Interface, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFAttributes, IMFMediaBuffer, IMFMediaType, IMFSample, IMFSourceReader,
    MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video,
    MFStartup, MFVideoFormat_RGB32, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_PD_DURATION, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::VT_UI8;

use crate::error::{RendererError, RendererResult};
use crate::renderer::resources::BitmapHandle;

const MAX_VIDEO_DIMENSION: u32 = 16_384;

/// 视频元数据（spec §4.1 VideoInfo C struct 对应字段）
#[derive(Clone, Copy, Debug)]
pub(crate) struct VideoInfo {
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

/// 一个本地视频文件解码上下文。
///
/// 持有 IMFSourceReader + 配套 CPU staging buffer。bitmap handle 记录在外部，
/// 由 Renderer 层把 BitmapHandle 写进 VideoSource —— 这层模块只管 MF。
pub(crate) struct VideoSource {
    reader: IMFSourceReader,
    info: VideoInfo,
    /// 内部 BitmapHandle（create_texture(BGRA8) 拿到的），由 Renderer 创建并塞进来。
    /// `present_frame` 直接复用这个 handle 走 update_texture。
    pub(crate) bitmap: BitmapHandle,
    /// CPU staging：每帧 ReadSample 后 memcpy 到这里再 update_texture。
    /// 大小 = width × height × 4，per-instance 一次性分配。
    staging: Vec<u8>,
    /// 上一帧 PTS（100ns 单位）。debug / 跳跃检测用，目前未用，预留。
    #[allow(dead_code)]
    last_pts_100ns: i64,
    /// 已 read 到 EOS。设为 true 后 ReadSample 不再调（避免 MF 反复返 EOS 让上层 spam）。
    eof: bool,
}

/// 全局 MFStartup 守门 —— OnceLock 保证只调一次。
/// 调用失败时（未来某天 MF 没装）OnceLock 留 Err，下次 open_file 再试也走相同分支。
static MF_STARTUP: OnceLock<windows::core::Result<()>> = OnceLock::new();

fn ensure_mf_startup() -> RendererResult<()> {
    let result = MF_STARTUP.get_or_init(|| unsafe { MFStartup(MF_VERSION, 0) });
    match result {
        Ok(()) => Ok(()),
        Err(e) => Err(RendererError::DeviceInit(e.clone())),
    }
}

/// 100ns ticks → ms（保留整数 ms 精度，向下取整）
#[allow(dead_code)]
fn ticks_100ns_to_ms(ticks: i64) -> u64 {
    if ticks <= 0 {
        0
    } else {
        (ticks as u64) / 10_000
    }
}

fn ticks_100ns_to_ms_u64(ticks: u64) -> u64 {
    ticks / 10_000
}

fn ms_to_ticks_100ns(ms: u64) -> i64 {
    (ms.saturating_mul(10_000)) as i64
}

impl VideoSource {
    /// 打开一个本地视频文件，初始化 MF 解码并查出元数据。
    ///
    /// `bitmap_handle` 由 Renderer 上层调 `painter.create_texture(w, h, BGRA8)` 拿好后传入。
    /// VideoSource 不创建 bitmap —— 责任分离：MF 解码 vs D2D 资源管理。
    pub(crate) fn open_file(path: &str, bitmap: BitmapHandle) -> RendererResult<Self> {
        ensure_mf_startup()?;

        // path UTF-8 → UTF-16 给 MFCreateSourceReaderFromURL
        let mut wide: Vec<u16> = path.encode_utf16().collect();
        wide.push(0);

        // attributes：开启 video processing（自动 NV12 → RGB32 转码）+ 硬件加速
        let mut attrs: Option<IMFAttributes> = None;
        unsafe {
            MFCreateAttributes(&mut attrs, 4).map_err(RendererError::VideoOpenFail)?;
        }
        let attrs = attrs.ok_or_else(|| {
            RendererError::VideoOpenFail(windows::core::Error::new(
                windows::core::HRESULT(0x80004005u32 as i32),
                "MFCreateAttributes returned null",
            ))
        })?;
        unsafe {
            attrs
                .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
                .map_err(RendererError::VideoOpenFail)?;
            attrs
                .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                .map_err(RendererError::VideoOpenFail)?;
        }

        // SourceReader from URL
        // 文件不存在 / codec 不支持 / DRM 都会落到这里。统一报 VideoOpenFail。
        let reader = unsafe {
            MFCreateSourceReaderFromURL(PCWSTR::from_raw(wide.as_ptr()), &attrs)
                .map_err(RendererError::VideoOpenFail)?
        };

        // 关闭所有流，只开第一条 video stream
        unsafe {
            reader
                .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
                .map_err(RendererError::VideoOpenFail)?;
            reader
                .SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, true)
                .map_err(RendererError::VideoOpenFail)?;
        }

        // 配置输出 = RGB32（B8G8R8X8）。MF source reader 内部走 video processor
        // 自动从 H.264 NV12 转 RGB32，不用我们自己写 shader。
        let out_type = unsafe {
            let mt = MFCreateMediaType().map_err(RendererError::VideoOpenFail)?;
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(RendererError::VideoOpenFail)?;
            mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .map_err(RendererError::VideoOpenFail)?;
            mt
        };
        unsafe {
            reader
                .SetCurrentMediaType(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    None,
                    &out_type,
                )
                .map_err(RendererError::VideoOpenFail)?;
        }

        // 拿当前 media type 读 width / height / frame rate
        let cur_type = unsafe {
            reader
                .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
                .map_err(RendererError::VideoOpenFail)?
        };
        ensure_rgb32_subtype(&cur_type)?;
        let (width, height) = read_frame_size(&cur_type)?;
        let (fps_num, fps_den) = read_frame_rate(&cur_type).unwrap_or((30, 1));

        // duration：从 PresentationDescriptor 取 PD_DURATION（100ns ticks）
        let duration_ms = read_duration_ms(&reader).unwrap_or(0);

        let staging_size = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| RendererError::InvalidParam("video frame size overflow"))?;

        Ok(Self {
            reader,
            info: VideoInfo {
                duration_ms,
                width,
                height,
                fps_num,
                fps_den,
            },
            bitmap,
            staging: vec![0u8; staging_size],
            last_pts_100ns: 0,
            eof: false,
        })
    }

    pub(crate) fn info(&self) -> VideoInfo {
        self.info
    }

    /// 跳到指定毫秒位置。EOS flag 也清掉（重新开始读）。
    pub(crate) fn seek(&mut self, time_ms: u64) -> RendererResult<()> {
        if self.info.duration_ms > 0 && time_ms > self.info.duration_ms {
            return Err(RendererError::VideoSeekFail(windows::core::Error::new(
                windows::core::HRESULT(0x80070057u32 as i32),
                "seek position exceeds video duration",
            )));
        }
        let ticks = ms_to_ticks_100ns(time_ms);
        let var = make_propvariant_u8(ticks as u64);
        // GUID_NULL = 默认时间格式（100ns ticks，参考 MSDN MF_REFERENCE_TIME）
        let null_guid = windows::core::GUID::zeroed();
        unsafe {
            self.reader
                .SetCurrentPosition(&null_guid as *const _, &var as *const _)
                .map_err(RendererError::VideoSeekFail)?;
        }
        self.eof = false;
        self.last_pts_100ns = 0;
        Ok(())
    }

    /// 解一帧到 self.staging，返回 (BGRA bytes 切片引用, stride, eof)。
    ///
    /// stride 在 RGB32 packed 输出场景下 == width × 4。如果硬件返了 padded
    /// stride（一些 GPU 解码器会 4-byte align 之上再 64-byte align），按实际 buffer length 校验。
    /// 上层调 `update_texture(bitmap, slice, stride, BGRA8)` 完成 GPU 上传。
    pub(crate) fn read_next_frame(&mut self) -> RendererResult<(&[u8], i32, bool)> {
        if self.eof {
            // EOS 后不再 ReadSample，直接告诉上层「还是上一帧 + EOS」
            return Ok((&self.staging, (self.info.width * 4) as i32, true));
        }

        let mut sample: Option<IMFSample> = None;
        let mut stream_index: u32 = 0;
        let mut flags: u32 = 0;
        let mut timestamp: i64 = 0;

        // ReadSample 是同步阻塞调用（默认 dwControlFlags=0）。
        // 跨进程 pump 线程驱动，UI 线程不卡。
        unsafe {
            self.reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    Some(&mut stream_index as *mut u32),
                    Some(&mut flags as *mut u32),
                    Some(&mut timestamp as *mut i64),
                    Some(&mut sample as *mut Option<IMFSample>),
                )
                .map_err(|e| RendererError::VideoDecodeFail(format!("ReadSample failed: {e}")))?;
        }

        // EOS：sample 可能为 None，flags 含 ENDOFSTREAM
        if (flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            self.eof = true;
        }

        // current media type 改了（hw decoder 切了输出格式）—— 重新读尺寸。
        // 当前实现简化：不动 staging buffer 大小，直接报错让业务重启 video。
        if (flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32) != 0 {
            return Err(RendererError::VideoFormatChanged);
        }

        let sample = match sample {
            Some(s) => s,
            None => {
                // 正常：EOS 时 MF 返 sample=None。staging 仍是上一帧。
                return Ok((&self.staging, (self.info.width * 4) as i32, self.eof));
            }
        };

        self.last_pts_100ns = timestamp;

        let buffer: IMFMediaBuffer = unsafe {
            sample.ConvertToContiguousBuffer().map_err(|e| {
                RendererError::VideoDecodeFail(format!("ConvertToContiguousBuffer: {e}"))
            })?
        };

        if let Ok(buffer2d) = buffer.cast::<IMF2DBuffer>() {
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut pitch: i32 = 0;
            unsafe {
                buffer2d
                    .Lock2D(&mut data, &mut pitch)
                    .map_err(|e| RendererError::VideoDecodeFail(format!("buffer Lock2D: {e}")))?;
            }
            let res = (|| -> RendererResult<()> {
                if data.is_null() || pitch <= 0 {
                    return Err(RendererError::VideoDecodeFail(format!(
                        "MF 2D buffer returned invalid pitch {}",
                        pitch
                    )));
                }
                let src_len =
                    source_len_for_stride(pitch as usize, self.info.width, self.info.height)?;
                let src = unsafe { std::slice::from_raw_parts(data, src_len) };
                copy_bgra_rows_force_opaque_alpha(
                    src,
                    pitch as usize,
                    self.info.width,
                    self.info.height,
                    &mut self.staging,
                )
            })();
            unsafe { buffer2d.Unlock2D().ok() };
            res?;
            return Ok((
                &self.staging,
                bgra_row_bytes(self.info.width)? as i32,
                self.eof,
            ));
        }

        let mut data: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        let mut cur_len: u32 = 0;
        unsafe {
            buffer
                .Lock(&mut data, Some(&mut max_len), Some(&mut cur_len))
                .map_err(|e| RendererError::VideoDecodeFail(format!("buffer Lock: {e}")))?;
        }
        let res = (|| -> RendererResult<()> {
            if data.is_null() {
                return Err(RendererError::VideoDecodeFail(
                    "MF buffer returned null data".to_string(),
                ));
            }
            let src = unsafe { std::slice::from_raw_parts(data, cur_len as usize) };
            let row_bytes = bgra_row_bytes(self.info.width)?;
            copy_bgra_rows_force_opaque_alpha(
                src,
                row_bytes,
                self.info.width,
                self.info.height,
                &mut self.staging,
            )
        })();
        unsafe { buffer.Unlock().ok() };
        res?;

        Ok((
            &self.staging,
            bgra_row_bytes(self.info.width)? as i32,
            self.eof,
        ))
    }
}

/// IMFSourceReader / IMFAttributes / IMFMediaType 都是 COM 接口，drop 时 windows-rs 自动 Release。
impl Drop for VideoSource {
    fn drop(&mut self) {
        // 显式 Release 由 windows-rs RAII 处理。staging Vec 自动释放。
        // bitmap 由 Renderer 上层 destroy（VideoSource 不知道 painter）。
    }
}

// -------------------- helpers --------------------

fn ensure_rgb32_subtype(mt: &IMFMediaType) -> RendererResult<()> {
    let subtype = unsafe {
        mt.GetGUID(&MF_MT_SUBTYPE)
            .map_err(RendererError::VideoOpenFail)?
    };
    if subtype == MFVideoFormat_RGB32 {
        Ok(())
    } else {
        Err(RendererError::VideoOpenFail(windows::core::Error::new(
            windows::core::HRESULT(0x80004005u32 as i32),
            "MF did not keep RGB32 output subtype",
        )))
    }
}

fn bgra_row_bytes(width: u32) -> RendererResult<usize> {
    (width as usize)
        .checked_mul(4)
        .ok_or_else(|| RendererError::VideoDecodeFail("video row byte count overflow".to_string()))
}

fn source_len_for_stride(stride: usize, width: u32, height: u32) -> RendererResult<usize> {
    let row_bytes = bgra_row_bytes(width)?;
    if stride < row_bytes {
        return Err(RendererError::VideoDecodeFail(format!(
            "MF stride too small: {} < {}",
            stride, row_bytes
        )));
    }
    if height == 0 {
        return Ok(0);
    }
    stride
        .checked_mul(height.saturating_sub(1) as usize)
        .and_then(|n| n.checked_add(row_bytes))
        .ok_or_else(|| {
            RendererError::VideoDecodeFail("video source byte count overflow".to_string())
        })
}

fn copy_bgra_rows_force_opaque_alpha(
    src: &[u8],
    src_stride: usize,
    width: u32,
    height: u32,
    dst: &mut [u8],
) -> RendererResult<()> {
    let row_bytes = bgra_row_bytes(width)?;
    let expected_dst = row_bytes.checked_mul(height as usize).ok_or_else(|| {
        RendererError::VideoDecodeFail("video staging byte count overflow".to_string())
    })?;
    if dst.len() < expected_dst {
        return Err(RendererError::VideoDecodeFail(format!(
            "video staging buffer too small: {} < {}",
            dst.len(),
            expected_dst
        )));
    }
    let expected_src = source_len_for_stride(src_stride, width, height)?;
    if src.len() < expected_src {
        return Err(RendererError::VideoDecodeFail(format!(
            "MF buffer too small: {} < {}",
            src.len(),
            expected_src
        )));
    }

    for y in 0..height as usize {
        let src_start = y * src_stride;
        let dst_start = y * row_bytes;
        let src_row = &src[src_start..src_start + row_bytes];
        let dst_row = &mut dst[dst_start..dst_start + row_bytes];
        dst_row.copy_from_slice(src_row);
        for px in dst_row.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }
    Ok(())
}

fn read_frame_size(mt: &IMFMediaType) -> RendererResult<(u32, u32)> {
    let packed = unsafe {
        mt.GetUINT64(&MF_MT_FRAME_SIZE)
            .map_err(RendererError::VideoOpenFail)?
    };
    // MF 把 (w, h) pack 进一个 u64：高 32 = w，低 32 = h
    let width = (packed >> 32) as u32;
    let height = (packed & 0xFFFF_FFFF) as u32;
    if width == 0 || height == 0 {
        return Err(RendererError::VideoOpenFail(windows::core::Error::new(
            windows::core::HRESULT(0x80004005u32 as i32),
            "MF reported zero frame size",
        )));
    }
    if width > MAX_VIDEO_DIMENSION || height > MAX_VIDEO_DIMENSION {
        return Err(RendererError::VideoOpenFail(windows::core::Error::new(
            windows::core::HRESULT(0x80070057u32 as i32),
            "MF reported oversized frame size",
        )));
    }
    Ok((width, height))
}

fn read_frame_rate(mt: &IMFMediaType) -> Option<(u32, u32)> {
    let packed = unsafe { mt.GetUINT64(&MF_MT_FRAME_RATE).ok()? };
    let num = (packed >> 32) as u32;
    let den = (packed & 0xFFFF_FFFF) as u32;
    if num == 0 || den == 0 {
        None
    } else {
        Some((num, den))
    }
}

/// Duration 在 IMFSourceReader 上是 PresentationAttribute（不是 stream attribute）。
/// 走 `GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE, MF_PD_DURATION)`。
fn read_duration_ms(reader: &IMFSourceReader) -> Option<u64> {
    // MF_SOURCE_READER_MEDIASOURCE = 0xFFFFFFFF
    const MF_SOURCE_READER_MEDIASOURCE: u32 = 0xFFFF_FFFF;
    let pv: PROPVARIANT = unsafe {
        reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE, &MF_PD_DURATION as *const _)
    }
    .ok()?;
    // pv.vt == VT_UI8（u64 ticks 100ns）
    let ticks = unsafe { pv.Anonymous.Anonymous.Anonymous.uhVal };
    Some(ticks_100ns_to_ms_u64(ticks))
}

/// 构造 PROPVARIANT(VT_UI8, ticks) 给 IMFSourceReader::SetCurrentPosition
fn make_propvariant_u8(ticks: u64) -> PROPVARIANT {
    let mut pv = PROPVARIANT::default();
    unsafe {
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_UI8;
        inner.Anonymous.uhVal = ticks;
    }
    pv
}

// -------------------- 测试 --------------------
//
// 没法在 cargo test 里跑解码（没 mp4 资产），open_file 失败路径 + helpers 走过即可。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_100ns_conversions_round_trip_basic() {
        assert_eq!(ticks_100ns_to_ms(0), 0);
        assert_eq!(ticks_100ns_to_ms(10_000), 1);
        assert_eq!(ticks_100ns_to_ms(123_4500), 123); // 截断
        assert_eq!(ms_to_ticks_100ns(0), 0);
        assert_eq!(ms_to_ticks_100ns(1), 10_000);
        assert_eq!(ms_to_ticks_100ns(1_000), 10_000_000);
    }

    #[test]
    fn copy_bgra_rows_handles_packed_source_and_forces_alpha() {
        let src = [10u8, 20, 30, 0, 40, 50, 60, 7];
        let mut dst = [0u8; 8];
        copy_bgra_rows_force_opaque_alpha(&src, 8, 2, 1, &mut dst).unwrap();
        assert_eq!(dst, [10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[test]
    fn copy_bgra_rows_handles_padded_stride() {
        let src = [1u8, 2, 3, 4, 0xEE, 0xEE, 5, 6, 7, 8, 0xEE, 0xEE];
        let mut dst = [0u8; 8];
        copy_bgra_rows_force_opaque_alpha(&src, 6, 1, 2, &mut dst).unwrap();
        assert_eq!(dst, [1, 2, 3, 255, 5, 6, 7, 255]);
    }

    #[test]
    fn copy_bgra_rows_rejects_stride_below_row_bytes() {
        let src = [0u8; 8];
        let mut dst = [0u8; 8];
        let err = copy_bgra_rows_force_opaque_alpha(&src, 4, 2, 1, &mut dst).unwrap_err();
        assert!(matches!(err, RendererError::VideoDecodeFail(_)));
    }

    #[test]
    fn source_len_for_stride_checks_last_row_without_full_padding() {
        assert_eq!(source_len_for_stride(8, 1, 3).unwrap(), 20);
    }

    #[test]
    fn open_file_nonexistent_path_returns_video_open_error() {
        // MF 对不存在文件返 MF_E_NOT_AVAILABLE / 0x80070002 之类，统一走 VideoOpenFail
        let res = VideoSource::open_file("Z:\\nonexistent\\not_a_real_video.mp4", 1);
        match res {
            Err(RendererError::VideoOpenFail(_)) => {}
            other => panic!(
                "expected VideoOpenFail, got {}",
                other.map(|_| "Ok").unwrap_or("other Err")
            ),
        }
    }
}
