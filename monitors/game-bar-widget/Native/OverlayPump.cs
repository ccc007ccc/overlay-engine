using System;
using System.Diagnostics;
using System.Numerics;
using System.Runtime.InteropServices;
using System.Threading;
using System.Threading.Tasks;
using Windows.Foundation;
using Windows.UI.Composition;
using Windows.UI.Xaml;
using Windows.UI.Xaml.Hosting;

namespace OverlayWidget.Native
{
    public sealed class OverlayPump : IDisposable
    {
        private const string PipePath = @"\\.\pipe\overlay-core";
        private const uint GenericRead = 0x80000000;
        private const uint GenericWrite = 0x40000000;
        private const uint OpenExisting = 3;
        private const uint FileFlagOverlapped = 0x40000000;
        private static readonly IntPtr InvalidHandleValue = new IntPtr(-1);

        private const uint IPC_MAGIC = 0x4F56524C; // 'OVRL'
        private const ushort IPC_VERSION = 1;
        private const ushort OP_REGISTER_MONITOR = 0x0002;
        private const ushort OP_CANVAS_ATTACHED = 0x0005;
        private const ushort OP_APP_DETACHED = 0x0006;
        private const ushort OP_MONITOR_LOCAL_ATTACHED = 0x0007;

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern IntPtr CreateFileW(
            string lpFileName, uint dwDesiredAccess, uint dwShareMode,
            IntPtr lpSecurityAttributes, uint dwCreationDisposition,
            uint dwFlagsAndAttributes, IntPtr hTemplateFile);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool ReadFile(
            IntPtr hFile, byte[] lpBuffer, uint nNumberOfBytesToRead,
            out uint lpNumberOfBytesRead, IntPtr lpOverlapped);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool WriteFile(
            IntPtr hFile, byte[] lpBuffer, uint nNumberOfBytesToWrite,
            out uint lpNumberOfBytesWritten, IntPtr lpOverlapped);

        [DllImport("kernel32.dll")]
        private static extern bool CloseHandle(IntPtr hObject);

        [DllImport("kernel32.dll")]
        private static extern uint GetCurrentProcessId();

        private readonly FrameworkElement _hostElement;
        private IntPtr _hwnd;
        private IntPtr _pipe = IntPtr.Zero;

        private CancellationTokenSource _cts;
        private Task _readerTask;

        private Compositor _compositor;
        private ContainerVisual _rootVisual;

        // World Surface
        private ICompositionSurface _worldSurface;
        private CompositionSurfaceBrush _worldBrush;
        private SpriteVisual _worldVisual;
        private uint _worldLogicalW, _worldLogicalH;

        // MonitorLocal Surface
        private ICompositionSurface _mlSurface;
        private CompositionSurfaceBrush _mlBrush;
        private SpriteVisual _mlVisual;
        private uint _mlLogicalW, _mlLogicalH;

        public event Action<string> OnStatusChanged;

        public OverlayPump(FrameworkElement hostElement)
        {
            _hostElement = hostElement ?? throw new ArgumentNullException(nameof(hostElement));
        }

        public void Start(IntPtr widgetHwnd)
        {
            _hwnd = widgetHwnd;
            _cts = new CancellationTokenSource();

            InitVisualTree();

            // Run connection loop in background
            _readerTask = Task.Run(() => ConnectionLoop(_cts.Token), _cts.Token);

            _hostElement.SizeChanged += OnHostSizeChanged;
            Windows.UI.Xaml.Media.CompositionTarget.Rendering += OnRendering;
        }

        private void InitVisualTree()
        {
            Visual hostVisual = ElementCompositionPreview.GetElementVisual(_hostElement);
            _compositor = hostVisual.Compositor;
            _rootVisual = _compositor.CreateContainerVisual();
            ElementCompositionPreview.SetElementChildVisual(_hostElement, _rootVisual);
        }

