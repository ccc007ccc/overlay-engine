using System;
using System.Diagnostics;
using System.Numerics;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using Windows.Foundation;
using Windows.System.Threading;
using Windows.UI.Composition;
using Windows.UI.Core;
using Windows.UI.Xaml;
using Windows.UI.Xaml.Hosting;
using Windows.UI.Xaml.Media;

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

        // ---------- v0.7.1 host element 物理像素 + 偏移缓存 ----------
        // 拉伸 bug 根因：widget HWND winRect 物理像素 ≠ Surface.ActualSize × RasterizationScale
        // 物理像素 —— Game Bar host 在 widget 客户区与外层 HWND 之间存在 DPI/transform mismatch。
        // 修复方案：swap chain back buffer 严格跟 host element (Surface Border) 物理像素 1:1，
        // 而不是用 GetWindowRect。host element 物理像素由 SizeChanged callback 在 UI 线程同步缓存
        // （TransformToVisual + RasterizationScale），OnBgTick 在 ThreadPool 读 Volatile 缓存。
        // 0 表示尚未同步过（首帧 fallback 到 winRect）。
        private int _hostPhysW;
        private int _hostPhysH;
        private int _hostOffsetX;   // host element 在 widget HWND 客户区内的物理像素 X 偏移
        private int _hostOffsetY;   // host element 在 widget HWND 客户区内的物理像素 Y 偏移

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
        private static readonly byte[] s_showcaseUtf8 = Encoding.UTF8.GetBytes("v0.7 primitives");
        private static readonly byte[] s_phase2Utf8 = Encoding.UTF8.GetBytes(
            "v0.7 phase 2 bitmap: BGRA8 | BGRA8 anim | RGBA8 swizzle | nearest|linear");
        private static readonly byte[] s_phase5Utf8 = Encoding.UTF8.GetBytes(
            "v0.7 phase 5: SVG path (fill/stroke) | linear gradient | radial gradient");

        // 复用缓冲：polyline 三角形点（每帧原地改写，避免 GC alloc）
        private readonly float[] _triPts = new float[6];
        // 复用缓冲：2D 仿射矩阵 [m11,m12,m21,m22,dx,dy]
        private readonly float[] _xform = new float[6];

        // Phase 5：path byte buffer 复用（每帧重写，避免 GC alloc）
        // 五角星最大用量：5 × (1 MOVE_TO + 4 LINE_TO) + 1 CLOSE ≈ 46 bytes。留足空间避免扩容。
        private readonly byte[] _phase5PathBuf = new byte[128];
        // linear gradient stops（2 个，黑→蓝）：10 floats
        private readonly float[] _phase5LinearStops = new float[10];
        // radial gradient stops（3 个，红→黄→透明）：15 floats
        private readonly float[] _phase5RadialStops = new float[15];

        // v0.7 phase 2：bitmap showcase（首次 Start 创建 handle，每帧 update + draw，Stop 销毁）
        private Phase2BitmapShowcase _phase2;

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
                // 诊断结论(2026-05):vp=winRect=host_phys 三者完全相等,证明 swap chain 物理
                // 与 visual 物理已经 1:1 对齐 — 改 Stretch 模式不是拉伸 bug 的根因。
                // 改回 Fill(在物理像素 1:1 时跟 None 等价,但 Fill 能正确处理 DPI scale)。
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

            // 8. v0.7 phase 2：创建 bitmap showcase（4 个 16x16 / 4x4 bitmap，
            //    每帧 update + draw，验证 create_texture / update_texture / draw_bitmap 全链路）
            _phase2 = new Phase2BitmapShowcase();
            if (!_phase2.Create(rendererHandle))
            {
                Debug.WriteLine("[CompositionPump] phase2 bitmap showcase init failed; rendering will skip phase2 panel");
                _phase2 = null;
            }

            // 9. 拉伸 bug 修复：立即同步 host element 物理像素 + 偏移到缓存。
            //    SizeChanged 在 Start 之后才会触发首次（layout pass 完成后），首帧 OnBgTick
            //    可能会先于 SizeChanged 跑 —— 在这里先同步一次，避免首帧 fallback 到 winRect。
            SyncHostElementMetrics();

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
            // 拿当前 handle 用于释放 phase2 资源；与 _running=false 之间没有竞态，
            // 因为外层调用 Stop() 已确保 OnBgTick 不会再用 phase2（_running 守卫）
            IntPtr handleForCleanup;
            lock (_gate)
            {
                handleForCleanup = _rendererHandle;
                _running = false;
                _rendererHandle = IntPtr.Zero;
                _hwnd = IntPtr.Zero;
            }
            StopBgTimer();

            // 释放 phase2 bitmap handle —— 必须在 renderer_destroy 之前，否则 slot
            // 还没归还就被整个 Renderer 一起 drop 了，虽然 COM Release 自然会跑，
            // 但显式 destroy 让生命周期日志干净（不会 leak slot 计数到下次 Start）
            if (_phase2 != null && handleForCleanup != IntPtr.Zero)
            {
                _phase2.Destroy(handleForCleanup);
                _phase2 = null;
            }

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

        /// <summary>
        /// UI 线程同步 host element 的物理像素 size + 在 widget 客户区内的物理像素偏移。
        ///
        /// 拉伸 bug 修复关键：Game Bar widget 进程内 <see cref="ScreenInterop.TryGetWindowScreenRect"/>
        /// 拿到的 winRect 物理像素**不一定** == host element 实际渲染物理像素 ——
        /// XAML compositor 与 Win32 GetWindowRect 之间存在 DPI/transform 失配。
        /// swap chain back buffer 必须严格跟 host element 物理像素 1:1，否则
        /// SurfaceBrush.Stretch=Fill 会按二者比例横纵向不等量拉伸。
        ///
        /// 调用时机：SizeChanged callback、Start() 末尾首次同步、XamlRoot.Changed (DPI 变)。
        /// 跨线程读取走 <see cref="Volatile"/>（只读 4 字节 int，单调一致即可）。
        /// </summary>
        private void SyncHostElementMetrics()
        {
            FrameworkElement host = _hostElement;
            if (host == null) return;
            try
            {
                double dipW = host.ActualWidth;
                double dipH = host.ActualHeight;
                double scale = host.XamlRoot?.RasterizationScale ?? 1.0;
                if (scale <= 0) scale = 1.0;

                // host element 在 widget HWND 客户区（CoreWindow root visual）内的 DIP 偏移。
                // null 参数 = 相对于 root visual。Surface Border 通常 (0,0)，但若 XAML
                // theme/style 加了 padding，这里能精确捕获。
                Point hostOriginInClient = new Point(0, 0);
                try
                {
                    GeneralTransform t = host.TransformToVisual(null);
                    hostOriginInClient = t.TransformPoint(new Point(0, 0));
                }
                catch (Exception ex)
                {
                    Debug.WriteLine($"[CompositionPump] TransformToVisual failed: {ex.Message}");
                }

                int physW = (int)Math.Round(dipW * scale);
                int physH = (int)Math.Round(dipH * scale);
                int offX = (int)Math.Round(hostOriginInClient.X * scale);
                int offY = (int)Math.Round(hostOriginInClient.Y * scale);

                Volatile.Write(ref _hostPhysW, physW);
                Volatile.Write(ref _hostPhysH, physH);
                Volatile.Write(ref _hostOffsetX, offX);
                Volatile.Write(ref _hostOffsetY, offY);

                Debug.WriteLine(
                    $"[CompositionPump] host metrics: dip=({dipW:F1},{dipH:F1}) scale={scale:F2} " +
                    $"phys=({physW},{physH}) off=({offX},{offY})");
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[CompositionPump] SyncHostElementMetrics threw: {ex.Message}");
            }
        }

        private void OnHostElementSizeChanged(object sender, SizeChangedEventArgs e)
        {
            UpdateVisualSize(e.NewSize.Width, e.NewSize.Height);
            // 拉伸 bug 修复：每次 layout 变化同步 host element 物理像素到 OnBgTick 用的缓存
            SyncHostElementMetrics();
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

            // 拉伸 bug 根因（2026-05 诊断）：
            //   原方案让 canvas = monitor 物理像素、vp = host element 物理像素，二者不同
            //   → 业务 (canvasW * 0.78, ...) 经 SetTransform translate(-vp_x, -vp_y) 落在
            //   swap chain 上的位置 != 业务期望的 widget 78% 处（横纵比例不一致还会让
            //   "正方形"渲染成长方形）。
            //
            // 选项 A 修复：让 canvas == host element 物理像素，vp = (0, 0, canvasW, canvasH)。
            //   - swap chain back buffer = canvas = host element 物理像素 = visual 物理像素 → 1:1 锐利
            //   - SetTransform translate(0,0) → 业务 (canvasW * 0.78, ...) 直接对应 swap chain 同坐标
            //     → 业务百分比布局精确对齐 widget 内部
            //   - canvas != host_phys 时 fire OnMonitorMismatch（名字保留为历史兼容；新语义是
            //     "canvas 跟 widget 物理像素不一致，请 host 调 renderer_resize + SetCanvas"），
            //     widget resize / 跨 monitor / 系统 DPI 改都通过这条路径自动同步
            //
            // 取 host element 物理像素 + 偏移（SizeChanged callback 在 UI 线程同步过的 Volatile 缓存）
            int hostPhysW = Volatile.Read(ref _hostPhysW);
            int hostPhysH = Volatile.Read(ref _hostPhysH);
            int hostOffX = Volatile.Read(ref _hostOffsetX);
            int hostOffY = Volatile.Read(ref _hostOffsetY);

            // host element 首次 layout 还没跑 → 缓存为 0 → 跳本帧，等 SizeChanged 同步后再渲染。
            // 避免用 winRect 做"临时 vp"时画面错位一帧。
            if (hostPhysW <= 0 || hostPhysH <= 0) return;

            // canvas 跟 host element 物理像素不一致（widget resize / 跨 monitor / DPI 改）
            //   → fire 通知 host 调 renderer_resize + SetCanvas 让 canvas 追上来
            if (hostPhysW != (int)canvasW || hostPhysH != (int)canvasH)
            {
                if (hostPhysW != lastMonW || hostPhysH != lastMonH)
                {
                    lock (_gate) { _lastMonW = hostPhysW; _lastMonH = hostPhysH; }
                    Debug.WriteLine(
                        $"[CompositionPump] canvas mismatch: canvas={canvasW}x{canvasH} host_phys={hostPhysW}x{hostPhysH}, dispatching host resize");
                    var cb = OnMonitorMismatch;
                    if (cb != null && _dispatcher != null)
                    {
                        uint targetW = (uint)hostPhysW;
                        uint targetH = (uint)hostPhysH;
                        _ = _dispatcher.RunAsync(CoreDispatcherPriority.Normal, () =>
                        {
                            try { cb(targetW, targetH); }
                            catch (Exception ex) { Debug.WriteLine($"[CompositionPump] OnMonitorMismatch threw: {ex}"); }
                        });
                    }
                }
                return;
            }

            // canvas == host_phys 之后：vp = (0, 0, canvasW, canvasH)
            float vpX = 0f;
            float vpY = 0f;
            int viewW = (int)canvasW;
            int viewH = (int)canvasH;

            // 诊断保留：winRect 仅用于 DrawDebugContent 的诊断行显示，不参与 vp 计算
            int winRectW = 0;
            int winRectH = 0;
            if (ScreenInterop.TryGetWindowScreenRect(hwnd, out var winRect))
            {
                winRectW = winRect.Width;
                winRectH = winRect.Height;
            }
            if (viewW <= 0 || viewH <= 0) return;

            long tickT0 = Stopwatch.GetTimestamp();
            long acquireUs = 0;
            bool sampleValid = false;

            _renderInProgress = true;
            try
            {
                long t = Stopwatch.GetTimestamp();
                // v0.7 ABI：begin_frame 加了 outCanvasW / outCanvasH 出参用于业务百分比布局。
                // widget 当前画图代码仍走 v0.6 屏幕坐标系固定模式（不依赖画布尺寸），传 IntPtr.Zero 跳过出参。
                // 后续 widget 业务代码改造（百分比化）时换成 out int 重载。
                int status = Renderer.renderer_begin_frame(handle, vpX, vpY, viewW, viewH, IntPtr.Zero, IntPtr.Zero);
                if (status != Renderer.RENDERER_OK)
                {
                    Debug.WriteLine($"[CompositionPump] begin_frame failed: status={status}");
                    return;
                }

                bool frameOpen = true;
                try
                {
                    DrawDebugContent(handle, canvasW, canvasH,
                        vpX, vpY, viewW, viewH,
                        winRect.Width, winRect.Height,
                        hostPhysW, hostPhysH, hostOffX, hostOffY);

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
        ///
        /// 拉伸 bug 诊断版：把 viewport / host phys / winRect / canvas 同时画到副信息行，
        /// 用户截图后能直接看到根因。
        /// </summary>
        private void DrawDebugContent(IntPtr handle, uint canvasW, uint canvasH,
            float vpX, float vpY, int viewW, int viewH,
            int winRectW, int winRectH,
            int hostPhysW, int hostPhysH, int hostOffX, int hostOffY)
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
            //    + **诊断**：viewport / host phys / winRect 尺寸,用于定位拉伸 bug 根因
            string info = $"canvas {canvasW}x{canvasH} | t={tSec:F1}s | dcomp";
            float infoSize = Math.Min(Math.Max(fontSize * 0.30f, 14f), 28f);
            Renderer.DrawText(
                handle, info,
                textX, textY + fontSize * 1.15f, infoSize,
                0.85f, 0.85f, 0.85f, 0.70f);

            // 5b) 诊断行：vp / canvas / host_phys / winRect
            //   修复后预期：vp.size == canvas == host_phys（三者相等表示已对齐）；vp 起点恒 (0,0)
            //   winRect 只是参考（widget HWND 在屏幕上的物理矩形，不参与 vp 计算）
            string diagLine1 = $"vp=({vpX:F0},{vpY:F0}) {viewW}x{viewH}  canvas={canvasW}x{canvasH}";
            string diagLine2 = $"host_phys={hostPhysW}x{hostPhysH} winRect={winRectW}x{winRectH} off=({hostOffX},{hostOffY})";
            float diagSize = infoSize * 0.85f;
            Renderer.DrawText(
                handle, diagLine1,
                textX, textY + fontSize * 1.55f, diagSize,
                1.0f, 0.85f, 0.40f, 0.85f);   // 偏黄醒目
            Renderer.DrawText(
                handle, diagLine2,
                textX, textY + fontSize * 1.85f, diagSize,
                1.0f, 0.85f, 0.40f, 0.85f);

            // 6) v0.7 Phase 1 primitives showcase —— 一次性覆盖所有新 ABI，
            //    build 起来就能肉眼看到每条命令在 D2D 管线里实际生效。
            //    所有颜色均 premultiplied（rgb ≤ a）。
            DrawPrimitivesShowcase(handle, cw, ch, tF);

            // 7) v0.7 Phase 2 bitmap showcase —— 顶部带 4 个 slot，覆盖
            //    create_texture (BGRA8 / RGBA8) → update_texture → draw_bitmap
            //    （含 src_rect 子矩形 + nearest|linear 插值对比）。
            //    Phase2BitmapShowcase 负责状态生命周期，这里只负责构图。
            if (_phase2 != null) DrawPhase2Showcase(handle, cw, ch, tF);

            // 8) v0.7 Phase 5 path + 渐变 showcase —— 中间带，3 个 slot：
            //    左：SVG 风格五角星 path（fill_path + stroke_path）
            //    中：linear gradient 矩形（水平方向 黑 → 蓝）
            //    右：radial gradient 矩形（中心红 → 边缘透明）
            //    覆盖 phase 5 全部 4 个 ABI（fill_path / stroke_path /
            //    fill_rect_gradient_linear / fill_rect_gradient_radial）。
            DrawPhase5Showcase(handle, cw, ch, tF);
        }

        /// <summary>
        /// v0.7 Phase 1 primitives showcase：在屏幕下方 ~20% 区域画一条命令带，
        /// 依次调用 stroke_rect / fill_rounded_rect / stroke_rounded_rect / draw_line(各 dash) /
        /// draw_polyline(closed triangle) / fill_ellipse / stroke_ellipse / push_clip+pop_clip /
        /// set_transform(rotate)+reset_transform。每条命令用不同颜色便于肉眼识别。
        /// 高度不超过 canvas 高度 20%，左右各留 5% 边距，不跟 Hello 文本重叠。
        /// </summary>
        private void DrawPrimitivesShowcase(IntPtr h, float cw, float ch, float tF)
        {
            float margin = cw * 0.05f;
            float bandY = ch * 0.78f;
            float bandH = ch * 0.18f;

            // slot 锁正方形：size = min(横向单 slot 内宽, 纵向 cell 高)。
            // widget 长宽比变化时 slot 形状恒定（不再被横纵不等量百分比拉成长方形），
            // 只是整组大小适配 widget 短边。
            float horizontalSlotInner = (cw - margin * 2f) / 8f * 0.8f; // 原 SlotW() 的值
            float verticalCellH = bandH * 0.9f;                          // 原 cellH 的值
            float slotInnerSize = Math.Min(horizontalSlotInner, verticalCellH);

            // 反推每 slot 容器宽（含两侧 10% padding） = slotInnerSize / 0.8，整组在 band 内横向居中
            float slotPitch = slotInnerSize / 0.8f;
            float totalSlotsW = slotPitch * 8f;
            float bandLeft = margin;
            float bandWidth = cw - margin * 2f;
            float startX = bandLeft + (bandWidth - totalSlotsW) * 0.5f;

            float cellH = slotInnerSize;
            float cellY = bandY + (bandH - cellH) * 0.5f;

            // 背景带：保留原 band 全宽，让 slot 组在背景里居中
            const float bgA = 0.35f;
            Renderer.renderer_fill_rect(h, bandLeft, bandY, bandWidth, bandH,
                0.02f * bgA, 0.02f * bgA, 0.05f * bgA, bgA);

            int slot = 0;
            float SlotX(int i) => startX + slotPitch * i + slotPitch * 0.1f;
            float SlotW() => slotInnerSize;

            // --- slot 0: stroke_rect ---
            {
                float x = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                Renderer.renderer_stroke_rect(h, x, y, w, hh, 3f, 0.9f, 0.4f, 0.4f, 1f);
                slot++;
            }

            // --- slot 1: fill_rounded_rect（正圆角 rx == ry） ---
            // radius 用 slot 比例（不是固定 px），保证不同 widget 大小下圆角占比稳定
            {
                float x = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                float radius = slotInnerSize * 0.10f;
                Renderer.renderer_fill_rounded_rect(h, x, y, w, hh, radius, radius,
                    0.2f, 0.6f, 0.9f, 1f);
                slot++;
            }

            // --- slot 2: stroke_rounded_rect（椭圆角 rx != ry，与 slot 1 对照） ---
            // 故意 rx != ry：把 ABI 的非对称圆角路径纳入 dogfood，看起来"不自然"是预期效果
            // —— 直观对比 slot 1 的正圆角，能一眼看出 ABI 行为差异。比例同样按 slot 尺寸缩放
            {
                float x = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                float rx = slotInnerSize * 0.18f;
                float ry = slotInnerSize * 0.06f;
                Renderer.renderer_stroke_rounded_rect(h, x, y, w, hh, rx, ry, 2.5f,
                    0.9f, 0.9f, 0.3f, 1f);
                slot++;
            }

            // --- slot 3: draw_line，4 条竖线展示 4 种 dash style ---
            {
                float x0 = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                float step = w / 4f;
                for (int i = 0; i < 4; i++)
                {
                    float lx = x0 + step * (i + 0.5f);
                    Renderer.renderer_draw_line(h, lx, y, lx, y + hh, 2f,
                        0.4f, 0.9f, 0.5f, 1f, i); // dashStyle 0..3
                }
                slot++;
            }

            // --- slot 4: draw_polyline，闭合等边三角形（顶点向上，纵向居中）---
            // 旧画法用 (cx,top) / (right,bottom) / (left,bottom)：底边=w，左右腰=√(w²/4+hh²)，
            // 即使 slot 正方形 (w==hh=s) 侧边仍是 s·√5/2 ≈ 1.118 s — 等腰非等边，视觉
            // 上看起来像 y 轴被拉伸。改成真等边：side = min(w, hh/√3·2)，高度 = side·√3/2，
            // 在 slot 内纵向居中（slot 上下留对称空白）。
            {
                float x = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                const float Sqrt3Over2 = 0.8660254f;
                float side = Math.Min(w, hh / Sqrt3Over2);
                float triH = side * Sqrt3Over2;
                float topY = y + (hh - triH) * 0.5f;
                float botY = topY + triH;
                float midX = x + w * 0.5f;
                float halfSide = side * 0.5f;
                _triPts[0] = midX;            _triPts[1] = topY;
                _triPts[2] = midX + halfSide; _triPts[3] = botY;
                _triPts[4] = midX - halfSide; _triPts[5] = botY;
                Renderer.DrawPolyline(h, _triPts, 2.5f, 0.9f, 0.5f, 0.9f, 1f, true);
                slot++;
            }

            // --- slot 5: fill_ellipse + stroke_ellipse（同心）---
            {
                float x = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                float cx = x + w * 0.5f, cy = y + hh * 0.5f;
                float rx = w * 0.45f, ry = hh * 0.45f;
                Renderer.renderer_fill_ellipse(h, cx, cy, rx * 0.6f, ry * 0.6f,
                    0.3f, 0.8f, 0.8f, 1f);
                Renderer.renderer_stroke_ellipse(h, cx, cy, rx, ry, 2f,
                    0.9f, 0.9f, 0.9f, 1f);
                slot++;
            }

            // --- slot 6: push_clip_rect / pop_clip ——
            //     在半个 slot 内画一个超大圆，clip 外面被裁掉
            {
                float x = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                float clipW = w * 0.5f;
                Renderer.renderer_push_clip_rect(h, x, y, clipW, hh);
                Renderer.renderer_fill_ellipse(h, x + w * 0.5f, y + hh * 0.5f,
                    w * 0.6f, hh * 0.6f, 0.9f, 0.5f, 0.3f, 1f);
                Renderer.renderer_pop_clip(h);
                // 边框标出 slot 范围，方便对比裁切效果
                Renderer.renderer_stroke_rect(h, x, y, w, hh, 1f,
                    0.5f, 0.5f, 0.5f, 0.8f);
                slot++;
            }

            // --- slot 7: set_transform(rotate) + reset_transform ——
            //     绕 slot 中心旋转 tF 弧度画一个矩形。
            //     **故意**画长方形（0.6w × 0.5hh，长宽比 1.2）而不是正方形 ——
            //     正方形旋转 90° 倍数视觉上等同未转，长方形才能一眼验证 transform 在生效。
            {
                float x = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                float cx = x + w * 0.5f, cy = y + hh * 0.5f;
                double ang = tF * 0.8; // 慢速转
                float cos = (float)Math.Cos(ang);
                float sin = (float)Math.Sin(ang);
                // R around (cx,cy):  T(cx,cy) * R(θ) * T(-cx,-cy)
                _xform[0] = cos; _xform[1] = sin;
                _xform[2] = -sin; _xform[3] = cos;
                _xform[4] = cx - cx * cos + cy * sin;
                _xform[5] = cy - cx * sin - cy * cos;
                Renderer.SetTransform(h, _xform);
                float rw = w * 0.6f, rh = hh * 0.5f;  // 故意非正方形：转起来才看得出
                Renderer.renderer_fill_rect(h, cx - rw * 0.5f, cy - rh * 0.5f, rw, rh,
                    0.8f, 0.3f, 0.6f, 1f);
                Renderer.renderer_reset_transform(h);
                slot++;
            }

            // showcase 标题
            float lblSize = Math.Max(12f, bandH * 0.13f);
            unsafe
            {
                fixed (byte* p = s_showcaseUtf8)
                {
                    Renderer.renderer_draw_text(h, (IntPtr)p, s_showcaseUtf8.Length,
                        margin + 4f, bandY + 4f, lblSize,
                        0.85f, 0.85f, 0.85f, 0.9f);
                }
            }
        }

        /// <summary>
        /// v0.7 Phase 2 bitmap showcase：屏幕顶部一条带，4 个 slot 覆盖
        /// 静态 BGRA8 / 动态 BGRA8 / 动态 RGBA8（验证 swizzle）/ src_rect+nearest|linear 对比。
        ///
        /// 每帧两步：
        /// 1. <see cref="Phase2BitmapShowcase.UpdateAnimated"/> 把动画帧 push 到 GPU（renderer_update_texture）
        /// 2. 4 次 renderer_draw_bitmap 把 4 张 bitmap 画到屏幕指定位置
        ///
        /// 跟 phase 1 showcase 互不干扰：phase 1 在 ch * 0.78 起，phase 2 在 ch * 0.04 起。
        /// 缓冲在 Create 时一次性分配（_bgra8Buffer / _rgba8Buffer / _upscaleBuffer），
        /// per-frame 不 GC alloc。
        /// </summary>
        private void DrawPhase2Showcase(IntPtr h, float cw, float ch, float tF)
        {
            // 1) update_texture：动画两张（BGRA8 直传 + RGBA8 走 swizzle 路径）
            _phase2.UpdateAnimated(h, tF);

            // 2) 顶部带布局
            float margin = cw * 0.05f;
            float bandY = ch * 0.04f;
            float bandH = Math.Min(Math.Max(ch * 0.13f, 80f), 220f);

            // slot 锁正方形：size = min(横向单 slot 内宽, 纵向 cell 高)。
            // widget 长宽比变化时 slot 形状恒定（bitmap 不再被横纵不等量拉伸），
            // 整组在 band 内横向居中。
            float horizontalSlotInner = (cw - margin * 2f) / 4f * 0.84f; // 原 SlotW() 的值
            float verticalCellH = bandH * 0.78f;                          // 原 cellH 的值
            float slotInnerSize = Math.Min(horizontalSlotInner, verticalCellH);

            float slotPitch = slotInnerSize / 0.84f;
            float totalSlotsW = slotPitch * 4f;
            float bandLeft = margin;
            float bandWidth = cw - margin * 2f;
            float startX = bandLeft + (bandWidth - totalSlotsW) * 0.5f;

            float cellH = slotInnerSize;
            float cellY = bandY + (bandH - cellH) * 0.5f + bandH * 0.08f;

            // 半透明深底，让 bitmap 有底色对比，不被游戏画面干扰
            const float bgA = 0.40f;
            Renderer.renderer_fill_rect(h, bandLeft, bandY, bandWidth, bandH,
                0.02f * bgA, 0.05f * bgA, 0.02f * bgA, bgA);

            int slot = 0;
            float SlotX(int i) => startX + slotPitch * i + slotPitch * 0.08f;
            float SlotW() => slotInnerSize;

            // --- slot 0: 静态 16x16 BGRA8 棋盘格（整图，nearest 插值放大保持锐利） ---
            {
                float x = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                Renderer.renderer_draw_bitmap(h, _phase2.StaticBgra8,
                    0f, 0f, 0f, 0f,           // src_*=0 → 整图
                    x, y, w, hh, 1f, Renderer.INTERP_NEAREST);
                Renderer.renderer_stroke_rect(h, x, y, w, hh, 1f, 0.6f, 0.6f, 0.6f, 0.6f);
                slot++;
            }

            // --- slot 1: 动态 16x16 BGRA8 径向渐变（整图，linear 插值，opacity 0.9） ---
            {
                float x = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                Renderer.renderer_draw_bitmap(h, _phase2.AnimatedBgra8,
                    0f, 0f, 0f, 0f,
                    x, y, w, hh, 0.9f, Renderer.INTERP_LINEAR);
                Renderer.renderer_stroke_rect(h, x, y, w, hh, 1f, 0.6f, 0.6f, 0.6f, 0.6f);
                slot++;
            }

            // --- slot 2: 动态 16x16 RGBA8 径向渐变（整图，linear 插值；
            //      Rust 端 update_texture 走 swizzle_rgba_to_bgra 路径）---
            {
                float x = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                Renderer.renderer_draw_bitmap(h, _phase2.AnimatedRgba8,
                    0f, 0f, 0f, 0f,
                    x, y, w, hh, 1f, Renderer.INTERP_LINEAR);
                Renderer.renderer_stroke_rect(h, x, y, w, hh, 1f, 0.6f, 0.6f, 0.6f, 0.6f);
                slot++;
            }

            // --- slot 3: 4x4 大色块上采样，左 nearest（块状）右 linear（平滑），src_rect 整图 ---
            {
                float x = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                float halfW = w * 0.5f;
                Renderer.renderer_draw_bitmap(h, _phase2.UpscaleSrc,
                    0f, 0f, 4f, 4f,
                    x, y, halfW, hh, 1f, Renderer.INTERP_NEAREST);
                Renderer.renderer_draw_bitmap(h, _phase2.UpscaleSrc,
                    0f, 0f, 4f, 4f,
                    x + halfW, y, halfW, hh, 1f, Renderer.INTERP_LINEAR);
                Renderer.renderer_stroke_rect(h, x, y, w, hh, 1f, 0.6f, 0.6f, 0.6f, 0.6f);
                // 中线分隔
                Renderer.renderer_draw_line(h, x + halfW, y, x + halfW, y + hh, 1f,
                    0.9f, 0.9f, 0.9f, 0.9f, Renderer.DashStyle.Dash);
                slot++;
            }

            // 标题
            float lblSize = Math.Max(11f, bandH * 0.10f);
            unsafe
            {
                fixed (byte* p = s_phase2Utf8)
                {
                    Renderer.renderer_draw_text(h, (IntPtr)p, s_phase2Utf8.Length,
                        margin + 4f, bandY + 2f, lblSize,
                        0.85f, 0.95f, 0.85f, 0.9f);
                }
            }
        }

        /// <summary>
        /// v0.7 Phase 5 showcase：中间带，3 个 slot 各覆盖 1-2 个 ABI。
        ///   slot 0：SVG 风格五角星（绿底 fill_path + 黄边 stroke_path），转动以验证非平凡 path
        ///   slot 1：水平 linear gradient 矩形（黑 → 蓝），相位随 t 漂移
        ///   slot 2：radial gradient 矩形（中心红 → 中段黄 → 边缘透明），脉动半径
        ///
        /// path opcode 用 little-endian f32 直接写入 byte buffer，per-frame 不 GC alloc
        /// （buffer / stops 数组都是 readonly 字段一次性分配）。
        /// </summary>
        private void DrawPhase5Showcase(IntPtr h, float cw, float ch, float tF)
        {
            // 中间带：phase 2 在 ch * 0.04 起 (高 ch * 0.13)，phase 1 在 ch * 0.78 起。
            // 中间留 ch * 0.20 ~ ch * 0.55 给 phase 5。
            float margin = cw * 0.05f;
            float bandY = ch * 0.22f;
            float bandH = ch * 0.30f;

            // slot 锁正方形：min(横向单 slot 内宽, 纵向 cell 高)。整组居中
            float horizontalSlotInner = (cw - margin * 2f) / 3f * 0.84f;
            float verticalCellH = bandH * 0.78f;
            float slotInnerSize = Math.Min(horizontalSlotInner, verticalCellH);
            float slotPitch = slotInnerSize / 0.84f;
            float totalSlotsW = slotPitch * 3f;
            float bandLeft = margin;
            float bandWidth = cw - margin * 2f;
            float startX = bandLeft + (bandWidth - totalSlotsW) * 0.5f;
            float cellH = slotInnerSize;
            float cellY = bandY + (bandH - cellH) * 0.5f + bandH * 0.05f;

            // 半透明深底，让 phase 5 内容跟其他 phase 区分
            const float bgA = 0.35f;
            Renderer.renderer_fill_rect(h, bandLeft, bandY, bandWidth, bandH,
                0.05f * bgA, 0.02f * bgA, 0.06f * bgA, bgA);

            int slot = 0;
            float SlotX(int i) => startX + slotPitch * i + slotPitch * 0.08f;
            float SlotW() => slotInnerSize;

            // --- slot 0: 五角星 path（fill_path 绿底 + stroke_path 黄边） ---
            // 五角星 5 个外顶点 + 5 个内顶点（10 个顶点 + 1 close = MOVE_TO + 9 LINE_TO + CLOSE）
            // 中心 (cx, cy)，外半径 R，内半径 r ≈ R * 0.382。每 36° 一个顶点，外内交替
            {
                float x = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                float cx = x + w * 0.5f;
                float cy = y + hh * 0.5f;
                float R = Math.Min(w, hh) * 0.42f;
                float r = R * 0.382f;
                // 慢速旋转，让 path 是动态的方便肉眼区分"几何在生效"
                double angle0 = -Math.PI * 0.5 + tF * 0.3;

                int len = WritePentagram(_phase5PathBuf, cx, cy, R, r, (float)angle0);

                // fill: 绿色（premultiplied alpha 1.0）
                unsafe
                {
                    fixed (byte* p = _phase5PathBuf)
                    {
                        Renderer.renderer_fill_path(h, (IntPtr)p, len,
                            0.20f, 0.65f, 0.30f, 1.0f);
                        // stroke: 黄边（dashStyle=0 solid）
                        Renderer.renderer_stroke_path(h, (IntPtr)p, len, 2.0f,
                            0.95f, 0.85f, 0.20f, 1.0f, 0);
                    }
                }
                // slot 框
                Renderer.renderer_stroke_rect(h, x, y, w, hh, 1f, 0.5f, 0.5f, 0.5f, 0.6f);
                slot++;
            }

            // --- slot 1: linear gradient 矩形（水平 黑 → 蓝） ---
            //   start = slot 左中，end = slot 右中。方向随 t 微微抖动验证渐变线坐标在生效
            {
                float x = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                float cy = y + hh * 0.5f;
                // 起止点随 t 缓慢倾斜（线性渐变方向）
                float wobble = (float)(Math.Sin(tF * 0.7) * hh * 0.15);
                float sx = x;
                float sy = cy - wobble;
                float ex = x + w;
                float ey = cy + wobble;

                // 2 stops: offset, r, g, b, a
                _phase5LinearStops[0] = 0.0f;
                _phase5LinearStops[1] = 0.0f;
                _phase5LinearStops[2] = 0.0f;
                _phase5LinearStops[3] = 0.0f;
                _phase5LinearStops[4] = 1.0f;
                _phase5LinearStops[5] = 1.0f;
                _phase5LinearStops[6] = 0.10f;
                _phase5LinearStops[7] = 0.20f;
                _phase5LinearStops[8] = 0.85f;  // 蓝
                _phase5LinearStops[9] = 1.0f;

                unsafe
                {
                    fixed (float* p = _phase5LinearStops)
                    {
                        Renderer.renderer_fill_rect_gradient_linear(h,
                            x, y, w, hh,
                            sx, sy, ex, ey,
                            (IntPtr)p, 2);
                    }
                }
                Renderer.renderer_stroke_rect(h, x, y, w, hh, 1f, 0.5f, 0.5f, 0.5f, 0.6f);
                slot++;
            }

            // --- slot 2: radial gradient 矩形（中心 红 → 中段 黄 → 边缘 透明） ---
            //   半径随 t 脉动（验证 radius_x / radius_y 在生效）
            {
                float x = SlotX(slot), y = cellY, w = SlotW(), hh = cellH;
                float cx = x + w * 0.5f;
                float cy = y + hh * 0.5f;
                float pulse = (float)(0.85 + Math.Sin(tF * 1.2) * 0.15);
                float rx = w * 0.5f * pulse;
                float ry = hh * 0.5f * pulse;

                // 3 stops: 中心红 → 中段黄 → 边缘透明（premultiplied：rgb *= a）
                _phase5RadialStops[0] = 0.0f;
                _phase5RadialStops[1] = 0.95f;
                _phase5RadialStops[2] = 0.20f;
                _phase5RadialStops[3] = 0.20f;
                _phase5RadialStops[4] = 1.0f;
                _phase5RadialStops[5] = 0.5f;
                _phase5RadialStops[6] = 0.95f;
                _phase5RadialStops[7] = 0.80f;
                _phase5RadialStops[8] = 0.20f;
                _phase5RadialStops[9] = 1.0f;
                _phase5RadialStops[10] = 1.0f;
                _phase5RadialStops[11] = 0.0f;
                _phase5RadialStops[12] = 0.0f;
                _phase5RadialStops[13] = 0.0f;
                _phase5RadialStops[14] = 0.0f;

                unsafe
                {
                    fixed (float* p = _phase5RadialStops)
                    {
                        Renderer.renderer_fill_rect_gradient_radial(h,
                            x, y, w, hh,
                            cx, cy, rx, ry,
                            (IntPtr)p, 3);
                    }
                }
                Renderer.renderer_stroke_rect(h, x, y, w, hh, 1f, 0.5f, 0.5f, 0.5f, 0.6f);
                slot++;
            }

            // 标题
            float lblSize = Math.Max(11f, bandH * 0.06f);
            unsafe
            {
                fixed (byte* p = s_phase5Utf8)
                {
                    Renderer.renderer_draw_text(h, (IntPtr)p, s_phase5Utf8.Length,
                        margin + 4f, bandY + 2f, lblSize,
                        0.95f, 0.85f, 0.95f, 0.9f);
                }
            }
        }

        /// <summary>
        /// 写五角星 path 到 buf，返回写入字节数。
        /// 10 顶点（外内交替），中心 (cx, cy)，外半径 R，内半径 r，起始角 startAngle（弧度）。
        /// 编码：MOVE_TO + 9 × LINE_TO + CLOSE。每个 f32 = 4 bytes little-endian。
        /// </summary>
        private static int WritePentagram(byte[] buf, float cx, float cy, float R, float r, float startAngle)
        {
            int idx = 0;
            // 10 个顶点：i 偶 → 外，i 奇 → 内
            for (int i = 0; i < 10; i++)
            {
                float theta = startAngle + (float)(Math.PI * 0.2 * i); // 36° = π/5
                float radius = (i % 2 == 0) ? R : r;
                float vx = cx + (float)Math.Cos(theta) * radius;
                float vy = cy + (float)Math.Sin(theta) * radius;
                if (i == 0)
                {
                    buf[idx++] = 0x01; // MOVE_TO
                }
                else
                {
                    buf[idx++] = 0x02; // LINE_TO
                }
                WriteF32Le(buf, idx, vx); idx += 4;
                WriteF32Le(buf, idx, vy); idx += 4;
            }
            buf[idx++] = 0x05; // CLOSE
            return idx;
        }

        /// <summary>
        /// 把 f32 按 little-endian 直接写入 buf，避免 BitConverter.GetBytes 的 GC alloc。
        /// 用 unsafe 位转，兼容 .NET Native 2.2 / netstandard2.0（不依赖 BitConverter.SingleToInt32Bits）。
        /// </summary>
        private static unsafe void WriteF32Le(byte[] buf, int offset, float value)
        {
            uint bits = *(uint*)(&value);
            buf[offset + 0] = (byte)(bits & 0xFF);
            buf[offset + 1] = (byte)((bits >> 8) & 0xFF);
            buf[offset + 2] = (byte)((bits >> 16) & 0xFF);
            buf[offset + 3] = (byte)((bits >> 24) & 0xFF);
        }

        /// <summary>
        /// Phase 2 bitmap showcase 的状态持有者：4 个 bitmap handle + 复用的像素缓冲。
        /// Create 一次拿 4 个 handle，UpdateAnimated 每帧 push 像素到其中两个，Destroy 释放全部。
        ///
        /// 设计原则：handle / 缓冲都属于一次 widget 生命周期；Stop → Destroy → 下次 Start 时
        /// 重新创建。不在帧回调里检查 handle valid（创建失败时整个 _phase2 字段被设为 null）。
        /// </summary>
        private sealed class Phase2BitmapShowcase
        {
            // 静态棋盘格（BGRA8）—— Create 时一次性 update，之后只 draw_bitmap
            public uint StaticBgra8;
            // 动态径向渐变（BGRA8 直传）—— 每帧 update_texture
            public uint AnimatedBgra8;
            // 动态径向渐变（RGBA8 输入，验证 swizzle）—— 每帧 update_texture
            public uint AnimatedRgba8;
            // 4x4 大色块（BGRA8）—— upscale 路径下 nearest vs linear 插值对比
            public uint UpscaleSrc;

            private const int AnimSize = 16;
            private const int UpscaleSize = 4;
            private byte[] _bgra8Buffer;       // AnimSize * AnimSize * 4 = 1024 bytes
            private byte[] _rgba8Buffer;
            private byte[] _staticBuffer;

            /// <summary>
            /// 创建 4 个 bitmap + 准备 buffer + 一次性 push 静态像素。
            /// 任何步骤失败都会 destroy 已创建的 handle 并返 false。
            /// </summary>
            public bool Create(IntPtr h)
            {
                int st;
                st = Renderer.renderer_create_texture(h, AnimSize, AnimSize, Renderer.BITMAP_FORMAT_BGRA8, out StaticBgra8);
                if (st != Renderer.RENDERER_OK) { Cleanup(h); return false; }
                st = Renderer.renderer_create_texture(h, AnimSize, AnimSize, Renderer.BITMAP_FORMAT_BGRA8, out AnimatedBgra8);
                if (st != Renderer.RENDERER_OK) { Cleanup(h); return false; }
                st = Renderer.renderer_create_texture(h, AnimSize, AnimSize, Renderer.BITMAP_FORMAT_RGBA8, out AnimatedRgba8);
                if (st != Renderer.RENDERER_OK) { Cleanup(h); return false; }
                st = Renderer.renderer_create_texture(h, UpscaleSize, UpscaleSize, Renderer.BITMAP_FORMAT_BGRA8, out UpscaleSrc);
                if (st != Renderer.RENDERER_OK) { Cleanup(h); return false; }

                _bgra8Buffer = new byte[AnimSize * AnimSize * 4];
                _rgba8Buffer = new byte[AnimSize * AnimSize * 4];
                _staticBuffer = new byte[AnimSize * AnimSize * 4];

                // 静态棋盘格（每 4x4 像素一格，深 / 浅 alternating）
                FillCheckerboard(_staticBuffer, AnimSize, AnimSize);
                if (Renderer.UpdateTexture(h, StaticBgra8, _staticBuffer, AnimSize * 4, Renderer.BITMAP_FORMAT_BGRA8) != Renderer.RENDERER_OK)
                { Cleanup(h); return false; }

                // 4x4 大色块：左上红 / 右上绿 / 左下蓝 / 右下黄（BGRA8 顺序写入）
                var us = new byte[UpscaleSize * UpscaleSize * 4];
                for (int y = 0; y < UpscaleSize; y++)
                {
                    for (int x = 0; x < UpscaleSize; x++)
                    {
                        int i = (y * UpscaleSize + x) * 4;
                        bool right = x >= UpscaleSize / 2;
                        bool bottom = y >= UpscaleSize / 2;
                        // R G B
                        byte r = (byte)((right) ? 60 : 230);
                        byte g = (byte)((bottom) ? 60 : 230);
                        byte b = (byte)((right ^ bottom) ? 230 : 80);
                        us[i + 0] = b; // BGRA: B
                        us[i + 1] = g; // G
                        us[i + 2] = r; // R
                        us[i + 3] = 255;
                    }
                }
                if (Renderer.UpdateTexture(h, UpscaleSrc, us, UpscaleSize * 4, Renderer.BITMAP_FORMAT_BGRA8) != Renderer.RENDERER_OK)
                { Cleanup(h); return false; }

                return true;
            }

            /// <summary>
            /// 每帧调：把 BGRA8 / RGBA8 两张 16x16 渲染成"颜色随时间旋转的径向渐变"，
            /// push 到 GPU。两张图视觉应当完全相同（验证 RGBA8 swizzle 路径正确）。
            /// </summary>
            public void UpdateAnimated(IntPtr h, float tF)
            {
                FillRadialGradient(_bgra8Buffer, AnimSize, AnimSize, tF, isBgra: true);
                Renderer.UpdateTexture(h, AnimatedBgra8, _bgra8Buffer,
                    AnimSize * 4, Renderer.BITMAP_FORMAT_BGRA8);

                FillRadialGradient(_rgba8Buffer, AnimSize, AnimSize, tF, isBgra: false);
                Renderer.UpdateTexture(h, AnimatedRgba8, _rgba8Buffer,
                    AnimSize * 4, Renderer.BITMAP_FORMAT_RGBA8);
            }

            public void Destroy(IntPtr h) => Cleanup(h);

            private void Cleanup(IntPtr h)
            {
                if (StaticBgra8 != 0) { Renderer.renderer_destroy_bitmap(h, StaticBgra8); StaticBgra8 = 0; }
                if (AnimatedBgra8 != 0) { Renderer.renderer_destroy_bitmap(h, AnimatedBgra8); AnimatedBgra8 = 0; }
                if (AnimatedRgba8 != 0) { Renderer.renderer_destroy_bitmap(h, AnimatedRgba8); AnimatedRgba8 = 0; }
                if (UpscaleSrc != 0) { Renderer.renderer_destroy_bitmap(h, UpscaleSrc); UpscaleSrc = 0; }
            }

            // 棋盘格：每 4 像素 1 格，深 / 浅交替，premultiplied alpha=255 → rgb 即原色
            private static void FillCheckerboard(byte[] buf, int w, int h)
            {
                int stride = w * 4;
                for (int y = 0; y < h; y++)
                {
                    for (int x = 0; x < w; x++)
                    {
                        int i = y * stride + x * 4;
                        bool dark = (((x / 4) + (y / 4)) % 2) == 0;
                        byte v = dark ? (byte)40 : (byte)220;
                        buf[i + 0] = v; buf[i + 1] = v; buf[i + 2] = v; buf[i + 3] = 255;
                    }
                }
            }

            // 中心径向渐变 + 三色相位（按时间旋转）。
            // premultiplied：rgb 必须 ≤ a，所以最终 rgb *= alpha_norm。
            // isBgra=true → 字节序 [B, G, R, A]；false → [R, G, B, A]（让 Rust swizzle 翻 R/B）
            private static void FillRadialGradient(byte[] buf, int w, int h, float tF, bool isBgra)
            {
                float cx = w * 0.5f - 0.5f;
                float cy = h * 0.5f - 0.5f;
                float maxR = (float)Math.Sqrt(cx * cx + cy * cy);
                if (maxR < 1f) maxR = 1f;
                float hue = tF * 0.6f;
                int stride = w * 4;
                for (int y = 0; y < h; y++)
                {
                    for (int x = 0; x < w; x++)
                    {
                        int i = y * stride + x * 4;
                        float dx = x - cx, dy = y - cy;
                        float r = (float)Math.Sqrt(dx * dx + dy * dy) / maxR;
                        float intensity = Math.Max(0f, 1f - r);
                        // intensity^2 让中心更亮，边缘衰减更明显
                        float alphaN = intensity * intensity;
                        byte a = (byte)(255f * alphaN);
                        // 简易三相 RGB 旋转
                        float rRaw = (float)Math.Abs(Math.Sin(hue + 0.0));
                        float gRaw = (float)Math.Abs(Math.Sin(hue + 2.094));
                        float bRaw = (float)Math.Abs(Math.Sin(hue + 4.188));
                        // premultiplied：rgb *= alphaN
                        byte rPm = (byte)(255f * rRaw * alphaN);
                        byte gPm = (byte)(255f * gRaw * alphaN);
                        byte bPm = (byte)(255f * bRaw * alphaN);
                        if (isBgra)
                        {
                            buf[i + 0] = bPm; buf[i + 1] = gPm; buf[i + 2] = rPm; buf[i + 3] = a;
                        }
                        else
                        {
                            buf[i + 0] = rPm; buf[i + 1] = gPm; buf[i + 2] = bPm; buf[i + 3] = a;
                        }
                    }
                }
            }
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
