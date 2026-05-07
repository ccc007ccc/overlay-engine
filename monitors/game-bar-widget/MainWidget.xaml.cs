using System;
using System.Diagnostics;
using System.Globalization;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading.Tasks;
using Windows.ApplicationModel.DataTransfer;
using Windows.ApplicationModel.Resources;
using Windows.Foundation;
using Windows.Storage;
using Windows.UI.Core;
using Windows.UI.Xaml;
using Windows.UI.Xaml.Controls;
using Windows.UI.Xaml.Input;
using Windows.UI.Xaml.Media;
using Windows.UI.Xaml.Navigation;
using Microsoft.Gaming.XboxGameBar;
using OverlayWidget.Native;

namespace OverlayWidget
{
    /// <summary>
    /// Game Bar widget 内容页（v0.6 DComp 路径）。
    ///
    /// 职责：
    /// 1. <see cref="CompositionPump"/> 把 Rust swap chain 包成 ICompositionSurface 挂到
    ///    OS visual tree（绕开 XAML compositor）；ThreadPoolTimer 后台驱动 Rust 渲染
    /// 2. SizeChanged 时刷状态文本（v0.6 取景框模式 widget 大小不再触发 renderer_resize；
    ///    SpriteVisual.Size 由 pump 内部跟 Surface 同步）
    /// 3. hover-only 齿轮按钮 + Flyout 设置面板：Maximize / Reset / Custom resize / Auto-calibrate
    ///
    /// 不参与每帧绘制 -- 渲染循环全部在 CompositionPump 后台线程完成。
    ///
    /// 路径阶段：
    /// - 旧 R3：SwapChainPanel + ISwapChainPanelNative（widget host 跨进程代理拒绝该 native COM 接口）
    /// - 旧 V1-V3：MediaPlayer / WriteableBitmap + CPU readback（modal 拖动期间画面冻结）
    /// - **当前 v0.6**：CreateSwapChainForComposition + ICompositionSurface →
    ///   ElementCompositionPreview.SetElementChildVisual 挂到 Surface visual 上；
    ///   DWM 内核合成器直接显示，**modal 不阻塞**
    ///
    /// i18n：
    /// - 静态文本（XAML）通过 x:Uid 绑定 Strings/<lang>/Resources.resw
    /// - 动态格式化字符串（含数字/状态）通过 ResourceLoader.GetString + string.Format
    ///
    /// 跨屏幕兼容：
    /// - Auto-calibrate 二分扫出实际 SDK cap，存进 LocalSettings
    /// - Maximize 优先用 LocalSettings 里的 calibrated 值，没有时用 GetSystemMetrics 估算
    /// </summary>
    public sealed partial class MainWidget : Page
    {
        // 默认还原尺寸（DIP）-- 与 manifest <Window><Size>1280x720</Size> 对齐
        private const double DefaultWidth = 1280;
        private const double DefaultHeight = 720;

        // GetSystemMetrics 估算值的 reserve（DIP），仅作 fallback
        private const double SideReserve = 5;
        private const double VerticalReserve = 80;

        private const int SM_CXSCREEN = 0;
        private const int SM_CYSCREEN = 1;

        [DllImport("user32.dll")]
        private static extern int GetSystemMetrics(int nIndex);

        // LocalSettings 持久化的 key
        private const string LSKey_CalibratedW = "CalibratedW";
        private const string LSKey_CalibratedH = "CalibratedH";

        // 资源加载器：进程级 cache
        private static readonly ResourceLoader Loader = ResourceLoader.GetForViewIndependentUse();

        // ---------- Rust 日志回调 ----------
        // delegate 必须 static + 永久 keep-alive，否则 GC 会回收，
        // Rust 端持有的函数指针就变 dangling。
        private static readonly Renderer.LogCallback _logCallback = OnRendererLog;
        // 简单 singleton 路由：callback 是 static，但要把日志送到具体 widget 实例。
        // widget 是 single-instance，这个简化是 OK 的。
        private static MainWidget _activeInstance;

        private IntPtr _renderer = IntPtr.Zero;
        private CompositionPump _pump;
        private XboxGameBarWidget _widget;
        private bool _calibrating;
        private Size? _calibratedSize; // null = 没校准过

        // ---------- SizeChanged 防抖 ----------
        // 校准期间 widget 反复 resize（二分查找时短时间触发数十次），每次都
        // renderer_resize + pump.Resize（重建 MediaPlayer pipeline）会让 player
        // 卡在"启动中"反复死掉 —— 校准结束后画面再也不来。所以 resize 改防抖：
        // 连续 SizeChanged 期间只记录最新尺寸，停 150ms 没变化才真正生效。
        private DispatcherTimer _resizeDebounceTimer;
        private int _pendingResizeW;
        private int _pendingResizeH;