        private void ConnectionLoop(CancellationToken token)
        {
            while (!token.IsCancellationRequested)
            {
                OnStatusChanged?.Invoke("Connecting to Core Server...");
                _pipe = CreateFileW(PipePath, GenericRead | GenericWrite, 0, IntPtr.Zero, OpenExisting, 0, IntPtr.Zero);

                if (_pipe == InvalidHandleValue)
                {
                    // Pipe busy or not found, wait and retry
                    Task.Delay(500, token).Wait();
                    continue;
                }

                try
                {
                    // Register Monitor
                    byte[] pidPayload = BitConverter.GetBytes(GetCurrentProcessId());
                    WriteIpcMessage(_pipe, OP_REGISTER_MONITOR, pidPayload);
                    OnStatusChanged?.Invoke("Registered with Core Server. Waiting for Canvas...");

                    // Read Loop
                    byte[] headerBuf = new byte[12];
                    while (!token.IsCancellationRequested)
                    {
                        if (!ReadExact(_pipe, headerBuf))
                        {
                            break; // Connection lost
                        }

                        uint magic = BitConverter.ToUInt32(headerBuf, 0);
                        ushort version = BitConverter.ToUInt16(headerBuf, 4);
                        ushort opcode = BitConverter.ToUInt16(headerBuf, 6);
                        uint payloadLen = BitConverter.ToUInt32(headerBuf, 8);

                        if (magic != IPC_MAGIC) break;

                        byte[] payload = null;
                        if (payloadLen > 0)
                        {
                            payload = new byte[payloadLen];
                            if (!ReadExact(_pipe, payload)) break;
                        }

                        HandleMessage(opcode, payload);
                    }
                }
                catch (Exception ex)
                {
                    Debug.WriteLine($"[OverlayPump] IPC Loop error: {ex}");
                }
                finally
                {
                    if (_pipe != IntPtr.Zero && _pipe != InvalidHandleValue)
                    {
                        CloseHandle(_pipe);
                        _pipe = IntPtr.Zero;
                    }
                }

                // Cleanup UI upon disconnect
                _ = _hostElement.Dispatcher.RunAsync(Windows.UI.Core.CoreDispatcherPriority.Normal, ClearSurfaces);
                OnStatusChanged?.Invoke("Disconnected. Retrying...");
                Task.Delay(1000, token).Wait();
            }
        }

        private void HandleMessage(ushort opcode, byte[] payload)
        {
            switch (opcode)
            {
                case OP_CANVAS_ATTACHED:
                {
                    if (payload == null || payload.Length < 28) break;
                    long handleRaw = unchecked((long)BitConverter.ToUInt64(payload, 4));
                    uint logW = BitConverter.ToUInt32(payload, 12);
                    uint logH = BitConverter.ToUInt32(payload, 16);

                    _ = _hostElement.Dispatcher.RunAsync(Windows.UI.Core.CoreDispatcherPriority.Normal, () =>
                    {
                        try { MountWorldSurface(new IntPtr(handleRaw), logW, logH); }
                        catch (Exception ex) { Debug.WriteLine($"[OverlayPump] MountWorldSurface threw: {ex}"); }
                    });
                    break;
                }
                case OP_MONITOR_LOCAL_ATTACHED:
                {
                    if (payload == null || payload.Length < 24) break;
                    long handleRaw = unchecked((long)BitConverter.ToUInt64(payload, 8));
                    uint logW = BitConverter.ToUInt32(payload, 16);
                    uint logH = BitConverter.ToUInt32(payload, 20);

                    _ = _hostElement.Dispatcher.RunAsync(Windows.UI.Core.CoreDispatcherPriority.Normal, () =>
                    {
                        try { MountMonitorLocalSurface(new IntPtr(handleRaw), logW, logH); }
                        catch (Exception ex) { Debug.WriteLine($"[OverlayPump] MountMonitorLocalSurface threw: {ex}"); }
                    });
                    break;
                }
                case OP_APP_DETACHED:
                {
                    _ = _hostElement.Dispatcher.RunAsync(Windows.UI.Core.CoreDispatcherPriority.Normal, () =>
                    {
                        try { ClearSurfaces(); } catch { }
                    });
                    break;
                }
                default:
                    Debug.WriteLine($"[OverlayPump] Unknown opcode: {opcode}");
                    break;
            }
        }

        private void MountWorldSurface(IntPtr handle, uint logicalW, uint logicalH)
        {
            ClearWorldSurface();

            _worldLogicalW = logicalW;
            _worldLogicalH = logicalH;

            ICompositorInterop interop = (ICompositorInterop)(object)_compositor;
            interop.CreateCompositionSurfaceForHandle(handle, out object surfaceObj);
            _worldSurface = (ICompositionSurface)surfaceObj;

            _worldBrush = _compositor.CreateSurfaceBrush(_worldSurface);
            _worldBrush.Stretch = CompositionStretch.Fill;

            _worldVisual = _compositor.CreateSpriteVisual();
            _worldVisual.Brush = _worldBrush;

            // World visual goes at the bottom (index 0)
            _rootVisual.Children.InsertAtBottom(_worldVisual);

            UpdateVisualTransform();
            OnStatusChanged?.Invoke($"Attached World Canvas: {logicalW}x{logicalH}");
        }

        private void MountMonitorLocalSurface(IntPtr handle, uint logicalW, uint logicalH)
        {
            ClearMonitorLocalSurface();

            _mlLogicalW = logicalW;
            _mlLogicalH = logicalH;

            ICompositorInterop interop = (ICompositorInterop)(object)_compositor;
            interop.CreateCompositionSurfaceForHandle(handle, out object surfaceObj);
            _mlSurface = (ICompositionSurface)surfaceObj;

            _mlBrush = _compositor.CreateSurfaceBrush(_mlSurface);
            _mlBrush.Stretch = CompositionStretch.Fill;

            _mlVisual = _compositor.CreateSpriteVisual();
            _mlVisual.Brush = _mlBrush;

            // Scale and size are fixed to the widget's internal dip sizes
            _mlVisual.Size = new Vector2((float)logicalW, (float)logicalH);
            _mlVisual.Offset = new Vector3(0, 0, 0);

            // ML visual goes on top
            _rootVisual.Children.InsertAtTop(_mlVisual);
            OnStatusChanged?.Invoke($"Attached MonitorLocal Surface: {logicalW}x{logicalH}");
        }

