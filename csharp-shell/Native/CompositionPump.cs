using System;
using System.Diagnostics;
using System.Numerics;
using System.Runtime.InteropServices;
using System.Text;
using Windows.System.Threading;
using Windows.UI.Composition;
using Windows.UI.Core;
using Windows.UI.Xaml;
using Windows.UI.Xaml.Hosting;

namespace OverlayWidget.Native
{
    /// <summary>
    /// ICompositorInterop（Windows.UI.Composition.Interop）。
    /// 标准 native COM 接口（windows.ui.composition.interop.h），所有 Compositor 实例都实现。
    /// 进程内 QI 必然成功 —— widget host 跨进程代理拒绝过 ISwapChainPanelNative，但
    /// 这里 compositor 本身在 widget 进程，QI 不跨进程边界。
    ///
    /// vtable 顺序必须严格匹配头文件，否则方法槽位错位。
    /// </summary>
    [ComImport]
    [Guid("25297D5C-3AD4-4C9C-B5CF-E36A38512330")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface ICompositorInterop
    {
        // vtable[3]: HRESULT CreateCompositionSurfaceForHandle(HANDLE, ICompositionSurface**)
        void CreateCompositionSurfaceForHandle(
            IntPtr swapChainHandle,
            [MarshalAs(UnmanagedType.IInspectable)] out object surface);

        // vtable[4]: HRESULT CreateCompositionSurfaceForSwapChain(IUnknown*, ICompositionSurface**)
        void CreateCompositionSurfaceForSwapChain(
            IntPtr swapChainIUnknown,
            [MarshalAs(UnmanagedType.IInspectable)] out object surface);

        // vtable[5]: HRESULT CreateGraphicsDevice(IUnknown*, ICompositionGraphicsDevice**)
        void CreateGraphicsDevice(
            IntPtr renderingDevice,
            [MarshalAs(UnmanagedType.IInspectable)] out object device);
    }

    /// <summary>
    /// C# 端单帧 perf 滑窗统计（v0.6 DComp 路径下 readback/copy 字段语义改变）。
    /// 字段单位均为微秒。MainWidget 的 PerfInfo TextBlock 仍用同名字段显示。
    /// </summary>
    internal struct PumpPerfStats
    {
        /// <summary>v0.6: begin_frame → end_frame 全程耗时（含 cmd 推送 + EndDraw + Present 调用）—— us</summary>
        public ulong AvgAcquireUs;
        /// <summary>v0.6 已废弃，永远 0（DComp 不需要 readback）。保留字段名以兼容 MainWidget UI</summary>
        public ulong AvgReadbackUs;
        /// <summary>v0.6 已废弃，永远 0（DComp 不需要 memcpy）。保留字段名以兼容 MainWidget UI</summary>
        public ulong AvgCopyUs;
        /// <summary>整个 ThreadPool tick 耗时（含 GetWindowRect / monitor 检查 / Rust 调用）—— us</summary>
        public ulong AvgTickUs;
        public ulong PeakAcquireUs;
        public ulong PeakReadbackUs;
        public ulong PeakCopyUs;
        public ulong PeakTickUs;
        public ulong TickCount;
        public uint ValidSamples;
        public uint WindowSize;
    }

    /// <summary>
    /// v0.6 DComp 渲染 pump：把 Rust swap chain 包成 ICompositionSurface 挂到 OS visual tree，
    /// ThreadPoolTimer 后台线程驱动 Rust 渲染。
    ///
    /// ## 跟 BitmapRenderPump 的区别（关键 — 解决了 modal 拖动期间画面冻结问题）
    ///
    /// BitmapRenderPump 路径：
    /// <code>
    /// Rust 渲染 → CPU readback → memcpy 到 WriteableBitmap.PixelBuffer
    ///   → wb.Invalidate（必须 UI 线程）→ XAML compositor 拉 wb → DWM 合成 → 屏幕
    /// </code>
    /// 模态拖动时：UI 线程被 Win32 modal move loop 占住 → wb.Invalidate dispatch
    /// 排队 → XAML compositor 不工作 → 画面冻结直到松开。
    ///
    /// CompositionPump 路径：
    /// <code>
    /// Rust 渲染 → swap chain Present(0,0)
    ///   → ICompositionSurface（OS 级）→ DWM 内核合成器 → 屏幕
    /// </code>
    /// 模态拖动时：DWM 在内核线程持续合成，**绕开 XAML compositor 和 UI 线程**。
    /// ThreadPoolTimer 后台线程也不被 modal 阻塞。x 轴拖动期间画面持续刷新。
    ///
    /// ## 渲染 → 上屏路径
    /// <code>
    /// canvas (monitor 2560×1440)             - 业务命令的逻辑坐标系
    /// 业务命令仍按 canvas 坐标推（"屏幕中心画 Hello"）
    ///
    /// Rust 内部：
    ///   begin_frame(vpX, vpY, vpW, vpH)        ← C# 把 widget 屏幕矩形作为 viewport
    ///   SetTransform(translate(-vpX,-vpY))     ← 命令自动平移到 viewport-local
    ///   swap chain 大小 = (vpW, vpH)            ← 跟 widget 物理像素一致
    ///   Present(0, 0)                          ← DComp 拉新内容
    ///
    /// C# 端（ThreadPool 线程）：
    ///   只调 begin_frame / cmd_* / end_frame，不做 readback / memcpy / Invalidate
    /// </code>
    ///
    /// ## widget 大小变化
    /// - SpriteVisual.Size 跟 host element ActualSize 同步（XAML 拉伸时 visual 跟着拉）
    /// - swap chain 物理大小由 Rust begin_frame 自动 ResizeBuffers 到 widget 物理像素
    /// - 视觉效果：widget 任意 resize，画面 1:1 渲染到正确像素，不会被 brush stretch 变形
    ///
    /// ## 不泄漏的关键设计
    /// 1. swap chain IUnknown 一次 AddRef 给 ICompositionSurface 持有；C# 拿到后立即 Release，
    ///    surface 自己保 ref；Stop() 释放 surface → swap chain ref count 归 0 → swap chain
    ///    由 Rust renderer_destroy 释放
    /// 2. SpriteVisual / SurfaceBrush / Surface 这些 WinRT RCW 由 GC 管，Stop() 时显式置 null
    /// 3. ThreadPoolTimer 用 Cancel() 停，不依赖 Dispose pattern
    /// </summary>
    internal sealed class CompositionPump : IDisposable
    {
        // host element：visual 挂在它上，size 跟它走
        private readonly FrameworkElement _hostElement;
        // _hostElement.Dispatcher 缓存（构造时取，跨线程 dispatch monitor mismatch 用）
        private CoreDispatcher _dispatcher;

        // 渲染参数
        private IntPtr _rendererHandle = IntPtr.Zero;
        private IntPtr _hwnd = IntPtr.Zero;       // widget CoreWindow HWND
        private uint _canvasW;                    // canvas 逻辑宽（业务命令坐标系）
        private uint _canvasH;
        // 缓存上一次发现的 monitor 矩形 size，与 _canvasW/_canvasH 不一致时 fire OnMonitorMismatch
        private int _lastMonW;
        private int _lastMonH;

        // Composition 资源
        private SpriteVisual _spriteVisual;
        private CompositionSurfaceBrush _brush;
        private ICompositionSurface _surface;

        private readonly object _gate = new object();
        private bool _running;
        // ThreadPool periodic timer 在前一 tick 没结束时也可能触发；用这个标志做防重入
        private volatile bool _renderInProgress;

        /// <summary>
        /// 检测到当前 widget 所在显示器尺寸与 canvas 尺寸不一致时触发（widget 拖到不同显示器、
        /// 或显示器分辨率被系统改了）。host (MainWidget) 在回调里应：
        /// 1. <c>renderer_resize(handle, monW, monH)</c>
        /// 2. <c>pump.SetCanvas(monW, monH)</c>
        /// callback dispatch 到 UI 线程触发。
        /// </summary>
        public Action<uint, uint> OnMonitorMismatch;

        // ---------- C# 端 Perf 滑窗 ----------
        private const int PerfWindow = 60;
        private readonly long[] _acquireSamples = new long[PerfWindow];  // begin→end 全程
        private readonly long[] _tickSamples = new long[PerfWindow];     // 整个 OnBgTick 耗时
        private int _sampleIdx;
        private uint _validPerfSamples;
        private long _peakAcquireUs;
        private long _peakTickUs;
        private ulong _tickCount;
        private readonly object _perfGate = new object();

        // ThreadPool periodic timer。CreatePeriodicTimer 不被 modal move loop 阻塞，
        // 在专用 ThreadPool 线程持续 fire（间隔由 OS 调度，不严格等同 16ms）
        private ThreadPoolTimer _bgTimer;
        private static readonly TimeSpan BgTickInterval = TimeSpan.FromMilliseconds(16);

        private long _startTicks;
        private static readonly byte[] s_helloUtf8 = Encoding.UTF8.GetBytes("Hello, Overlay!");

        public CompositionPump(FrameworkElement hostElement)
        {
            _hostElement = hostElement ?? throw new ArgumentNullException(nameof(hostElement));
            _dispatcher = hostElement.Dispatcher;
        }

        /// <summary>
        /// 启动 pump。必须在 UI 线程调（涉及 ElementCompositionPreview / Compositor 创建）。
        /// </summary>
        /// <param name="rendererHandle">renderer_create 返的 handle</param>
        /// <param name="widgetHwnd">widget CoreWindow HWND；每 tick GetWindowRect 用</param>
        /// <param name="canvasW">canvas 逻辑宽（=显示器物理像素，业务命令坐标系参考）</param>
        /// <param name="canvasH">canvas 逻辑高</param>
        /// <exception cref="InvalidOperationException">renderer_get_swapchain / 互操作失败</exception>
        public void Start(IntPtr rendererHandle, IntPtr widgetHwnd, uint canvasW, uint canvasH)
        {
            if (rendererHandle == IntPtr.Zero) throw new ArgumentNullException(nameof(rendererHandle));
            if (canvasW == 0 || canvasH == 0) throw new ArgumentException("canvas size zero");
            if (_dispatcher == null) _dispatcher = _hostElement.Dispatcher;

            // 1. 拿 swap chain IUnknown（Rust AddRef 一次给我们）
            int st = Renderer.renderer_get_swapchain(rendererHandle, out IntPtr swapChainUnk);
            if (st != Renderer.RENDERER_OK || swapChainUnk == IntPtr.Zero)
            {
                throw new InvalidOperationException(
                    $"renderer_get_swapchain failed: status={st}");
            }

            try
            {
                // 2. 拿 host element 所在的 Compositor（widget 进程内的 XAML compositor）
                Visual hostVisual = ElementCompositionPreview.GetElementVisual(_hostElement);
                Compositor compositor = hostVisual.Compositor;

                // 3. QI ICompositorInterop → CreateCompositionSurfaceForSwapChain
                // ICompositorInterop 是 native COM 接口；compositor RCW QI 在进程内必成功。
                ICompositorInterop interop = (ICompositorInterop)(object)compositor;
                interop.CreateCompositionSurfaceForSwapChain(swapChainUnk, out object surfaceObj);
                _surface = (ICompositionSurface)surfaceObj;

                // 4. 创建 SurfaceBrush + SpriteVisual + 挂到 host element
                _brush = compositor.CreateSurfaceBrush(_surface);
                // swap chain 物理像素 = widget 物理像素（begin_frame 时 ResizeBuffers），
                // visual size = host element 物理像素（DIP × scale），1:1 显示。
                // 用 Fill 让 surface 完全填充 visual（如果 swap chain 大小落后于 visual，
                // 这一帧会有轻微 stretch；下一帧 ResizeBuffers 后恢复 1:1）。
                _brush.Stretch = CompositionStretch.Fill;

                _spriteVisual = compositor.CreateSpriteVisual();
                _spriteVisual.Brush = _brush;
                // 初始 size 用 host element ActualSize；后续 SizeChanged 时同步更新
                UpdateVisualSize(_hostElement.ActualWidth, _hostElement.ActualHeight);

                // SetElementChildVisual 把 spriteVisual 挂到 _hostElement 之上（不替换 XAML 内容）
                ElementCompositionPreview.SetElementChildVisual(_hostElement, _spriteVisual);
            }
            finally
            {
                // ICompositionSurface 内部已对 swap chain AddRef，这里释放 Rust 给的那一份
                Marshal.Release(swapChainUnk);
            }

            // 5. 监听 host element 大小变化（widget resize / DPI 改 / 系统 scale 变）
            _hostElement.SizeChanged += OnHostElementSizeChanged;

            // 6. 启动渲染参数
            lock (_gate)
            {
                _rendererHandle = rendererHandle;
                _hwnd = widgetHwnd;
                _canvasW = canvasW;
                _canvasH = canvasH;
                _lastMonW = (int)canvasW;
                _lastMonH = (int)canvasH;
                _running = true;
            }
            _startTicks = Stopwatch.GetTimestamp();

            // 7. 启动后台 timer
            StartBgTimer();

            Debug.WriteLine(
                $"[CompositionPump] started: canvas={canvasW}x{canvasH} hwnd=0x{widgetHwnd.ToInt64():X} hostSize={_hostElement.ActualWidth:F0}x{_hostElement.ActualHeight:F0}");
        }

        /// <summary>
        /// 显示器分辨率改变（或 widget 移到不同显示器）时调，更新 canvas 尺寸。
        /// 调用前应先 <c>renderer_resize</c>。swap chain 大小由下次 begin_frame 按 viewport 自动重建。
        /// </summary>
        public void SetCanvas(uint canvasW, uint canvasH)
        {
            if (canvasW == 0 || canvasH == 0) return;
            lock (_gate)
            {
                if (!_running) return;
                _canvasW = canvasW;
                _canvasH = canvasH;
                _lastMonW = (int)canvasW;
                _lastMonH = (int)canvasH;
            }
            Debug.WriteLine($"[CompositionPump] SetCanvas {canvasW}x{canvasH}");
        }

        public void Stop()
        {
            lock (_gate)
            {
                _running = false;
                _rendererHandle = IntPtr.Zero;
                _hwnd = IntPtr.Zero;
            }
            StopBgTimer();

            try
            {
                if (_hostElement != null)
                {
                    _hostElement.SizeChanged -= OnHostElementSizeChanged;
                    // 解除 visual 挂载（host element 仍在 XAML 树）
                    ElementCompositionPreview.SetElementChildVisual(_hostElement, null);
                }
            }
            catch (Exception ex) { Debug.WriteLine($"[CompositionPump] unhook visual threw: {ex.Message}"); }

            // 清掉 RCW 引用 — surface ref 归 0 → 释放 swap chain ref → Rust 端 swap chain
            // 仍由 renderer_destroy 持有最后一份 ref
            try { _spriteVisual?.Dispose(); } catch { }
            try { _brush?.Dispose(); } catch { }
            try { (_surface as IDisposable)?.Dispose(); } catch { }
            _spriteVisual = null;
            _brush = null;
            _surface = null;
        }

        public void Dispose() => Stop();

        // ---------- Visual size 同步（永远在 UI 线程；SizeChanged 也是 UI 线程） ----------

        private void UpdateVisualSize(double dipW, double dipH)
        {
            SpriteVisual sv = _spriteVisual;
            if (sv == null) return;
            float w = (float)Math.Max(1.0, dipW);
            float h = (float)Math.Max(1.0, dipH);
            sv.Size = new Vector2(w, h);
        }

        private void OnHostElementSizeChanged(object sender, SizeChangedEventArgs e)
        {
            UpdateVisualSize(e.NewSize.Width, e.NewSize.Height);
        }

        // ---------- ThreadPool timer ----------

        private void StartBgTimer()
        {
            if (_bgTimer != null) return;
            _bgTimer = ThreadPoolTimer.CreatePeriodicTimer(OnBgTick, BgTickInterval);
        }

        private void StopBgTimer()
        {
            if (_bgTimer == null) return;
            try { _bgTimer.Cancel(); } catch { }
            _bgTimer = null;
        }

        // ---------- 帧驱动（ThreadPool 线程） ----------

        /// <summary>
        /// ThreadPoolTimer 周期 callback（后台线程）：调一帧 begin/cmd/end。
        ///
        /// 步骤：
        /// 1. lock(_gate) 拿一致 handle / hwnd / canvas 快照
        /// 2. GetWindowRect / MonitorFromWindow（Win32 同步，跨线程 OK）
        /// 3. monitor 不一致 → dispatch <see cref="OnMonitorMismatch"/> 到 UI；跳帧
        /// 4. 算 viewport (vpX, vpY, vpW, vpH)
        /// 5. begin_frame → 推 canvas-space 命令 → end_frame
        ///
        /// 不再做 readback / memcpy / Invalidate —— DComp 自动拉 swap chain 内容合成。
        /// modal 拖动期间这条 callback 仍在 ThreadPool 线程跑，画面持续刷新。
        /// </summary>
        private void OnBgTick(ThreadPoolTimer timer)
        {
            if (_renderInProgress) return;

            IntPtr handle;
            IntPtr hwnd;
            uint canvasW, canvasH;
            int lastMonW, lastMonH;
            lock (_gate)
            {
                if (!_running || _rendererHandle == IntPtr.Zero) return;
                handle = _rendererHandle;
                hwnd = _hwnd;
                canvasW = _canvasW;
                canvasH = _canvasH;
                lastMonW = _lastMonW;
                lastMonH = _lastMonH;
            }
            if (hwnd == IntPtr.Zero || canvasW == 0 || canvasH == 0) return;

            // 取 widget 屏幕矩形 + 显示器矩形（任意线程 OK）
            if (!ScreenInterop.TryGetWindowScreenRect(hwnd, out var winRect)) return;
            if (!ScreenInterop.TryGetMonitorRectForWindow(hwnd, out var monRect)) return;

            int monW = monRect.Width;
            int monH = monRect.Height;
            // 显示器尺寸变了（拖到别的屏 / 系统改了分辨率）→ dispatch 通知 host 调整 canvas
            if (monW != (int)canvasW || monH != (int)canvasH)
            {
                if (monW != lastMonW || monH != lastMonH)
                {
                    lock (_gate) { _lastMonW = monW; _lastMonH = monH; }
                    Debug.WriteLine(
                        $"[CompositionPump] monitor size mismatch: canvas={canvasW}x{canvasH} mon={monW}x{monH}, dispatching host resize");
                    var cb = OnMonitorMismatch;
                    if (cb != null && _dispatcher != null)
                    {
                        uint mw = (uint)monW;
                        uint mh = (uint)monH;
                        _ = _dispatcher.RunAsync(CoreDispatcherPriority.Normal, () =>
                        {
                            try { cb(mw, mh); }
                            catch (Exception ex) { Debug.WriteLine($"[CompositionPump] OnMonitorMismatch threw: {ex}"); }
                        });
                    }
                }
                return;
            }

            // viewport in canvas-space。允许负值（widget 出屏），Rust 端 D2D 会 clip。
            float vpX = winRect.left - monRect.left;
            float vpY = winRect.top - monRect.top;
            int viewW = winRect.Width;
            int viewH = winRect.Height;
            if (viewW <= 0 || viewH <= 0) return;

            long tickT0 = Stopwatch.GetTimestamp();
            long acquireUs = 0;
            bool sampleValid = false;

            _renderInProgress = true;
            try
            {
                long t = Stopwatch.GetTimestamp();
                int status = Renderer.renderer_begin_frame(handle, vpX, vpY, viewW, viewH);
                if (status != Renderer.RENDERER_OK)
                {
                    Debug.WriteLine($"[CompositionPump] begin_frame failed: status={status}");
                    return;
                }

                bool frameOpen = true;
                try
                {
                    DrawDebugContent(handle, canvasW, canvasH);

                    status = Renderer.renderer_end_frame(handle);
                    frameOpen = false;
                    acquireUs = ToMicros(Stopwatch.GetTimestamp() - t);
                    if (status != Renderer.RENDERER_OK)
                    {
                        Debug.WriteLine($"[CompositionPump] end_frame failed: status={status}");
                        return;
                    }
                    sampleValid = true;
                }
                catch (Exception ex)
                {
                    Debug.WriteLine($"[CompositionPump] tick body threw: {ex}");
                    if (frameOpen)
                    {
                        // begin 后必须 end，否则下一帧 cmd_drawing 仍 true
                        try { Renderer.renderer_end_frame(handle); } catch { }
                    }
                }
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[CompositionPump] OnBgTick threw: {ex}");
            }
            finally
            {
                _renderInProgress = false;
                if (sampleValid)
                {
                    long tickUs = ToMicros(Stopwatch.GetTimestamp() - tickT0);
                    RecordPerf(acquireUs, tickUs);
                }
            }
        }

        /// <summary>
        /// 取景框模式调试内容：在 canvas（屏幕分辨率）坐标系绘制。
        /// "Hello, Overlay!" 永远位于屏幕几何中心；widget 任何位置移动都看到同一个画面相对位置。
        ///
        /// Rust 端 SetTransform 已把命令平移到 viewport-local，业务这边按"屏幕坐标"推就行。
        /// 命令在 viewport 外的部分被 D2D 自动 clip。
        /// </summary>
        private void DrawDebugContent(IntPtr handle, uint canvasW, uint canvasH)
        {
            float cw = canvasW;
            float ch = canvasH;
            double tSec = (double)(Stopwatch.GetTimestamp() - _startTicks) / Stopwatch.Frequency;
            float tF = (float)tSec;
            float intensity = (float)(Math.Sin(tF * Math.PI * 2.0 * 0.6) * 0.25 + 0.75);

            // 1) 全透明清屏（清的是 swap chain back buffer，即 viewport 区域）
            Renderer.renderer_clear(handle, 0f, 0f, 0f, 0f);

            // 2) 半透明深色底（玻璃感）覆盖整屏；widget 看到的子区域才有这层底
            const float bgAlpha = 0.30f;
            float bgRgb = 0.05f * bgAlpha;
            Renderer.renderer_fill_rect(handle, 0f, 0f, cw, ch, bgRgb, bgRgb, bgRgb, bgAlpha);

            // 3) 屏幕上下色带
            const float bandAlpha = 0.55f;
            float bandR = 0.10f * intensity * bandAlpha;
            float bandG = 0.20f * intensity * bandAlpha;
            float bandB = 0.45f * intensity * bandAlpha;
            float bandH = Math.Min(Math.Max(ch * 0.015f, 2f), 8f);
            Renderer.renderer_fill_rect(handle, 0f, 0f, cw, bandH, bandR, bandG, bandB, bandAlpha);
            Renderer.renderer_fill_rect(handle, 0f, ch - bandH, cw, bandH, bandR, bandG, bandB, bandAlpha);

            // 3b) 屏幕中心十字定位线（轻量、便于肉眼验证 widget 移动时画面相对屏幕固定）
            const float crossAlpha = 0.40f;
            float crossR = 0.30f * crossAlpha;
            float crossG = 0.30f * crossAlpha;
            float crossB = 0.30f * crossAlpha;
            float crossLen = Math.Min(cw, ch) * 0.10f;
            float crossThick = 2f;
            Renderer.renderer_fill_rect(handle,
                cw * 0.5f - crossLen * 0.5f, ch * 0.5f - crossThick * 0.5f,
                crossLen, crossThick, crossR, crossG, crossB, crossAlpha);
            Renderer.renderer_fill_rect(handle,
                cw * 0.5f - crossThick * 0.5f, ch * 0.5f - crossLen * 0.5f,
                crossThick, crossLen, crossR, crossG, crossB, crossAlpha);

            // 4) "Hello, Overlay!" 屏幕几何中心
            float fontSize = Math.Min(Math.Max(ch * 0.06f, 24f), 96f);
            float estTextW = fontSize * s_helloUtf8.Length * 0.55f;
            float textX = cw * 0.5f - estTextW * 0.5f;
            float textY = ch * 0.5f - fontSize * 0.5f;
            unsafe
            {
                fixed (byte* p = s_helloUtf8)
                {
                    Renderer.renderer_draw_text(
                        handle, (IntPtr)p, s_helloUtf8.Length,
                        textX, textY, fontSize, 1f, 1f, 1f, 0.92f);
                }
            }

            // 5) 副信息：canvas 尺寸 + 时间 + 标识符（确认 v0.6 版本部署成功）
            string info = $"canvas {canvasW}x{canvasH} | t={tSec:F1}s | dcomp";
            float infoSize = Math.Min(Math.Max(fontSize * 0.30f, 14f), 28f);
            Renderer.DrawText(
                handle, info,
                textX, textY + fontSize * 1.15f, infoSize,
                0.85f, 0.85f, 0.85f, 0.70f);
        }

        // ---------- Perf 记录 / 拉取 ----------

        private static long ToMicros(long stopwatchTicks)
        {
            if (stopwatchTicks <= 0) return 0;
            return (stopwatchTicks * 1_000_000L) / Stopwatch.Frequency;
        }

        private void RecordPerf(long acquireUs, long tickUs)
        {
            lock (_perfGate)
            {
                _acquireSamples[_sampleIdx] = acquireUs;
                _tickSamples[_sampleIdx] = tickUs;
                _sampleIdx = (_sampleIdx + 1) % PerfWindow;
                if (_validPerfSamples < PerfWindow) _validPerfSamples++;
                _tickCount++;
                if (acquireUs > _peakAcquireUs) _peakAcquireUs = acquireUs;
                if (tickUs > _peakTickUs) _peakTickUs = tickUs;
            }
        }

        public PumpPerfStats GetPerfStats()
        {
            lock (_perfGate)
            {
                if (_validPerfSamples == 0)
                {
                    return new PumpPerfStats
                    {
                        WindowSize = PerfWindow,
                        TickCount = _tickCount,
                    };
                }
                long acqSum = 0, tickSum = 0;
                int n = (int)_validPerfSamples;
                for (int i = 0; i < n; i++)
                {
                    acqSum += _acquireSamples[i];
                    tickSum += _tickSamples[i];
                }
                return new PumpPerfStats
                {
                    AvgAcquireUs = (ulong)(acqSum / n),
                    AvgReadbackUs = 0,           // v0.6 DComp 不需要 readback
                    AvgCopyUs = 0,               // v0.6 DComp 不需要 memcpy
                    AvgTickUs = (ulong)(tickSum / n),
                    PeakAcquireUs = (ulong)_peakAcquireUs,
                    PeakReadbackUs = 0,
                    PeakCopyUs = 0,
                    PeakTickUs = (ulong)_peakTickUs,
                    TickCount = _tickCount,
                    ValidSamples = _validPerfSamples,
                    WindowSize = PerfWindow,
                };
            }
        }
    }
}
