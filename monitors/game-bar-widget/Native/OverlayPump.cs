using System;
using System.Diagnostics;
using System.IO.Pipes;
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
        private const string PipeName = "overlay-core";
        private const uint IPC_MAGIC = 0x4F56524C; // 'OVRL'
        private const ushort IPC_VERSION = 1;

        private enum IpcOpcode : ushort
        {
            RegisterMonitor = 0x0002,
            CanvasAttached = 0x0005,
            AppDetached = 0x0006,
            MonitorLocalAttached = 0x0007
        }

        private readonly FrameworkElement _hostElement;
        private IntPtr _hwnd;
        private NamedPipeClientStream _pipeStream;
        private CancellationTokenSource _cts;
        private Task _readerTask;

        private Compositor _compositor;
        private ContainerVisual _rootVisual;

        private OverlayLayer _worldLayer;
        private OverlayLayer _mlLayer;

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

            _readerTask = ConnectionLoopAsync(_cts.Token);

            _hostElement.SizeChanged += OnHostSizeChanged;
            Windows.UI.Xaml.Media.CompositionTarget.Rendering += OnRendering;
        }

        private void InitVisualTree()
        {
            Visual hostVisual = ElementCompositionPreview.GetElementVisual(_hostElement);
            _compositor = hostVisual.Compositor;
            _rootVisual = _compositor.CreateContainerVisual();
            ElementCompositionPreview.SetElementChildVisual(_hostElement, _rootVisual);

            _worldLayer = new OverlayLayer(_compositor, _rootVisual, isTopLayer: false);
            _mlLayer = new OverlayLayer(_compositor, _rootVisual, isTopLayer: true);
        }

        private async Task ConnectionLoopAsync(CancellationToken token)
        {
            byte[] headerBuf = new byte[12];
            byte[] payloadBuf = new byte[4096];

            while (!token.IsCancellationRequested)
            {
                OnStatusChanged?.Invoke("Connecting to Core Server...");

                // Dispose previous stream if any
                _pipeStream?.Dispose();
                _pipeStream = new NamedPipeClientStream(".", PipeName, PipeDirection.InOut, PipeOptions.Asynchronous);

                try
                {
                    // UWP sandbox allows named pipe connection if the server explicitly granted access
                    await _pipeStream.ConnectAsync(500, token);
                    OnStatusChanged?.Invoke("Connected. Registering...");

                    // Register Monitor
                    byte[] pidPayload = BitConverter.GetBytes(System.Diagnostics.Process.GetCurrentProcess().Id);
                    await WriteIpcMessageAsync(_pipeStream, IpcOpcode.RegisterMonitor, pidPayload, token);

                    OnStatusChanged?.Invoke("Registered with Core Server. Waiting for Canvas...");

                    // Read Loop
                    while (!token.IsCancellationRequested)
                    {
                        if (!await ReadExactAsync(_pipeStream, headerBuf, 12, token))
                            break; // Connection lost

                        uint magic = System.Buffers.Binary.BinaryPrimitives.ReadUInt32LittleEndian(headerBuf.AsSpan(0));
                        ushort version = System.Buffers.Binary.BinaryPrimitives.ReadUInt16LittleEndian(headerBuf.AsSpan(4));
                        ushort opcode = System.Buffers.Binary.BinaryPrimitives.ReadUInt16LittleEndian(headerBuf.AsSpan(6));
                        uint payloadLen = System.Buffers.Binary.BinaryPrimitives.ReadUInt32LittleEndian(headerBuf.AsSpan(8));

                        if (magic != IPC_MAGIC) break;

                        if (payloadLen > 0)
                        {
                            if (payloadLen > payloadBuf.Length)
                            {
                                // Resize buffer if a larger payload arrives (rare)
                                payloadBuf = new byte[payloadLen];
                            }
                            if (!await ReadExactAsync(_pipeStream, payloadBuf, (int)payloadLen, token))
                                break;
                        }

                        HandleMessage((IpcOpcode)opcode, payloadBuf, payloadLen);
                    }
                }
                catch (OperationCanceledException)
                {
                    break;
                }
                catch (Exception ex)
                {
                    Debug.WriteLine($"[OverlayPump] IPC Loop error: {ex.Message}");
                }
                finally
                {
                    _pipeStream?.Dispose();
                    _pipeStream = null;
                }

                // Cleanup UI upon disconnect
                _ = _hostElement.Dispatcher.RunAsync(Windows.UI.Core.CoreDispatcherPriority.Normal, ClearAllSurfaces);
                OnStatusChanged?.Invoke("Disconnected. Retrying...");

                try { await Task.Delay(1000, token); }
                catch (OperationCanceledException) { break; }
            }
        }

        private void HandleMessage(IpcOpcode opcode, byte[] payload, uint payloadLen)
        {
            switch (opcode)
            {
                case IpcOpcode.CanvasAttached:
                {
                    if (payloadLen < 28) break;
                    long handleRaw = unchecked((long)System.Buffers.Binary.BinaryPrimitives.ReadUInt64LittleEndian(payload.AsSpan(4)));
                    uint logW = System.Buffers.Binary.BinaryPrimitives.ReadUInt32LittleEndian(payload.AsSpan(12));
                    uint logH = System.Buffers.Binary.BinaryPrimitives.ReadUInt32LittleEndian(payload.AsSpan(16));

                    _ = _hostElement.Dispatcher.RunAsync(Windows.UI.Core.CoreDispatcherPriority.Normal, () =>
                    {
                        try
                        {
                            _worldLayer.MountSurface(new IntPtr(handleRaw), logW, logH);
                            UpdateVisualTransform();
                            OnStatusChanged?.Invoke($"Attached World Canvas: {logW}x{logH}");
                        }
                        catch (Exception ex) { Debug.WriteLine($"[OverlayPump] MountWorldSurface threw: {ex}"); }
                    });
                    break;
                }
                case IpcOpcode.MonitorLocalAttached:
                {
                    if (payloadLen < 24) break;
                    long handleRaw = unchecked((long)System.Buffers.Binary.BinaryPrimitives.ReadUInt64LittleEndian(payload.AsSpan(8)));
                    uint logW = System.Buffers.Binary.BinaryPrimitives.ReadUInt32LittleEndian(payload.AsSpan(16));
                    uint logH = System.Buffers.Binary.BinaryPrimitives.ReadUInt32LittleEndian(payload.AsSpan(20));

                    _ = _hostElement.Dispatcher.RunAsync(Windows.UI.Core.CoreDispatcherPriority.Normal, () =>
                    {
                        try
                        {
                            _mlLayer.MountSurface(new IntPtr(handleRaw), logW, logH);
                            _mlLayer.SetFixedTransform(0, 0);
                            OnStatusChanged?.Invoke($"Attached MonitorLocal Surface: {logW}x{logH}");
                        }
                        catch (Exception ex) { Debug.WriteLine($"[OverlayPump] MountMonitorLocalSurface threw: {ex}"); }
                    });
                    break;
                }
                case IpcOpcode.AppDetached:
                {
                    _ = _hostElement.Dispatcher.RunAsync(Windows.UI.Core.CoreDispatcherPriority.Normal, () =>
                    {
                        try { ClearAllSurfaces(); } catch { }
                    });
                    break;
                }
                default:
                    Debug.WriteLine($"[OverlayPump] Unknown opcode: {opcode}");
                    break;
            }
        }

        private void ClearAllSurfaces()
        {
            _worldLayer.Clear();
            _mlLayer.Clear();
        }

        private float _lastViewportX, _lastViewportY;
        private double _lastScale;

        private void OnHostSizeChanged(object sender, SizeChangedEventArgs e) => UpdateVisualTransform();

        private void OnRendering(object sender, object e) => UpdateVisualTransform();

        private void UpdateVisualTransform()
        {
            if (!_worldLayer.IsMounted || _hwnd == IntPtr.Zero) return;

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

            // Debounce unnecessary updates
            if (Math.Abs(_lastViewportX - viewportX) < 0.1 && Math.Abs(_lastViewportY - viewportY) < 0.1 && Math.Abs(_lastScale - scale) < 0.01)
                return;

            _lastViewportX = viewportX;
            _lastViewportY = viewportY;
            _lastScale = scale;

            float logicalDipW = (float)(_worldLayer.LogicalW / scale);
            float logicalDipH = (float)(_worldLayer.LogicalH / scale);

            try
            {
                _worldLayer.SetFixedTransform((float)(-viewportX / scale), (float)(-viewportY / scale), logicalDipW, logicalDipH);
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayPump] UpdateVisualTransform failed: {ex.Message}");
            }
        }

        private static async Task WriteIpcMessageAsync(NamedPipeClientStream stream, IpcOpcode opcode, byte[] payload, CancellationToken token)
        {
            int headerSize = 12;
            byte[] msg = System.Buffers.ArrayPool<byte>.Shared.Rent(headerSize + (payload?.Length ?? 0));
            try
            {
                Span<byte> span = msg.AsSpan();
                System.Buffers.Binary.BinaryPrimitives.WriteUInt32LittleEndian(span.Slice(0), IPC_MAGIC);
                System.Buffers.Binary.BinaryPrimitives.WriteUInt16LittleEndian(span.Slice(4), IPC_VERSION);
                System.Buffers.Binary.BinaryPrimitives.WriteUInt16LittleEndian(span.Slice(6), (ushort)opcode);
                System.Buffers.Binary.BinaryPrimitives.WriteUInt32LittleEndian(span.Slice(8), (uint)(payload?.Length ?? 0));

                if (payload != null && payload.Length > 0)
                    payload.AsSpan().CopyTo(span.Slice(12));

                await stream.WriteAsync(msg, 0, headerSize + (payload?.Length ?? 0), token);
                await stream.FlushAsync(token);
            }
            finally
            {
                System.Buffers.ArrayPool<byte>.Shared.Return(msg);
            }
        }

        private static async Task<bool> ReadExactAsync(NamedPipeClientStream stream, byte[] buf, int count, CancellationToken token)
        {
            int offset = 0;
            while (offset < count)
            {
                int read = await stream.ReadAsync(buf, offset, count - offset, token);
                if (read == 0) return false;
                offset += read;
            }
            return true;
        }

        public void Stop()
        {
            _cts?.Cancel();
            try { Windows.UI.Xaml.Media.CompositionTarget.Rendering -= OnRendering; } catch { }
            try { _hostElement.SizeChanged -= OnHostSizeChanged; } catch { }

            _pipeStream?.Dispose();

            _ = _hostElement.Dispatcher.RunAsync(Windows.UI.Core.CoreDispatcherPriority.Normal, () =>
            {
                ClearAllSurfaces();
                try { ElementCompositionPreview.SetElementChildVisual(_hostElement, null); } catch { }
                if (_rootVisual != null) { _rootVisual.Dispose(); _rootVisual = null; }
            });
        }

        public void Dispose() => Stop();

        // --------------------------------------------------------
        // Embedded Class: OverlayLayer
        // --------------------------------------------------------
        private class OverlayLayer : IDisposable
        {
            private readonly Compositor _compositor;
            private readonly ContainerVisual _parent;
            private readonly bool _isTopLayer;

            private ICompositionSurface _surface;
            private CompositionSurfaceBrush _brush;
            private SpriteVisual _visual;

            public uint LogicalW { get; private set; }
            public uint LogicalH { get; private set; }
            public bool IsMounted => _visual != null;

            public OverlayLayer(Compositor compositor, ContainerVisual parent, bool isTopLayer)
            {
                _compositor = compositor;
                _parent = parent;
                _isTopLayer = isTopLayer;
            }

            public void MountSurface(IntPtr handle, uint logicalW, uint logicalH)
            {
                Clear();

                LogicalW = logicalW;
                LogicalH = logicalH;

                ICompositorInterop interop = (ICompositorInterop)(object)_compositor;
                interop.CreateCompositionSurfaceForHandle(handle, out object surfaceObj);
                _surface = (ICompositionSurface)surfaceObj;

                _brush = _compositor.CreateSurfaceBrush(_surface);
                _brush.Stretch = CompositionStretch.Fill;

                _visual = _compositor.CreateSpriteVisual();
                _visual.Brush = _brush;

                if (_isTopLayer)
                    _parent.Children.InsertAtTop(_visual);
                else
                    _parent.Children.InsertAtBottom(_visual);
            }

            public void SetFixedTransform(float offsetX, float offsetY, float width = -1, float height = -1)
            {
                if (_visual == null) return;

                if (width < 0) width = LogicalW;
                if (height < 0) height = LogicalH;

                _visual.Size = new Vector2(width, height);
                _visual.Offset = new Vector3(offsetX, offsetY, 0f);
            }

            public void Clear()
            {
                if (_visual != null)
                {
                    _parent.Children.Remove(_visual);
                    _visual.Dispose();
                    _visual = null;
                }
                if (_brush != null) { _brush.Dispose(); _brush = null; }
                if (_surface is IDisposable ds) { ds.Dispose(); }
                _surface = null;
            }

            public void Dispose() => Clear();
        }

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