        // ---------- Perf 仪表盘 ----------
        // 1Hz 拉 renderer_get_perf_stats，把 v0.6 渲染管线耗时显示在 Flyout。
        //
        // v0.6 字段语义：
        // - AvgRenderUs：begin_frame→end_frame 之间所有 cmd_* + EndDraw 累积耗时
        // - AvgReadbackUs（字段名沿用）：Present(0,0) 调用耗时（CPU 端，不等 GPU 完成）
        //
        // C# 端（CompositionPump）：
        // - AvgAcquireUs：整个一帧 begin→end 的 P/Invoke 总耗时
        // - AvgReadbackUs / AvgCopyUs：v0.6 不需要，永远 0
        // - AvgTickUs：整个 OnBgTick callback 耗时（含 GetWindowRect / monitor 检测）
        //
        // 颜色阈值（avg_render_us）：
        //   < 8000us  绿（远低于 60fps 阈值）
        //   8000-12000  默认（健康）
        //   12000-15000 黄（开始紧张）
        //   >= 15000    红（接近 60fps 16667us 阈值）
        private DispatcherTimer _perfTimer;
        private const ulong PerfThresholdHealthyUs = 8000;
        private const ulong PerfThresholdWarnUs = 12000;
        private const ulong PerfThresholdDangerUs = 15000;

        // SolidColorBrush 缓存避免每秒 alloc
        private static readonly SolidColorBrush PerfBrushHealthy = new SolidColorBrush(Windows.UI.Color.FromArgb(0xCC, 0x66, 0xDD, 0x77));
        private static readonly SolidColorBrush PerfBrushNormal = new SolidColorBrush(Windows.UI.Color.FromArgb(0xCC, 0xFF, 0xFF, 0xFF));
        private static readonly SolidColorBrush PerfBrushWarn = new SolidColorBrush(Windows.UI.Color.FromArgb(0xCC, 0xFF, 0xCC, 0x44));
        private static readonly SolidColorBrush PerfBrushDanger = new SolidColorBrush(Windows.UI.Color.FromArgb(0xCC, 0xFF, 0x66, 0x66));

        public MainWidget()
        {
            InitializeComponent();
            Loaded += OnLoaded;
            Unloaded += OnUnloaded;
            Surface.SizeChanged += OnSizeChanged;

            // 注册 Rust 日志回调 —— 必须在 renderer_create 之前，这样 init 期间的
            // emit 也能被转发到 UI（用来诊断 SwapChainInit 这种早期错误）
            _activeInstance = this;
            try
            {
                Renderer.renderer_set_log_callback(_logCallback);
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayWidget] register log callback failed: {ex.Message}");
            }

            // 初始文本（XAML 默认值用占位，避免编辑器与运行时不一致；启动后立即覆盖成本地化版本）
            SizeInfo.Text = Loader.GetString("Status_SizeInitial");
            MaxInfo.Text = Loader.GetString("Status_MaxInitial");
            LastResult.Text = Loader.GetString("Status_LastInitial");
            RendererInfo.Text = Loader.GetString("Status_RendererNotAttached");
            PerfInfo.Text = Loader.GetString("Perf_Initial");

            // ToolTip 通过代码 set（attached property 在 .resw 里的 syntax 太复杂）
            ToolTipService.SetToolTip(SettingsBtn, Loader.GetString("Tooltip_Settings"));
            ToolTipService.SetToolTip(ReadCurrentBtn, Loader.GetString("Tooltip_ReadCurrent"));
            // 阶段1：复制诊断按钮的 tooltip
            ToolTipService.SetToolTip(CopyDiagBtn, Loader.GetString("Tooltip_CopyDiagnostics"));

            // 读取 LocalSettings 中已保存的 calibrated 值（如果有）
            _calibratedSize = ReadCalibratedFromLocalSettings();
            UpdateCalibratedInfo();
        }

        // ---------- Rust 日志回调实现 ----------

        /// <summary>
        /// Rust 通过 C ABI 把日志吐过来：(level, *utf8_msg)。
        /// 我们手解 UTF-8（UWP BCL 没有 Marshal.PtrToStringUTF8）后写到 Debug + LastResult 行。
        ///
        /// 注意：callback 可能从 Rust 渲染线程触发，必须 dispatch 回 UI 线程更新文本。
        /// 在 renderer_create 期间触发的 emit 是 UI 线程同一栈，dispatch 也安全。
        /// </summary>
        private static void OnRendererLog(int level, IntPtr utf8MsgPtr)
        {
            if (utf8MsgPtr == IntPtr.Zero) return;

            string msg;
            try
            {
                int len = 0;
                while (Marshal.ReadByte(utf8MsgPtr, len) != 0) len++;
                if (len == 0) return;
                byte[] bytes = new byte[len];
                Marshal.Copy(utf8MsgPtr, bytes, 0, len);
                msg = Encoding.UTF8.GetString(bytes);
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayWidget] log decode failed: {ex.Message}");
                return;
            }

            string levelTag;
            switch (level)
            {
                case 0: levelTag = "TRACE"; break;
                case 1: levelTag = "DEBUG"; break;
                case 2: levelTag = "INFO"; break;
                case 3: levelTag = "WARN"; break;
                case 4: levelTag = "ERROR"; break;
                default: levelTag = "L" + level; break;
            }
            Debug.WriteLine($"[Rust:{levelTag}] {msg}");

            // 推到 UI 线程显示
            MainWidget inst = _activeInstance;
            if (inst == null) return;
            CoreDispatcher dispatcher = inst.Dispatcher;
            if (dispatcher == null) return;

