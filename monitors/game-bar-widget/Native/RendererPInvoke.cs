using System;
using System.Runtime.InteropServices;
using System.Text;

namespace OverlayWidget.Native
{
    /// <summary>
    /// renderer.dll 的 P/Invoke 包装（v0.6 DComp swap chain 路径）。
    ///
    /// 常量、struct 顺序、字段类型必须与 Rust 端 <c>rust-renderer/src/ffi.rs</c> 严格一致。
    /// 调用约定 StdCall 在 Windows x64 下等价 MS x64 ABI，与 Rust 端
    /// <c>extern "system"</c> 匹配。
    ///
    /// ## 阶段史
    /// - **R3**（已废弃）：SwapChainPanel + ISwapChainPanelNative。widget host 跨进程代理拒
    ///   该 native COM 接口（QI E_NOINTERFACE），路被堵死。
    /// - **V1-V3 Pinned**（已废弃）：CPU readback + WriteableBitmap。modal move loop 期间
    ///   UI 线程冻结，wb.Invalidate 排队 → x 轴拖动期间画面冻结。
    /// - **v0.5 viewport-aware**（已废弃）：viewport-sized RT 池 + ThreadPool 后台渲染。
    ///   modal block 仍在 XAML compositor 层。
    /// - **v0.6 DComp**（当前）：CreateSwapChainForComposition + ICompositionSurface。
    ///   widget 内容由 DWM 内核合成，绕开 XAML compositor，**modal 不阻塞**。
    ///
    /// ## 调用流（典型）
    /// <code>
    /// init:    renderer_set_log_callback(cb);
    ///          renderer_create(canvasW, canvasH, out h);
    ///          renderer_get_swapchain(h, out swapChainIUnknown);
    ///          // C# 用 swapChainIUnknown 包装 ICompositionSurface 挂到 widget visual
    /// 每帧:    renderer_begin_frame(h, vpX, vpY, vpW, vpH);
    ///          renderer_clear / fill_rect / draw_text(...);
    ///          renderer_end_frame(h);
    ///          // 内部 EndDraw + Present(0,0)；DComp 自动拉新内容；C# 不需要 readback / 同步
    /// resize:  renderer_resize(h, canvasW, canvasH);
    /// 关闭:    renderer_destroy(h);
    /// </code>
    /// </summary>
    internal static class Renderer
    {
        // dll 名仅文件名，UWP 包部署时位于 PackageRoot，DllImport 会自动找到
        private const string Dll = "renderer.dll";

        // 与 ffi.rs::RendererStatus 严格一致
        public const int RENDERER_OK = 0;
        public const int RENDERER_ERR_INVALID_PARAM = -1;
        public const int RENDERER_ERR_DEVICE_INIT = -2;
        public const int RENDERER_ERR_SWAPCHAIN_INIT = -3;
        public const int RENDERER_ERR_THREAD_INIT = -4;
        public const int RENDERER_ERR_NOT_ATTACHED = -5;
        public const int RENDERER_ERR_FRAME_HELD = -6;
        public const int RENDERER_ERR_FRAME_ACQUIRE = -7;
        // v0.7 phase 2/3 — Bitmap & 外部纹理
        public const int RENDERER_ERR_RESOURCE_NOT_FOUND = -8;
        public const int RENDERER_ERR_RESOURCE_LIMIT = -9;
        public const int RENDERER_ERR_DECODE_FAIL = -10;
        public const int RENDERER_ERR_IO = -11;
        public const int RENDERER_ERR_UNSUPPORTED_FORMAT = -12;
        // v0.7 §2.6.3 — 画布管理（renderer_resize_canvas 失败专用）
        public const int RENDERER_ERR_CANVAS_RESIZE_FAIL = -14;
        // v0.7 phase 3 video（spec §4.1）
        public const int RENDERER_ERR_VIDEO_OPEN_FAIL = -15;
        public const int RENDERER_ERR_VIDEO_NOT_FOUND = -16;
        public const int RENDERER_ERR_VIDEO_SEEK_FAIL = -17;
        public const int RENDERER_ERR_VIDEO_DECODE_FAIL = -18;
        public const int RENDERER_ERR_VIDEO_FORMAT_CHANGED = -19;

        // bitmap format（与 Rust 端 painter::BitmapFormat repr 一致）
        public const int BITMAP_FORMAT_BGRA8 = 0;
        public const int BITMAP_FORMAT_RGBA8 = 1;
        public const int BITMAP_FORMAT_NV12 = 2;

