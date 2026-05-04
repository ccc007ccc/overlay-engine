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
        /// 重复 begin 不调 end 返 INVALID_PARAM。
        /// </summary>
        [DllImport(Dll, CallingConvention = CallingConvention.StdCall, ExactSpelling = true)]
        public static extern int renderer_begin_frame(
            IntPtr handle,
            float viewportX,
            float viewportY,
            float viewportW,
            float viewportH);

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