            string captured = msg;
            string capturedTag = levelTag;
            // fire-and-forget；callback 不能 block Rust 调用方
            _ = dispatcher.RunAsync(CoreDispatcherPriority.Normal, () =>
            {
                if (inst.LastResult != null)
                {
                    // 截断过长消息以免撑爆 UI
                    string trimmed = captured.Length > 240 ? captured.Substring(0, 240) + "..." : captured;
                    inst.LastResult.Text = $"[{capturedTag}] {trimmed}";
                }
            });
        }

        protected override void OnNavigatedTo(NavigationEventArgs e)
        {
            base.OnNavigatedTo(e);
            _widget = e.Parameter as XboxGameBarWidget;
            UpdateMaxInfo();
        }

        // ---------- Hover 显隐（阶段1：从 Root 移到 SettingsBtn 自身）----------
        // v0.4 改：hover 监听从 SettingsBtn 移到 Root Grid。Root.Background=Transparent
        // 接收 hit-test；指针进 widget 区域 fade in 按钮；指针离开 widget 区域 fade out。
        // pinned 模式下 Game Bar host 自动屏蔽 widget hit-test，不影响游戏 click 透传。

        private void OnRootPointerEntered(object sender, PointerRoutedEventArgs e)
        {
            FadeInStory.Begin();
        }

        private void OnRootPointerExited(object sender, PointerRoutedEventArgs e)
        {
            FadeOutStory.Begin();
        }

        // ---------- Renderer 接入 ----------

        private void OnLoaded(object sender, RoutedEventArgs e)
        {
            UpdateSizeInfo();
            UpdateMaxInfo();
            try
            {
                AttachRenderer();
            }
            catch (DllNotFoundException ex)
            {
                Debug.WriteLine($"[OverlayWidget] renderer.dll not found: {ex.Message}");
                RendererInfo.Text = Loader.GetString("Status_RendererDllMissing");
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayWidget] renderer init exception: {ex}");
                RendererInfo.Text = string.Format(
                    Loader.GetString("Status_RendererInitFailed"), ex.GetType().Name);
            }

            // 阶段1: 订阅 XamlRoot.Changed 处理 DPI / scale 变化
            // SizeChanged 只在 DIP 尺寸变时触发；DPI 改变时 DIP 不变但 px 变化，
            // 单靠 SizeChanged 无法 catch 用户改 Windows 显示设置 → widget 内部 px 错位。
            // XamlRoot.Changed 在 RasterizationScale / Size / Visible / Content 任一变化时都 fire。
            TrySubscribeXamlRootChanged();
        }

        private void AttachRenderer()
        {
            // v0.6 DComp 取景框模式：renderer canvas = 屏幕物理分辨率，与 widget 大小/位置完全解耦。
            // CompositionPump 把 Rust swap chain 挂到 OS visual tree；ThreadPoolTimer 每 tick
            // 用 HWND 屏幕矩形作为 viewport 推帧。modal 拖动不阻塞渲染。
            //
            // 历史路径都失败：R3 SwapChainPanel + ISwapChainPanelNative 跨进程代理拒绝；
            // V1/V2 MediaPlayer + MediaStreamSource 内存泄漏；V3 Pinned + WriteableBitmap
            // modal 期间冻结。当前 v0.6 是第一次让 modal 期间也能持续刷新的路径。

            // 1. 取 widget CoreWindow HWND（pump 每 tick 用它调 GetWindowRect）
            IntPtr hwnd = ScreenInterop.GetCoreWindowHwnd();
            if (hwnd == IntPtr.Zero)
            {
                Debug.WriteLine("[OverlayWidget] GetCoreWindowHwnd returned 0 — viewport mode requires HWND");
                RendererInfo.Text = "Attach failed: GetCoreWindowHwnd returned 0";
                return;
            }

            // 2. 取 widget 所在显示器的物理像素矩形（canvas 尺寸）
            if (!ScreenInterop.TryGetMonitorRectForWindow(hwnd, out var monRect))
            {
                Debug.WriteLine("[OverlayWidget] TryGetMonitorRectForWindow failed");
                RendererInfo.Text = "Attach failed: cannot resolve monitor rect";
                return;
            }
            int canvasW = Math.Max(1, monRect.Width);
            int canvasH = Math.Max(1, monRect.Height);
            Debug.WriteLine(
                $"[OverlayWidget] AttachRenderer: hwnd=0x{hwnd.ToInt64():X} monitor={monRect}");

            // 3. renderer_create 用 canvas 尺寸（不再用 widget 像素）
            int status = Renderer.renderer_create(canvasW, canvasH, out _renderer);
            if (status != Renderer.RENDERER_OK)
            {
                string detail = null;
                try { detail = Renderer.GetLastErrorString(); }
                catch (Exception ex) { Debug.WriteLine($"[OverlayWidget] GetLastErrorString threw: {ex.Message}"); }

                Debug.WriteLine($"[OverlayWidget] renderer_create failed: status={status}, detail={detail}");
                _renderer = IntPtr.Zero;

                string baseText = string.Format(
                    Loader.GetString("Status_RendererCreateFailed"), status);
                if (!string.IsNullOrEmpty(detail))
                {
                    if (detail.Length > 8000) detail = detail.Substring(0, 8000) + "...";
                    RendererInfo.Text = baseText + "\n" + detail;
                }
                else
                {
                    RendererInfo.Text = baseText;
                }
                return;
            }

            // 4. 启动 CompositionPump（DComp 路径）
            //    host element = Surface（XAML 中是 Border 控件作为 visual 锚点；
            //    Image 没有 Source 时 ActualSize=0 会让 spriteVisual 缩成 1x1 看不见）
            try
            {
                _pump = new CompositionPump(Surface);
                _pump.OnMonitorMismatch = OnMonitorMismatchFromPump;
                _pump.Start(_renderer, hwnd, (uint)canvasW, (uint)canvasH);
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayWidget] CompositionPump.Start threw: {ex}");
                RendererInfo.Text = "Attach failed: CompositionPump init: " + ex.GetType().Name + " " + ex.Message;
                if (_renderer != IntPtr.Zero) { Renderer.renderer_destroy(_renderer); _renderer = IntPtr.Zero; }
                _pump = null;
                return;
            }

            StartPerfTimer();

            RendererInfo.Text = string.Format(
                Loader.GetString("Status_RendererAttached"), canvasW, canvasH);
        }

        /// <summary>
        /// pump 每 tick 检测到 widget 当前所在显示器与 canvas 尺寸不一致时触发（拖到别的显示器、
        /// 或系统改了显示分辨率）。共用 _resizeDebounceTimer 做 150ms 防抖，连续触发只在停顿后真正
        /// 调 renderer_resize + pump.SetCanvas。
        /// </summary>
        private void OnMonitorMismatchFromPump(uint newCanvasW, uint newCanvasH)
        {
            if (_renderer == IntPtr.Zero) return;
            _pendingResizeW = (int)newCanvasW;
            _pendingResizeH = (int)newCanvasH;
            if (_resizeDebounceTimer == null)
            {
                _resizeDebounceTimer = new DispatcherTimer
                {
                    Interval = TimeSpan.FromMilliseconds(150),
                };
                _resizeDebounceTimer.Tick += OnResizeDebounceTick;
            }
            _resizeDebounceTimer.Stop();
            _resizeDebounceTimer.Start();
            Debug.WriteLine(
                $"[OverlayWidget] OnMonitorMismatchFromPump scheduled canvas resize -> {newCanvasW}x{newCanvasH}");
        }

        private void OnSizeChanged(object sender, SizeChangedEventArgs e)
        {
            // v0.5 取景框模式：renderer canvas 尺寸 = 显示器物理像素，与 widget 大小完全解耦；
            // widget resize 不再触发 renderer_resize。
            // - widget 的 wb 大小由 pump 每 tick 用 GetWindowRect 自动跟踪
            // - 显示器分辨率变化由 pump 检测后 fire OnMonitorMismatch，host 这边走 _resizeDebounceTimer 防抖
            UpdateSizeInfo();
        }

        /// <summary>
        /// 防抖到期：拿最后一次记录的目标 canvas 尺寸（来自 <see cref="OnMonitorMismatchFromPump"/>），
        /// 调 <c>renderer_resize</c> + <c>pump.SetCanvas</c>。
        ///
        /// 顺序至关重要：先让 Rust 端把 RT/staging 重建到新尺寸；再让 pump 更新缓存的 canvas，
        /// 让下一帧 begin_frame 走新尺寸。两边 size 一致后 pump 的 mismatch 不再触发。
        /// </summary>
        private void OnResizeDebounceTick(object sender, object e)
        {
            _resizeDebounceTimer?.Stop();
            if (_renderer == IntPtr.Zero) return;
            int w = _pendingResizeW;
            int h = _pendingResizeH;
            if (w <= 0 || h <= 0) return;

            int status = Renderer.renderer_resize(_renderer, w, h);
            if (status != Renderer.RENDERER_OK)
            {
                Debug.WriteLine($"[OverlayWidget] renderer_resize failed: status={status}");
                return;
            }
            _pump?.SetCanvas((uint)w, (uint)h);
            Debug.WriteLine($"[OverlayWidget] canvas resize applied (debounced): {w}x{h}");
        }

        // ---------- 阶段1: XamlRoot.Changed -> DPI / scale 自适应 ----------

        /// <summary>
        /// 在 OnLoaded 时订阅；OnUnloaded 时退订。XamlRoot 在 widget 加入树后才有值，
        /// 所以构造函数里订阅会 NRE。订阅本身的异常都 swallow，订阅失败不应该让 widget 崩。
        /// </summary>
        private void TrySubscribeXamlRootChanged()
        {
            if (XamlRoot == null) return;
            try { XamlRoot.Changed += OnXamlRootChanged; }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayWidget] subscribe XamlRoot.Changed failed: {ex.Message}");
            }
        }

        private void TryUnsubscribeXamlRootChanged()
        {
            if (XamlRoot == null) return;
            try { XamlRoot.Changed -= OnXamlRootChanged; }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayWidget] unsubscribe XamlRoot.Changed failed: {ex.Message}");
            }
        }

        /// <summary>
        /// XamlRoot.Changed 触发时机：RasterizationScale / Size / IsHostVisible / Content 任一变化。
        /// v0.5 取景框模式下 canvas 由 pump 每 tick 自动检测显示器尺寸变化触发 callback，
        /// 这里只刷新 UI 文本（SizeInfo 里的 px 数字依赖 scale，scale 变了要刷）。
        /// </summary>
        private void OnXamlRootChanged(XamlRoot sender, XamlRootChangedEventArgs args)
        {
            UpdateSizeInfo();
            Debug.WriteLine(
                $"[OverlayWidget] XamlRoot.Changed scale={(sender?.RasterizationScale ?? 1.0):F2}");
        }

        private void OnUnloaded(object sender, RoutedEventArgs e)
        {
            // 顺序：先停 perf timer + resize 防抖 + pump（解除 visual + 停后台 timer），
            // 再 destroy renderer。反过来会让 ThreadPool tick 调到 destroy 后的 handle 而崩。
            TryUnsubscribeXamlRootChanged();
            StopPerfTimer();
            if (_resizeDebounceTimer != null)
            {
                _resizeDebounceTimer.Stop();
                _resizeDebounceTimer.Tick -= OnResizeDebounceTick;
                _resizeDebounceTimer = null;
            }
            if (_pump != null)
            {
                _pump.Dispose();
                _pump = null;
            }
            if (_renderer != IntPtr.Zero)
            {
                Renderer.renderer_destroy(_renderer);
                _renderer = IntPtr.Zero;
            }
        }

        // ---------- Perf 仪表盘 ----------

        private void StartPerfTimer()
        {
            if (_perfTimer != null) return;
            _perfTimer = new DispatcherTimer
            {
                Interval = TimeSpan.FromSeconds(1),
            };
            _perfTimer.Tick += OnPerfTick;
            _perfTimer.Start();
        }

        private void StopPerfTimer()
        {
            if (_perfTimer == null) return;
            _perfTimer.Stop();
            _perfTimer.Tick -= OnPerfTick;
            _perfTimer = null;
        }

        /// <summary>
        /// 1Hz 定时器：拉 renderer_get_perf_stats + pump.GetPerfStats，把 Rust GPU 渲染数据 +
        /// C# pump tick (acquire / readback / copy) 合并写入 PerfInfo TextBlock。
        ///
        /// V3 路径下 readback 是单帧最大耗时（CreateCopyFromSurfaceAsync ~3-5ms），
        /// 加 C# 端数据后能区分 GPU 渲染快/慢 vs 整个 pump tick 快/慢。
        /// 颜色阈值仍以 Rust GPU avg_render_us 判断（V2 GPU 不含 readback）。
        /// 数字用 InvariantCulture 格式化避免本地化 decimal separator 干扰。
        /// </summary>
        private void OnPerfTick(object sender, object e)
        {
            if (_renderer == IntPtr.Zero || PerfInfo == null) return;

            int status = Renderer.renderer_get_perf_stats(_renderer, out Renderer.PerfStats stats);
            if (status != Renderer.RENDERER_OK)
            {
                Debug.WriteLine($"[OverlayWidget] get_perf_stats failed: status={status}");
                return;
            }

            // 阶段1: 拉取 C# pump 端 perf 滑窗（acquire / readback / copy / total tick）
            // pump 可能为 null（Renderer 创建失败但 timer 已启动的窗口期），用 default(0) 兜底
            PumpPerfStats pumpStats = _pump != null ? _pump.GetPerfStats() : default;

            if (stats.ValidSamples == 0 || stats.TotalFrames == 0)
            {
                PerfInfo.Text = Loader.GetString("Perf_FormatNoData");
                PerfInfo.Foreground = PerfBrushNormal;
                return;
            }

            // us → ms（保留两位小数）
            string renderMs = (stats.AvgRenderUs / 1000.0).ToString("F2", CultureInfo.InvariantCulture);
            string readbackMs = (stats.AvgReadbackUs / 1000.0).ToString("F2", CultureInfo.InvariantCulture);
            string totalMs = (stats.AvgTotalUs / 1000.0).ToString("F2", CultureInfo.InvariantCulture);
            string peakRenderMs = (stats.PeakRenderUs / 1000.0).ToString("F2", CultureInfo.InvariantCulture);
            string peakReadbackMs = (stats.PeakReadbackUs / 1000.0).ToString("F2", CultureInfo.InvariantCulture);
            // C# pump 端
            string tickMs = (pumpStats.AvgTickUs / 1000.0).ToString("F2", CultureInfo.InvariantCulture);
            string csReadbackMs = (pumpStats.AvgReadbackUs / 1000.0).ToString("F2", CultureInfo.InvariantCulture);
            string csCopyMs = (pumpStats.AvgCopyUs / 1000.0).ToString("F2", CultureInfo.InvariantCulture);
            string csPeakTickMs = (pumpStats.PeakTickUs / 1000.0).ToString("F2", CultureInfo.InvariantCulture);

            // 11 个 args 对应 .resw Perf_Format {0..10}（参见 .resw 注释字段说明）
            PerfInfo.Text = string.Format(
                Loader.GetString("Perf_Format"),
                renderMs,                                                       // {0} Rust GPU avg
                readbackMs,                                                     // {1} Rust readback avg (V2=0)
                totalMs,                                                        // {2} Rust total avg
                stats.ValidSamples.ToString(CultureInfo.InvariantCulture),      // {3} 滑窗样本数
                stats.TotalFrames.ToString(CultureInfo.InvariantCulture),       // {4} Rust 总帧数
                peakRenderMs,                                                   // {5} Rust GPU peak
                peakReadbackMs,                                                 // {6} Rust readback peak (V2=0)
                tickMs,                                                         // {7} C# tick avg
                csReadbackMs,                                                   // {8} C# readback avg
                csCopyMs,                                                       // {9} C# copy avg
                csPeakTickMs);                                                  // {10} C# tick peak

            // 颜色阈值：V2 颜色判 GPU 渲染时延（不是含 readback 的总时延，V2 readback=0）
            ulong total = stats.AvgRenderUs;
            SolidColorBrush brush;
            if (total < PerfThresholdHealthyUs) brush = PerfBrushHealthy;
            else if (total < PerfThresholdWarnUs) brush = PerfBrushNormal;
            else if (total < PerfThresholdDangerUs) brush = PerfBrushWarn;
            else brush = PerfBrushDanger;
            PerfInfo.Foreground = brush;
        }

        // ---------- Maximize / Reset ----------

        /// <summary>
        /// Maximize 目标。优先级：
        ///   1. LocalSettings 里上次 calibrate 的结果（最准）
        ///   2. GetSystemMetrics + reserve（估算 fallback）
        ///   3. 1880x980 固定值（最坏情况）
        ///
        /// 注：Game Bar 顶部 Home Bar 会占用一段垂直空间，widget 接近全屏时
        /// CenterWindowAsync 无法物理居中（widget 会被推下方）。这是 SDK 限制。
        /// 用户接受"宁可拿全尺寸不要居中"，所以这里不做 vertical 缩减。
        /// </summary>
        private Size ComputeMaximizeTarget()
        {
            double dipW;
            double dipH;

            // 1. 优先用上次校准结果
            if (_calibratedSize.HasValue)
            {
                Size c = _calibratedSize.Value;
                dipW = c.Width;
                dipH = c.Height;
            }
            else
            {
                // 2. GetSystemMetrics 估算
                double scale = XamlRoot?.RasterizationScale ?? 1.0;
                try
                {
                    int physW = GetSystemMetrics(SM_CXSCREEN);
                    int physH = GetSystemMetrics(SM_CYSCREEN);
                    if (physW <= 0 || physH <= 0) throw new InvalidOperationException("metrics zero");
                    dipW = physW / scale - SideReserve;
                    dipH = physH / scale - VerticalReserve;
                }
                catch (Exception ex)
                {
                    Debug.WriteLine($"[OverlayWidget] GetSystemMetrics failed, using fallback: {ex.Message}");
                    // 3. 最坏情况
                    dipW = 1880;
                    dipH = 980;
                }
            }

            // 不能小于 default
            dipW = Math.Max(DefaultWidth, dipW);
            dipH = Math.Max(DefaultHeight, dipH);

            // 不能超过 manifest 上限
            if (_widget != null)
            {
                Size m = _widget.MaxWindowSize;
                if (m.Width > 0) dipW = Math.Min(dipW, m.Width);
                if (m.Height > 0) dipH = Math.Min(dipH, m.Height);
            }

            return new Size(dipW, dipH);
        }

        private async void OnMaximize(object sender, RoutedEventArgs e)
        {
            if (_widget == null) return;

            Size target = ComputeMaximizeTarget();
            try
            {
                bool ok = await _widget.TryResizeWindowAsync(target);
                Debug.WriteLine($"[OverlayWidget] Maximize -> {target.Width:F0}x{target.Height:F0} DIP, ok={ok}");
                try { await _widget.CenterWindowAsync(); }
                catch (Exception cex) { Debug.WriteLine($"[OverlayWidget] Maximize-Center threw: {cex.Message}"); }
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayWidget] Maximize threw: {ex}");
            }
            UpdateSizeInfo();
        }

        private async void OnRestoreDefault(object sender, RoutedEventArgs e)
        {
            if (_widget == null) return;
            Size target = new Size(DefaultWidth, DefaultHeight);
            try
            {
                bool ok = await _widget.TryResizeWindowAsync(target);
                Debug.WriteLine($"[OverlayWidget] Reset -> {target.Width:F0}x{target.Height:F0} DIP, ok={ok}");
                try { await _widget.CenterWindowAsync(); }
                catch (Exception cex) { Debug.WriteLine($"[OverlayWidget] Reset-Center threw: {cex.Message}"); }
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayWidget] Reset threw: {ex}");
            }
            UpdateSizeInfo();
        }

        // ---------- Custom resize ----------

        private async void OnCustomApply(object sender, RoutedEventArgs e)
        {
            if (_widget == null)
            {
                LastResult.Text = Loader.GetString("Last_WidgetNull");
                return;
            }
            // CultureInfo.InvariantCulture：用户输入数字，避免本地化 decimal separator 影响
            if (!double.TryParse(CustomW?.Text, NumberStyles.Number, CultureInfo.InvariantCulture, out double w) || w <= 0 ||
                !double.TryParse(CustomH?.Text, NumberStyles.Number, CultureInfo.InvariantCulture, out double h) || h <= 0)
            {
                LastResult.Text = Loader.GetString("Last_InvalidInput");
                return;
            }

            Size target = new Size(w, h);
            try
            {
                bool ok = await _widget.TryResizeWindowAsync(target);
                Debug.WriteLine($"[OverlayWidget] CustomApply -> {w:F0}x{h:F0} DIP, ok={ok}");
                LastResult.Text = string.Format(
                    Loader.GetString("Last_RequestResult"),
                    ((int)w).ToString(), ((int)h).ToString(), ok);
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayWidget] CustomApply threw: {ex}");
                LastResult.Text = string.Format(Loader.GetString("Last_Threw"), ex.GetType().Name);
            }
            UpdateSizeInfo();
        }

        private void OnReadCurrent(object sender, RoutedEventArgs e)
        {
            if (CustomW != null) CustomW.Text = ((int)Surface.ActualWidth).ToString(CultureInfo.InvariantCulture);
            if (CustomH != null) CustomH.Text = ((int)Surface.ActualHeight).ToString(CultureInfo.InvariantCulture);
        }

        // ---------- Auto-calibrate ----------

        private async void OnCalibrate(object sender, RoutedEventArgs e)
        {
            if (_widget == null)
            {
                CalibrateProgress.Text = Loader.GetString("Calibrate_WidgetNull");
                return;
            }
            if (_calibrating) return;
            _calibrating = true;
            CalibrateBtn.IsEnabled = false;

            Size original = new Size(Surface.ActualWidth, Surface.ActualHeight);

            try
            {
                int physW = 0, physH = 0;
                try { physW = GetSystemMetrics(SM_CXSCREEN); physH = GetSystemMetrics(SM_CYSCREEN); } catch { }
                double scale = XamlRoot?.RasterizationScale ?? 1.0;
                int screenDipW = physW > 0 ? (int)(physW / scale) : 4096;
                int screenDipH = physH > 0 ? (int)(physH / scale) : 4096;

                int loInitW = (int)DefaultWidth;
                int loInitH = (int)DefaultHeight;

                CalibrateProgress.Text = string.Format(
                    Loader.GetString("Calibrate_Phase1Header"), loInitW, screenDipW);
                int probesW = 0;
                int maxW = await BinarySearchMaxAsync(
                    loInit: loInitW,
                    hiInit: screenDipW,
                    fixedOther: 500,
                    isWidth: true,
                    onProbe: (mid, ok, lo, hi) =>
                    {
                        probesW++;
                        CalibrateProgress.Text = string.Format(
                            Loader.GetString("Calibrate_Phase1Probe"),
                            mid, ok, lo, hi, probesW);
                    });

                CalibrateProgress.Text = string.Format(
                    Loader.GetString("Calibrate_Phase2Header"), maxW, loInitH, screenDipH);
                int probesH = 0;
                int maxH = await BinarySearchMaxAsync(
                    loInit: loInitH,
                    hiInit: screenDipH,
                    fixedOther: maxW,
                    isWidth: false,
                    onProbe: (mid, ok, lo, hi) =>
                    {
                        probesH++;
                        CalibrateProgress.Text = string.Format(
                            Loader.GetString("Calibrate_Phase2Probe"),
                            maxW, mid, ok, lo, hi, probesH);
                    });

                bool finalOk = await SafeTryResize(maxW, maxH);
                try { await _widget.CenterWindowAsync(); }
                catch (Exception cex) { Debug.WriteLine($"[OverlayWidget] Calibrate-Center threw: {cex.Message}"); }

                // 持久化
                _calibratedSize = new Size(maxW, maxH);
                WriteCalibratedToLocalSettings(maxW, maxH);

                int reserveW = screenDipW - maxW;
                int reserveH = screenDipH - maxH;
                CalibrateProgress.Text = string.Format(
                    Loader.GetString("Calibrate_Done"),
                    maxW, maxH, finalOk, reserveW, reserveH, probesW, probesH);

                if (CustomW != null) CustomW.Text = maxW.ToString(CultureInfo.InvariantCulture);
                if (CustomH != null) CustomH.Text = maxH.ToString(CultureInfo.InvariantCulture);

                UpdateCalibratedInfo();
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayWidget] Calibrate threw: {ex}");
                CalibrateProgress.Text = string.Format(
                    Loader.GetString("Calibrate_Error"), ex.GetType().Name, ex.Message);
                try { await _widget.TryResizeWindowAsync(original); } catch { }
            }
            finally
            {
                _calibrating = false;
                CalibrateBtn.IsEnabled = true;
                UpdateSizeInfo();
            }
        }

        /// <summary>
        /// 二分查找最大可接受值。回调 onProbe 用来更新 UI 进度。
        /// </summary>
        private async Task<int> BinarySearchMaxAsync(
            int loInit, int hiInit, int fixedOther, bool isWidth,
            Action<int, bool, int, int> onProbe)
        {
            int lo = loInit;
            int hi = hiInit;

            // 先 probe 一下 hi —— 罕见但万一可达直接结束
            bool hiOk = await SafeTryResize(isWidth ? hi : fixedOther, isWidth ? fixedOther : hi);
            onProbe(hi, hiOk, lo, hi);
            if (hiOk) return hi;

            while (hi - lo > 1)
            {
                int mid = lo + (hi - lo) / 2;
                bool ok = await SafeTryResize(isWidth ? mid : fixedOther, isWidth ? fixedOther : mid);
                onProbe(mid, ok, lo, hi);
                if (ok) lo = mid;
                else hi = mid - 1;
            }
            return lo;
        }

        private async Task<bool> SafeTryResize(int w, int h)
        {
            try
            {
                return await _widget.TryResizeWindowAsync(new Size(w, h));
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayWidget] SafeTryResize {w}x{h} threw: {ex.Message}");
                return false;
            }
        }

        // ---------- LocalSettings 持久化 ----------

        private static Size? ReadCalibratedFromLocalSettings()
        {
            try
            {
                var values = ApplicationData.Current.LocalSettings.Values;
                if (values.TryGetValue(LSKey_CalibratedW, out object wObj) &&
                    values.TryGetValue(LSKey_CalibratedH, out object hObj) &&
                    wObj is int w && hObj is int h && w > 0 && h > 0)
                {
                    return new Size(w, h);
                }
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayWidget] ReadCalibratedFromLocalSettings threw: {ex.Message}");
            }
            return null;
        }

        private static void WriteCalibratedToLocalSettings(int w, int h)
        {
            try
            {
                var values = ApplicationData.Current.LocalSettings.Values;
                values[LSKey_CalibratedW] = w;
                values[LSKey_CalibratedH] = h;
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayWidget] WriteCalibratedToLocalSettings threw: {ex.Message}");
            }
        }

        // ---------- Status 文本更新 ----------

        private void UpdateSizeInfo()
        {
            if (SizeInfo == null) return;
            double scale = XamlRoot?.RasterizationScale ?? 1.0;
            double dipW = Surface.ActualWidth;
            double dipH = Surface.ActualHeight;
            int pxW = (int)(dipW * scale);
            int pxH = (int)(dipH * scale);
            // 参数都先 ToString 把数字格式化好（避免 .resw 里写 {0:F0} 之类 culture-sensitive 的格式串）
            SizeInfo.Text = string.Format(
                Loader.GetString("Status_SizeFormat"),
                ((int)dipW).ToString(), ((int)dipH).ToString(),
                pxW.ToString(), pxH.ToString(),
                scale.ToString("F2", CultureInfo.InvariantCulture));
        }

        private void UpdateMaxInfo()
        {
            if (MaxInfo == null || _widget == null) return;
            Size mn = _widget.MinWindowSize;
            Size mx = _widget.MaxWindowSize;
            int physW = 0, physH = 0;
            try { physW = GetSystemMetrics(SM_CXSCREEN); physH = GetSystemMetrics(SM_CYSCREEN); } catch { }
            Size target = ComputeMaximizeTarget();
            MaxInfo.Text = string.Format(
                Loader.GetString("Status_MaxFormat"),
                ((int)mx.Width).ToString(), ((int)mx.Height).ToString(),
                ((int)mn.Width).ToString(), ((int)mn.Height).ToString(),
                physW.ToString(), physH.ToString(),
                ((int)target.Width).ToString(), ((int)target.Height).ToString());
        }

        private void UpdateCalibratedInfo()
        {
            if (CalibratedInfo == null) return;
            if (_calibratedSize.HasValue)
            {
                Size c = _calibratedSize.Value;
                CalibratedInfo.Text = string.Format(
                    Loader.GetString("Status_CalibratedFormat"),
                    ((int)c.Width).ToString(), ((int)c.Height).ToString());
            }
            else
            {
                CalibratedInfo.Text = Loader.GetString("Status_CalibratedNone");
            }
        }

        // ---------- 阶段1: 复制诊断信息 ----------

        /// <summary>
        /// 把 Flyout 内所有诊断字段一次性拷到剪贴板，方便用户粘贴反馈。
        /// 内容包括：Perf / Size / Max-Target / Calibrated / LastResult / Renderer log。
        ///
        /// 异常路径：Clipboard.SetContent 偶尔会因 widget 进程权限抛 COMException
        /// （UWP AppContainer 沙盒里 clipboard 写权限是受控 capability），
        /// 用 try/catch 把错误反馈到 LastResult，不让 widget 崩。
        ///
        /// DataPackage + Clipboard.SetContent 是 UWP 标准剪贴板 API，
        /// Game Bar widget 进程默认有 clipboard 写权限。
        /// </summary>
        private void OnCopyDiagnostics(object sender, RoutedEventArgs e)
        {
            try
            {
                StringBuilder sb = new StringBuilder(2048);
                sb.AppendLine("=== Overlay Widget Diagnostics ===");
                sb.Append("Time:  ").AppendLine(
                    DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss", CultureInfo.InvariantCulture));
                sb.AppendLine();
                sb.AppendLine("--- Perf ---");
                sb.AppendLine(PerfInfo?.Text ?? "(null)");
                sb.AppendLine();
                sb.AppendLine("--- Size ---");
                sb.AppendLine(SizeInfo?.Text ?? "(null)");
                sb.AppendLine();
                sb.AppendLine("--- Max / Target ---");
                sb.AppendLine(MaxInfo?.Text ?? "(null)");
                sb.AppendLine();
                sb.AppendLine("--- Calibrated ---");
                sb.AppendLine(CalibratedInfo?.Text ?? "(null)");
                sb.AppendLine();
                sb.AppendLine("--- Last result ---");
                sb.AppendLine(LastResult?.Text ?? "(null)");
                sb.AppendLine();
                sb.AppendLine("--- Renderer log ---");
                sb.AppendLine(RendererInfo?.Text ?? "(null)");
                string text = sb.ToString();

                DataPackage package = new DataPackage();
                package.SetText(text);
                Clipboard.SetContent(package);

                if (LastResult != null)
                {
                    LastResult.Text = string.Format(
                        Loader.GetString("Last_DiagnosticsCopied"),
                        text.Length.ToString(CultureInfo.InvariantCulture));
                }
                Debug.WriteLine($"[OverlayWidget] OnCopyDiagnostics -> {text.Length} chars copied");
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayWidget] OnCopyDiagnostics threw: {ex}");
                if (LastResult != null)
                {
                    LastResult.Text = string.Format(
                        Loader.GetString("Last_DiagnosticsCopyFailed"),
                        ex.GetType().Name);
                }
            }
        }
    }
}
