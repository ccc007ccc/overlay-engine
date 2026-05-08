using System;
using System.Diagnostics;
using System.Numerics;
using System.Runtime.InteropServices;
using System.Threading;
using Windows.Foundation;
using Windows.System.Threading;
using Windows.UI.Composition;
using Windows.UI.Xaml;
using Windows.UI.Xaml.Hosting;
using Windows.UI.Xaml.Media;

namespace OverlayWidget.Native
{
    internal sealed class ExternalSurfacePump : IDisposable
    {
        private const string PipePath = @"\\.\pipe\overlay-spike-dcomp";
        private const uint GenericRead = 0x80000000;
        private const uint GenericWrite = 0x40000000;
        private const uint OpenExisting = 3;
        private static readonly IntPtr InvalidHandleValue = new IntPtr(-1);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern IntPtr CreateFileW(
            string lpFileName,
            uint dwDesiredAccess,
            uint dwShareMode,
            IntPtr lpSecurityAttributes,
            uint dwCreationDisposition,
            uint dwFlagsAndAttributes,
            IntPtr hTemplateFile);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool ReadFile(
            IntPtr hFile,
            byte[] lpBuffer,
            uint nNumberOfBytesToRead,
            out uint lpNumberOfBytesRead,
            IntPtr lpOverlapped);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool WriteFile(
            IntPtr hFile,
            byte[] lpBuffer,
            uint nNumberOfBytesToWrite,
            out uint lpNumberOfBytesWritten,
            IntPtr lpOverlapped);

        [DllImport("kernel32.dll")]
        private static extern bool CloseHandle(IntPtr hObject);

        [DllImport("kernel32.dll")]
        private static extern uint GetCurrentProcessId();

        private readonly FrameworkElement _hostElement;
        private IntPtr _hwnd;
        private IntPtr _pipe = IntPtr.Zero;
        private IntPtr _surfaceHandle = IntPtr.Zero;
        private uint _logicalW;
        private uint _logicalH;
        private uint _renderW;
        private uint _renderH;
        private SpriteVisual _spriteVisual;
        private CompositionSurfaceBrush _brush;
        private ICompositionSurface _surface;
        private ThreadPoolTimer _timer;

        public ExternalSurfacePump(FrameworkElement hostElement)
        {
            _hostElement = hostElement ?? throw new ArgumentNullException(nameof(hostElement));
        }

        public static bool TryConnect(out ExternalSurfacePayload payload, out string error)
        {
            payload = default;
            error = null;

            IntPtr pipe = CreateFileW(PipePath, GenericRead | GenericWrite, 0, IntPtr.Zero, OpenExisting, 0, IntPtr.Zero);
            if (pipe == InvalidHandleValue)
            {
                error = "CreateFile pipe failed: " + Marshal.GetLastWin32Error();
                return false;
            }

            try
            {
                byte[] pid = BitConverter.GetBytes(GetCurrentProcessId());
                if (!WriteFile(pipe, pid, (uint)pid.Length, out uint written, IntPtr.Zero) || written != pid.Length)
                {
                    error = "Write PID failed: " + Marshal.GetLastWin32Error();
                    return false;
                }

                byte[] buf = new byte[24];
                int offset = 0;
                while (offset < buf.Length)
                {
                    byte[] chunk = new byte[buf.Length - offset];
                    if (!ReadFile(pipe, chunk, (uint)chunk.Length, out uint read, IntPtr.Zero) || read == 0)
                    {
                        error = "Read payload failed: " + Marshal.GetLastWin32Error();
                        return false;
                    }
                    Buffer.BlockCopy(chunk, 0, buf, offset, (int)read);
                    offset += (int)read;
                }

                payload = new ExternalSurfacePayload
                {
                    Pipe = pipe,
                    SurfaceHandle = new IntPtr(unchecked((long)BitConverter.ToUInt64(buf, 0))),
                    LogicalW = BitConverter.ToUInt32(buf, 8),
                    LogicalH = BitConverter.ToUInt32(buf, 12),
                    RenderW = BitConverter.ToUInt32(buf, 16),
                    RenderH = BitConverter.ToUInt32(buf, 20),
                };
                pipe = IntPtr.Zero;
                return true;
            }
            finally
            {
                if (pipe != IntPtr.Zero && pipe != InvalidHandleValue) CloseHandle(pipe);
            }
        }