        // interp mode for renderer_draw_bitmap
        public const int INTERP_NEAREST = 0;
        public const int INTERP_LINEAR = 1;

        /// <summary>
        /// 滑动平均的 perf 统计（最近 N 帧）。字段顺序与 Rust <c>PerfStats</c> struct 一致。
        ///
        /// v0.6 DComp 起 <c>AvgReadbackUs</c> / <c>PeakReadbackUs</c> 字段含义改为
        /// Present(0,0) 调用耗时（CPU 端时间，不等 GPU 完成）。字段名保留以维持 ABI 兼容。
        /// </summary>
        [StructLayout(LayoutKind.Sequential)]
        public struct PerfStats
        {
            /// <summary>begin_frame → end_frame 之间所有 cmd_* + EndDraw 累积耗时（us）</summary>
            public ulong AvgRenderUs;
            /// <summary>v0.6: Present(0,0) 调用耗时（us）。原 readback 含义已废弃</summary>
            public ulong AvgReadbackUs;
            /// <summary>render + present 总耗时（us）</summary>
            public ulong AvgTotalUs;
            public ulong PeakRenderUs;
            public ulong PeakReadbackUs;
            /// <summary>已 end_frame 成功的总帧数</summary>
            public ulong TotalFrames;
            public uint WindowSize;
            public uint ValidSamples;
        }

        /// <summary>
        /// 日志回调。utf8Msg 仅在调用期间有效，必须立即拷贝。
        /// 必须保持托管端引用以防 GC，否则 Rust 端持有的函数指针就 dangling。
        ///
        /// [UnmanagedFunctionPointer] 让 .NET Native (UWP release) 在 AOT 编译时
        /// 生成正确的 reverse P/Invoke thunk。
        /// </summary>
        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        public delegate void LogCallback(int level, IntPtr utf8Msg);

        // ============================================================
        // 生命周期
        // ============================================================

        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_create(
            int pixelWidth,
            int pixelHeight,
            out IntPtr outHandle);

        /// <summary>
        /// canvas 逻辑尺寸变化（显示器分辨率改了或 widget 移到不同显示器）。
        /// 不重建 swap chain（swap chain 由 begin_frame 按 viewport 大小自动 ResizeBuffers）。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_resize(
            IntPtr handle,
            int pixelWidth,
            int pixelHeight);

        /// <summary>
        /// **v0.7 §2.6.3 新增**：显式画布管理 ABI。
        ///
        /// 在 SizeChanged / WM_SIZE / 用户改设置面板时调；**不要每帧调**
        /// （ResizeBuffers 重分配 GPU 缓冲，per-frame 性能损失明显）。
        /// 同尺寸 short-circuit 零开销；零尺寸或 begin_frame/end_frame 之间调用返错。
        ///
        /// 返回:
        /// <list type="bullet">
        /// <item><c>RENDERER_OK</c>(0)</item>
        /// <item><c>RENDERER_ERR_INVALID_PARAM</c>(-1) — newW/newH ≤ 0 或 handle null</item>
        /// <item><c>RENDERER_ERR_FRAME_HELD</c>(-6) — 当前在 begin_frame/end_frame 之间</item>
        /// <item><c>RENDERER_ERR_CANVAS_RESIZE_FAIL</c>(-14) — ResizeBuffers / 重建 D2D bitmap 失败</item>
        /// </list>
        ///
        /// widget 端典型用法（SizeChanged 事件）:
        /// <code>
        /// var dpi = ScreenInterop.GetDpiForWindow(...);
        /// int physW = (int)(e.NewSize.Width * dpi / 96.0);
        /// int physH = (int)(e.NewSize.Height * dpi / 96.0);
        /// Renderer.renderer_resize_canvas(_handle, physW, physH);
        /// </code>
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_resize_canvas(
            IntPtr handle,
            int newW,
            int newH);

        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern void renderer_destroy(IntPtr handle);

        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_set_log_callback(LogCallback cb);

        // ============================================================
        // v0.6 DComp：拿 swap chain 的 IUnknown raw pointer（已 AddRef）
        // ============================================================