        private void ClearSurfaces()
        {
            ClearWorldSurface();
            ClearMonitorLocalSurface();
        }

        private void ClearWorldSurface()
        {
            if (_worldVisual != null)
            {
                _rootVisual.Children.Remove(_worldVisual);
                _worldVisual.Dispose();
                _worldVisual = null;
            }
            if (_worldBrush != null) { _worldBrush.Dispose(); _worldBrush = null; }
            if (_worldSurface is IDisposable ds) { ds.Dispose(); }
            _worldSurface = null;
        }

        private void ClearMonitorLocalSurface()
        {
            if (_mlVisual != null)
            {
                _rootVisual.Children.Remove(_mlVisual);
                _mlVisual.Dispose();
                _mlVisual = null;
            }
            if (_mlBrush != null) { _mlBrush.Dispose(); _mlBrush = null; }
            if (_mlSurface is IDisposable ds) { ds.Dispose(); }
            _mlSurface = null;
        }

        private void OnHostSizeChanged(object sender, SizeChangedEventArgs e) => UpdateVisualTransform();

        private void OnRendering(object sender, object e) => UpdateVisualTransform();

        private void UpdateVisualTransform()
        {
            if (_worldVisual == null || _hwnd == IntPtr.Zero) return;

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
            float logicalDipW = (float)(_worldLogicalW / scale);
            float logicalDipH = (float)(_worldLogicalH / scale);

            try
            {
                _worldVisual.Size = new Vector2(logicalDipW, logicalDipH);
                _worldVisual.Offset = new Vector3((float)(-viewportX / scale), (float)(-viewportY / scale), 0f);
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayPump] UpdateVisualTransform failed: {ex.Message}");
            }
        }

        private static void WriteIpcMessage(IntPtr pipe, ushort opcode, byte[] payload)
        {
            int headerSize = 12;
            byte[] msg = new byte[headerSize + (payload?.Length ?? 0)];
            BitConverter.GetBytes(IPC_MAGIC).CopyTo(msg, 0);
            BitConverter.GetBytes(IPC_VERSION).CopyTo(msg, 4);
            BitConverter.GetBytes(opcode).CopyTo(msg, 6);
            BitConverter.GetBytes((uint)(payload?.Length ?? 0)).CopyTo(msg, 8);
            if (payload != null && payload.Length > 0)
                Buffer.BlockCopy(payload, 0, msg, headerSize, payload.Length);
            WriteFile(pipe, msg, (uint)msg.Length, out _, IntPtr.Zero);
        }

        private static bool ReadExact(IntPtr pipe, byte[] buf)
        {
            int offset = 0;
            while (offset < buf.Length)
            {
                byte[] chunk = new byte[buf.Length - offset];
                if (!ReadFile(pipe, chunk, (uint)chunk.Length, out uint read, IntPtr.Zero) || read == 0)
                    return false;
                Buffer.BlockCopy(chunk, 0, buf, offset, (int)read);
                offset += (int)read;
            }
            return true;
        }

        public void Stop()
        {
            _cts?.Cancel();
            try { Windows.UI.Xaml.Media.CompositionTarget.Rendering -= OnRendering; } catch { }
            try { _hostElement.SizeChanged -= OnHostSizeChanged; } catch { }

            _ = _hostElement.Dispatcher.RunAsync(Windows.UI.Core.CoreDispatcherPriority.Normal, () =>
            {
                ClearSurfaces();
                try { ElementCompositionPreview.SetElementChildVisual(_hostElement, null); } catch { }
                if (_rootVisual != null) { _rootVisual.Dispose(); _rootVisual = null; }
            });

            if (_pipe != IntPtr.Zero && _pipe != InvalidHandleValue)
            {
                try { CloseHandle(_pipe); } catch { }
            }
            _pipe = IntPtr.Zero;
        }

        public void Dispose() => Stop();

        [ComImport]
        [Guid("25297D5C-3AD4-4C9C-B5CF-E36A38512330")]
        [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
        public interface ICompositorInterop
        {
            void CreateCompositionSurfaceForHandle(
                IntPtr swapChainHandle,
                [MarshalAs(UnmanagedType.IInspectable)] out object surface);

            void CreateCompositionSurfaceForSwapChain(
                IntPtr swapChain,
                [MarshalAs(UnmanagedType.IInspectable)] out object surface);

            void CreateGraphicsDevice(
                IntPtr renderingDevice,
                [MarshalAs(UnmanagedType.IInspectable)] out object device);
        }
    }
}