        public void Start(ExternalSurfacePayload payload, IntPtr widgetHwnd)
        {
            if (payload.SurfaceHandle == IntPtr.Zero) throw new ArgumentException("surface handle zero");
            if (payload.LogicalW == 0 || payload.LogicalH == 0 || payload.RenderW == 0 || payload.RenderH == 0)
                throw new ArgumentException("invalid canvas meta");

            _pipe = payload.Pipe;
            _surfaceHandle = payload.SurfaceHandle;
            _logicalW = payload.LogicalW;
            _logicalH = payload.LogicalH;
            _renderW = payload.RenderW;
            _renderH = payload.RenderH;
            _hwnd = widgetHwnd;

            Visual hostVisual = ElementCompositionPreview.GetElementVisual(_hostElement);
            Compositor compositor = hostVisual.Compositor;
            ICompositorInterop interop = (ICompositorInterop)(object)compositor;
            interop.CreateCompositionSurfaceForHandle(_surfaceHandle, out object surfaceObj);
            _surface = (ICompositionSurface)surfaceObj;

            _brush = compositor.CreateSurfaceBrush(_surface);
            _brush.Stretch = CompositionStretch.Fill;

            _spriteVisual = compositor.CreateSpriteVisual();
            _spriteVisual.Brush = _brush;
            UpdateVisualTransform();
            ElementCompositionPreview.SetElementChildVisual(_hostElement, _spriteVisual);

            _hostElement.SizeChanged += OnHostSizeChanged;
            Windows.UI.Xaml.Media.CompositionTarget.Rendering += OnRendering;

            Debug.WriteLine($"[ExternalSurfacePump] started: logical={_logicalW}x{_logicalH} render={_renderW}x{_renderH} handle=0x{_surfaceHandle.ToInt64():X}");
        }

        private void OnHostSizeChanged(object sender, SizeChangedEventArgs e) => UpdateVisualTransform();

        private void OnRendering(object sender, object e) => UpdateVisualTransform();

        private void UpdateVisualTransform()
        {
            if (_spriteVisual == null || _hwnd == IntPtr.Zero) return;
            double scale = _hostElement.XamlRoot?.RasterizationScale ?? 1.0;
            if (scale <= 0) scale = 1.0;

            int winLeft = 0;
            int winTop = 0;
            if (ScreenInterop.TryGetWindowScreenRect(_hwnd, out var winRect))
            {
                winLeft = winRect.left;
                winTop = winRect.top;
            }

            Point hostOrigin = new Point(0, 0);
            try
            {
                hostOrigin = _hostElement.TransformToVisual(null).TransformPoint(new Point(0, 0));
            }
            catch { }

            float viewportX = (float)(winLeft + hostOrigin.X * scale);
            float viewportY = (float)(winTop + hostOrigin.Y * scale);
            float logicalDipW = (float)(_logicalW / scale);
            float logicalDipH = (float)(_logicalH / scale);

            _spriteVisual.Size = new Vector2(logicalDipW, logicalDipH);
            _spriteVisual.Offset = new Vector3((float)(-viewportX / scale), (float)(-viewportY / scale), 0f);
        }

        public void Stop()
        {
            try { Windows.UI.Xaml.Media.CompositionTarget.Rendering -= OnRendering; } catch { }
            try { _hostElement.SizeChanged -= OnHostSizeChanged; } catch { }
            try { ElementCompositionPreview.SetElementChildVisual(_hostElement, null); } catch { }
            try { _spriteVisual?.Dispose(); } catch { }
            try { _brush?.Dispose(); } catch { }
            try { (_surface as IDisposable)?.Dispose(); } catch { }
            _spriteVisual = null;
            _brush = null;
            _surface = null;
            if (_pipe != IntPtr.Zero && _pipe != InvalidHandleValue)
            {
                try { CloseHandle(_pipe); } catch { }
            }
            _pipe = IntPtr.Zero;
        }

        public void Dispose() => Stop();
    }

    internal struct ExternalSurfacePayload
    {
        public IntPtr Pipe;
        public IntPtr SurfaceHandle;
        public uint LogicalW;
        public uint LogicalH;
        public uint RenderW;
        public uint RenderH;
    }
}