        /// <summary>
        /// 拿 swap chain 的 IUnknown raw pointer（已 AddRef）。C# 端用
        /// <see cref="Marshal.GetObjectForIUnknown"/> 转 IDXGISwapChain，再通过
        /// ICompositorInterop::CreateCompositionSurfaceForSwapChain 包成 ICompositionSurface。
        ///
        /// 调用方用完 IUnknown 必须调 <see cref="Marshal.Release"/> 一次（成对 AddRef）。
        /// 渲染器自身仍持有 swap chain 引用，destroy 时自动释放。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_get_swapchain(
            IntPtr handle,
            out IntPtr outIUnknown);

        // ============================================================
        // 命令式 Painter ABI（每帧三步）
        // ============================================================
        //
        //   1. renderer_begin_frame(h, vpX, vpY, vpW, vpH)   — 内部 SetTarget + BeginDraw + SetTransform
        //   2. renderer_clear / fill_rect / draw_text  ...0..N — 推绘制命令（按 canvas-space 坐标）
        //   3. renderer_end_frame(h)                          — EndDraw + Present(0, 0)
        //
        // 颜色：premultiplied alpha float [0,1]，rgb ≤ a。
        // 坐标：canvas-space 像素（与 renderer_create 设的 canvas 尺寸对齐）。

        /// <summary>
        /// 开始一帧。配对 <see cref="renderer_end_frame"/>。
        ///
        /// viewport-aware：业务侧告诉 renderer "本帧只关心 canvas 中 (vpX, vpY, vpW, vpH) 这块"。
        /// 业务命令坐标系仍是 canvas-space；Rust 内部 swap chain back buffer 大小=viewport，
        /// SetTransform(translate(-vpX, -vpY)) 自动平移命令；超出 viewport 部分被 D2D clip。
        ///
        /// 取景框模式：业务每帧把 widget 在 canvas 中的位置/大小当 viewport 传入，
        /// 命令仍按全屏 canvas 坐标 → 画面相对屏幕坐标固定，widget 移动就像"挪取景框"。
        ///
        /// **v0.7 新增**：outCanvasW / outCanvasH 写出当前画布尺寸（让业务做百分比布局）。
        /// 不需要这两个值的旧业务可以传 IntPtr.Zero 跳过。
        ///
        /// 重复 begin 不调 end 返 INVALID_PARAM。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_begin_frame(
            IntPtr handle,
            float viewportX,
            float viewportY,
            float viewportW,
            float viewportH,
            IntPtr outCanvasW,
            IntPtr outCanvasH);

        /// <summary>
        /// v0.7 重载：直接把画布尺寸读到 C# 的 out int 上（推荐用法）。
        /// 内部签名仍是 i32* 指针；C# unsafe 段把 &amp;canvasW 转 IntPtr 后转发到上面那条 P/Invoke。
        /// </summary>
        public static unsafe int renderer_begin_frame(
            IntPtr handle,
            float viewportX, float viewportY,
            float viewportW, float viewportH,
            out int canvasW, out int canvasH)
        {
            int cw = 0, ch = 0;
            int status = renderer_begin_frame(
                handle, viewportX, viewportY, viewportW, viewportH,
                (IntPtr)(&cw), (IntPtr)(&ch));
            canvasW = cw;
            canvasH = ch;
            return status;
        }

        /// <summary>清屏到指定颜色（premultiplied alpha：rgb ≤ a）。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_clear(
            IntPtr handle, float r, float g, float b, float a);

        /// <summary>实心矩形。坐标 = canvas-space pixel，颜色 premultiplied alpha。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_fill_rect(
            IntPtr handle,
            float x, float y, float w, float h,
            float r, float g, float b, float a);

        /// <summary>
        /// 单行文本（Segoe UI / NORMAL）。utf8 是 UTF-8 字节起点，utf8Len 是字节数（不含 NUL）。
        /// utf8Len = 0 视作空串。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_draw_text(
            IntPtr handle,
            IntPtr utf8, int utf8Len,
            float x, float y, float fontSize,
            float r, float g, float b, float a);

        /// <summary>
        /// 提交一帧 → EndDraw + Present(0, 0)。不返 mapped pointer。
        /// DComp 自动拉 swap chain 新内容做合成，C# 不需要做 readback / 同步。
        /// 必须先 begin_frame，否则返 INVALID_PARAM。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_end_frame(IntPtr handle);

        // ============================================================
        // v0.7 矢量图元（Phase 1）
        // ============================================================
        //
        // 全部在 begin_frame / end_frame 之间调用，否则返 INVALID_PARAM。
        // 颜色 premultiplied alpha [0, 1]，坐标 canvas-space pixel。
        // 详见 docs/spec/painter-abi-v0.7.md 第 2.3 节。

        /// <summary>v0.7 dash style 常量（与 painter.rs 同步）。</summary>
        public static class DashStyle
        {
            public const int Solid = 0;
            public const int Dash = 1;
            public const int Dot = 2;
            public const int DashDot = 3;
        }

        /// <summary>直线。dashStyle 用 <see cref="DashStyle"/> 常量；越界视为 Solid。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_draw_line(
            IntPtr handle,
            float x0, float y0,
            float x1, float y1,
            float strokeWidth,
            float r, float g, float b, float a,
            int dashStyle);

        /// <summary>
        /// 折线。points 是连续 [x0,y0,x1,y1,...] float 数组指针，pointCount = 点数（不是 float 数）。
        /// closed != 0 时首尾自动相接。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_draw_polyline(
            IntPtr handle,
            IntPtr points,
            int pointCount,
            float strokeWidth,
            float r, float g, float b, float a,
            int closed);

        /// <summary>矩形描边。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_stroke_rect(
            IntPtr handle,
            float x, float y, float w, float h,
            float strokeWidth,
            float r, float g, float b, float a);

        /// <summary>圆角矩形填充。radiusX != radiusY 时是椭圆角。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_fill_rounded_rect(
            IntPtr handle,
            float x, float y, float w, float h,
            float radiusX, float radiusY,
            float r, float g, float b, float a);

        /// <summary>圆角矩形描边。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_stroke_rounded_rect(
            IntPtr handle,
            float x, float y, float w, float h,
            float radiusX, float radiusY,
            float strokeWidth,
            float r, float g, float b, float a);

        /// <summary>椭圆填充（rx == ry 时是正圆）。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_fill_ellipse(
            IntPtr handle,
            float cx, float cy,
            float rx, float ry,
            float r, float g, float b, float a);

        /// <summary>椭圆描边。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_stroke_ellipse(
            IntPtr handle,
            float cx, float cy,
            float rx, float ry,
            float strokeWidth,
            float r, float g, float b, float a);

        /// <summary>
        /// 推矩形 clip 到栈，配对 <see cref="renderer_pop_clip"/>。
        /// 当前实现走 D2D PushAxisAlignedClip ALIASED，clip 边缘整像素。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_push_clip_rect(
            IntPtr handle,
            float x, float y, float w, float h);

        /// <summary>弹 clip 栈顶。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_pop_clip(IntPtr handle);

        /// <summary>
        /// 设置 2D 仿射变换。matrix 是 6 个 float 的指针：[m11, m12, m21, m22, dx, dy]。
        /// set_transform 后命令叠加该变换；reset_transform 恢复成 viewport 平移（不是 identity）。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_set_transform(
            IntPtr handle,
            IntPtr matrix);

        /// <summary>重置 transform 为 viewport 平移（v0.6 默认）。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_reset_transform(IntPtr handle);

        // ============================================================
        // 托管包装（避开每帧 P/Invoke marshal 数组的 alloc）
        // ============================================================

        /// <summary>
        /// renderer_draw_polyline 的便利包装。注意：每次调用都会 fix 一份 array 指针，
        /// 高频调用建议自己缓存 GCHandle，避免每帧 fix 开销。
        /// 用 float[] 而非 ReadOnlySpan&lt;float&gt; —— UWP / .NET Native 的 BCL 没自带 Span。
        /// </summary>
        public static int DrawPolyline(
            IntPtr handle,
            float[] points,
            float strokeWidth,
            float r, float g, float b, float a,
            bool closed)
        {
            if (points == null || points.Length == 0) return RENDERER_OK;
            if (points.Length % 2 != 0)
            {
                // 协议错误：points 必须成对
                return RENDERER_ERR_INVALID_PARAM;
            }
            int pointCount = points.Length / 2;
            unsafe
            {
                fixed (float* p = points)
                {
                    return renderer_draw_polyline(
                        handle, (IntPtr)p, pointCount,
                        strokeWidth, r, g, b, a, closed ? 1 : 0);
                }
            }
        }

        /// <summary>
        /// renderer_set_transform 的便利包装。matrix 必须 6 元素 [m11,m12,m21,m22,dx,dy]。
        /// </summary>
        public static int SetTransform(IntPtr handle, float[] matrix)
        {
            if (matrix == null || matrix.Length != 6) return RENDERER_ERR_INVALID_PARAM;
            unsafe
            {
                fixed (float* p = matrix)
                {
                    return renderer_set_transform(handle, (IntPtr)p);
                }
            }
        }

        /// <summary>
        /// 便利包装：托管 string → UTF-8 → 调 renderer_draw_text。
        /// 频繁调用建议自己缓存 byte[]，避免每帧 GetBytes alloc。
        /// </summary>
        public static int DrawText(
            IntPtr handle,
            string text,
            float x, float y, float fontSize,
            float r, float g, float b, float a)
        {
            if (string.IsNullOrEmpty(text))
            {
                return renderer_draw_text(handle, IntPtr.Zero, 0, x, y, fontSize, r, g, b, a);
            }
            byte[] bytes = Encoding.UTF8.GetBytes(text);
            unsafe
            {
                fixed (byte* p = bytes)
                {
                    return renderer_draw_text(
                        handle, (IntPtr)p, bytes.Length,
                        x, y, fontSize, r, g, b, a);
                }
            }
        }

        // ============================================================
        // v0.7 Phase 2/3：Bitmap + 外部纹理
        // ============================================================
        //
        // 句柄是不透明 uint，0 视作 invalid。生命周期由调用方维护：load/create_texture
        // 拿到的 handle 必须在 renderer_destroy 之前调 renderer_destroy_bitmap 释放，
        // 否则资源会留到 Renderer 整个生命周期结束。

        /// <summary>
        /// 从内存字节流解码（PNG/JPG/BMP/GIF/WEBP）→ ID2D1Bitmap。WIC 解码，BGRA8 premul。
        /// 失败：DECODE_FAIL（格式不识别）/ RESOURCE_LIMIT（slot 满）/ DEVICE_INIT（D2D 创建失败）。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_load_bitmap_from_memory(
            IntPtr handle,
            IntPtr bytes,
            int byteLen,
            out uint outBitmap);

        /// <summary>
        /// 从 UTF-8 路径解码（不带 NUL）。失败补 IO（文件读不到）。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_load_bitmap_from_file(
            IntPtr handle,
            IntPtr utf8Path,
            int pathLen,
            out uint outBitmap);

        /// <summary>
        /// 创建空可写纹理。format: 0=BGRA8 / 1=RGBA8 / 2=NV12（NV12 暂未支持，会返回 UNSUPPORTED_FORMAT）。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_create_texture(
            IntPtr handle,
            uint width,
            uint height,
            int format,
            out uint outBitmap);

        /// <summary>
        /// 上传一帧像素到 create_texture 出来的 bitmap。stride = 每行字节数。
        /// format 必须与 create 时一致（这里再传一次只是冗余校验）。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_update_texture(
            IntPtr handle,
            uint bitmap,
            IntPtr bytes,
            int byteLen,
            int stride,
            int format);

        /// <summary>查询 bitmap 像素尺寸（即 D2D 内部尺寸）。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_get_bitmap_size(
            IntPtr handle,
            uint bitmap,
            out uint outWidth,
            out uint outHeight);

        /// <summary>销毁 bitmap。已 destroy / 未知句柄 → RESOURCE_NOT_FOUND（调用方按需当 idempotent）。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_destroy_bitmap(
            IntPtr handle,
            uint bitmap);

        /// <summary>
        /// 把 bitmap 画到 canvas。src_* 全 0 → 整个 bitmap；否则裁子矩形。dst 在 canvas 坐标。
        /// interp_mode: 0=nearest, 1=linear。必须在 begin_frame / end_frame 之间调用。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_draw_bitmap(
            IntPtr handle,
            uint bitmap,
            float srcX, float srcY, float srcW, float srcH,
            float dstX, float dstY, float dstW, float dstH,
            float opacity,
            int interpMode);

        // ===== v0.7 phase 3 video（spec §4.1） =====

        /// <summary>
        /// 视频元数据。字段顺序与 Rust <c>VideoInfo</c> struct 一致（duration_ms, w, h, fps_num, fps_den）。
        /// </summary>
        [StructLayout(LayoutKind.Sequential)]
        public struct VideoInfo
        {
            public ulong DurationMs;
            public uint Width;
            public uint Height;
            public uint FpsNum;
            public uint FpsDen;
        }

        /// <summary>
        /// 打开本地视频文件。<paramref name="utf8Path"/> 不要求 NUL 终止；<paramref name="pathLen"/>
        /// 是字节数（不是字符数）。失败码：VIDEO_OPEN_FAIL / RESOURCE_LIMIT / INVALID_PARAM。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_video_open_file(
            IntPtr handle,
            IntPtr utf8Path,
            int pathLen,
            out uint outVideoHandle);

        /// <summary>查询视频元数据。video 已 close / 不存在 → VIDEO_NOT_FOUND。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_video_get_info(
            IntPtr handle,
            uint video,
            out VideoInfo outInfo);

        /// <summary>跳到指定毫秒位置。EOS 标志会清掉。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_video_seek(
            IntPtr handle,
            uint video,
            ulong timeMs);

        /// <summary>
        /// 解一帧到内部 bitmap，返回 BitmapHandle 与 EOF 标志（1=已 EOS）。
        /// 同 video 反复调返同一 BitmapHandle —— 用 <see cref="renderer_draw_bitmap"/> 画即可。
        /// **不要** 自行 destroy 这个 bitmap —— <see cref="renderer_video_close"/> 统一回收。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_video_present_frame(
            IntPtr handle,
            uint video,
            out uint outBitmap,
            out int outEof);

        /// <summary>关闭视频：内部 IMFSourceReader + bitmap slot 一起回收。video handle 即时失效。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_video_close(
            IntPtr handle,
            uint video);

        /// <summary>
        /// renderer_video_open_file 的 string 包装：自动 UTF-8 编码 + 临时 fixed 指针。
        /// path null/empty → INVALID_PARAM。
        /// </summary>
        public static int VideoOpenFile(IntPtr handle, string path, out uint outVideo)
        {
            outVideo = 0;
            if (string.IsNullOrEmpty(path)) return RENDERER_ERR_INVALID_PARAM;
            byte[] utf8 = System.Text.Encoding.UTF8.GetBytes(path);
            unsafe
            {
                fixed (byte* p = utf8)
                {
                    return renderer_video_open_file(handle, (IntPtr)p, utf8.Length, out outVideo);
                }
            }
        }

        /// <summary>
        /// renderer_load_bitmap_from_memory 的便利包装。data 不能为空，调用期间 fix 一份指针。
        /// </summary>
        public static int LoadBitmapFromMemory(IntPtr handle, byte[] data, out uint outBitmap)
        {
            outBitmap = 0;
            if (data == null || data.Length == 0) return RENDERER_ERR_INVALID_PARAM;
            unsafe
            {
                fixed (byte* p = data)
                {
                    return renderer_load_bitmap_from_memory(handle, (IntPtr)p, data.Length, out outBitmap);
                }
            }
        }

        /// <summary>
        /// renderer_update_texture 的便利包装。
        /// </summary>
        public static int UpdateTexture(IntPtr handle, uint bitmap, byte[] data, int stride, int format)
        {
            if (data == null || data.Length == 0) return RENDERER_ERR_INVALID_PARAM;
            unsafe
            {
                fixed (byte* p = data)
                {
                    return renderer_update_texture(handle, bitmap, (IntPtr)p, data.Length, stride, format);
                }
            }
        }

        /// <summary>
        /// renderer_load_bitmap_from_file 的便利包装。path UTF-8 编码后 fix 一份指针。
        /// </summary>
        public static int LoadBitmapFromFile(IntPtr handle, string path, out uint outBitmap)
        {
            outBitmap = 0;
            if (string.IsNullOrEmpty(path)) return RENDERER_ERR_INVALID_PARAM;
            byte[] bytes = Encoding.UTF8.GetBytes(path);
            unsafe
            {
                fixed (byte* p = bytes)
                {
                    return renderer_load_bitmap_from_file(handle, (IntPtr)p, bytes.Length, out outBitmap);
                }
            }
        }

        // ============================================================
        // 诊断
        // ============================================================

        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_get_perf_stats(
            IntPtr handle,
            out PerfStats outStats);

        /// <summary>拉取 Rust 端最近一条 ERROR 级别的日志。</summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern UIntPtr renderer_last_error_string(IntPtr buf, UIntPtr bufLen);

        public static string GetLastErrorString()
        {
            const int bufSize = 1024;
            IntPtr buf = Marshal.AllocHGlobal(bufSize);
            try
            {
                UIntPtr len = renderer_last_error_string(buf, (UIntPtr)bufSize);
                ulong actualLen = (ulong)len;
                if (actualLen == 0) return null;
                int copyLen = (int)Math.Min(actualLen, (ulong)(bufSize - 1));
                byte[] bytes = new byte[copyLen];
                Marshal.Copy(buf, bytes, 0, copyLen);
                return Encoding.UTF8.GetString(bytes);
            }
            finally
            {
                Marshal.FreeHGlobal(buf);
            }
        }
    }
}
