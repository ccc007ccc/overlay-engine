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
    /// Game Bar widget (New IPC architecture).
    /// </summary>
    public sealed partial class MainWidget : Page
    {
        private const double DefaultWidth = 1280;
        private const double DefaultHeight = 720;

        private const double SideReserve = 5;
        private const double VerticalReserve = 80;

        private const string LSKey_CalibratedW = "CalibratedW";
        private const string LSKey_CalibratedH = "CalibratedH";

        private static readonly ResourceLoader Loader = ResourceLoader.GetForViewIndependentUse();
        private static MainWidget _activeInstance;

        private OverlayPump _pump;
        private XboxGameBarWidget _widget;
        private bool _calibrating;
        private Size? _calibratedSize;

        public MainWidget()
        {
            InitializeComponent();
            Loaded += OnLoaded;
            Unloaded += OnUnloaded;
            Surface.SizeChanged += OnSizeChanged;

            _activeInstance = this;

            SizeInfo.Text = Loader.GetString("Status_SizeInitial");
            MaxInfo.Text = Loader.GetString("Status_MaxInitial");
            LastResult.Text = Loader.GetString("Status_LastInitial");
            RendererInfo.Text = "Not connected";
            PerfInfo.Text = "Perf stats disabled (Server-side rendering)";

            ToolTipService.SetToolTip(SettingsBtn, Loader.GetString("Tooltip_Settings"));
            ToolTipService.SetToolTip(ReadCurrentBtn, Loader.GetString("Tooltip_ReadCurrent"));
            ToolTipService.SetToolTip(CopyDiagBtn, Loader.GetString("Tooltip_CopyDiagnostics"));

            _calibratedSize = ReadCalibratedFromLocalSettings();
            UpdateCalibratedInfo();
        }

        protected override void OnNavigatedTo(NavigationEventArgs e)
        {
            base.OnNavigatedTo(e);
            _widget = e.Parameter as XboxGameBarWidget;
            UpdateMaxInfo();
        }

        private void OnRootPointerEntered(object sender, PointerRoutedEventArgs e)
        {
            FadeInStory.Begin();
        }

        private void OnRootPointerExited(object sender, PointerRoutedEventArgs e)
        {
            FadeOutStory.Begin();
        }

        private void OnLoaded(object sender, RoutedEventArgs e)
        {
            UpdateSizeInfo();
            UpdateMaxInfo();
            TryAttachSurface();
            TrySubscribeXamlRootChanged();
        }

        private void TryAttachSurface()
        {
            IntPtr hwnd = ScreenInterop.GetCoreWindowHwnd();
            if (hwnd == IntPtr.Zero)
            {
                RendererInfo.Text = "Attach failed: GetCoreWindowHwnd returned 0";
                return;
            }

            try
            {
                _pump = new OverlayPump(Surface);
                _pump.OnStatusChanged += Pump_OnStatusChanged;
                _pump.Start(hwnd);
            }
            catch (Exception ex)
            {
                Debug.WriteLine("[OverlayWidget] OverlayPump.Start threw: " + ex);
                RendererInfo.Text = "Attach failed: " + ex.GetType().Name + " " + ex.Message;
                _pump?.Stop();
                _pump = null;
            }
        }

        private void Pump_OnStatusChanged(string status)
        {
            _ = Dispatcher.RunAsync(CoreDispatcherPriority.Normal, () =>
            {
                RendererInfo.Text = status;
            });
        }

        private void OnSizeChanged(object sender, SizeChangedEventArgs e)
        {
            UpdateSizeInfo();
        }

        private void TrySubscribeXamlRootChanged()
        {
            if (XamlRoot == null) return;
            try { XamlRoot.Changed += OnXamlRootChanged; }
            catch (Exception ex) { Debug.WriteLine($"[OverlayWidget] subscribe XamlRoot.Changed failed: {ex.Message}"); }
        }

        private void TryUnsubscribeXamlRootChanged()
        {
            if (XamlRoot == null) return;
            try { XamlRoot.Changed -= OnXamlRootChanged; }
            catch (Exception ex) { Debug.WriteLine($"[OverlayWidget] unsubscribe XamlRoot.Changed failed: {ex.Message}"); }
        }

        private void OnXamlRootChanged(XamlRoot sender, XamlRootChangedEventArgs args)
        {
            UpdateSizeInfo();
        }

        private void OnUnloaded(object sender, RoutedEventArgs e)
        {
            TryUnsubscribeXamlRootChanged();
            if (_pump != null)
            {
                _pump.Dispose();
                _pump = null;
            }
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
                    int physW = 0, physH = 0;
                    try { (physW, physH) = ScreenInterop.GetScreenMetrics(); } catch { }
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
                try { (physW, physH) = ScreenInterop.GetScreenMetrics(); } catch { }
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
            try { (physW, physH) = ScreenInterop.GetScreenMetrics(); } catch { }
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
