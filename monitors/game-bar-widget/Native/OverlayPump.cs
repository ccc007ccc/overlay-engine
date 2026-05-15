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
        private const int HeaderSize = 12;
        private const int MaxControlPayloadBytes = 4096;
        private const long IdlePollTicks = 66 * TimeSpan.TicksPerMillisecond;
        private const long FastPollTicks = 16 * TimeSpan.TicksPerMillisecond;
        private const long FastTrackingTicks = 400 * TimeSpan.TicksPerMillisecond;

        private enum IpcOpcode : ushort
        {
            // Must match core-server/src/ipc/protocol.rs.
            RegisterMonitor = 0x0002,
            RegisterMonitorV2 = 0x0010,
            CanvasAttached = 0x0005,
            MonitorLocalAttached = 0x0007,
            AppDetached = 0x0008
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

        private bool _sizeChangedSubscribed;
        private bool _renderingSubscribed;
        private bool _layoutDirty = true;
        private bool _hasWindowRect;
        private bool _hasAppliedTransform;
        private ScreenInterop.RECT _lastWindowRect;
        private Point _cachedHostOrigin;
        private double _cachedScale = 1.0;
        private float _lastViewportX;
        private float _lastViewportY;
        private double _lastScale = 1.0;
        private float _lastWorldDipW;
        private float _lastWorldDipH;
        private long _nextWindowSampleTicks;
        private long _fastPollUntilTicks;

        public event Action<string> OnStatusChanged;

        public OverlayPump(FrameworkElement hostElement)
        {
            _hostElement = hostElement ?? throw new ArgumentNullException(nameof(hostElement));
        }

        public void Start(IntPtr widgetHwnd)
        {
            _hwnd = widgetHwnd;
            if (_cts != null)
            {
                MarkLayoutDirty();
                return;
            }

            _cts = new CancellationTokenSource();

            InitVisualTree();
            SubscribeSizeChanged();
            ResetTransformCache();

            _readerTask = ConnectionLoopAsync(_cts.Token);
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
            byte[] headerBuf = new byte[HeaderSize];
            byte[] payloadBuf = new byte[MaxControlPayloadBytes];

            while (!token.IsCancellationRequested)
            {
                OnStatusChanged?.Invoke("Connecting to Core Server...");

                _pipeStream?.Dispose();
                _pipeStream = new NamedPipeClientStream(".", PipeName, PipeDirection.InOut, PipeOptions.Asynchronous);

                try
                {
                    await _pipeStream.ConnectAsync(500, token);
                    OnStatusChanged?.Invoke("Connected. Registering...");

                    byte[] registerPayload = BuildRegisterMonitorV2Payload();
                    await WriteIpcMessageAsync(_pipeStream, IpcOpcode.RegisterMonitorV2, registerPayload, token);

                    OnStatusChanged?.Invoke("Registered with Core Server. Waiting for Canvas...");

                    while (!token.IsCancellationRequested)
                    {
                        if (!await ReadExactAsync(_pipeStream, headerBuf, HeaderSize, token))
                            break;

                        uint magic = ReadUInt32LittleEndian(headerBuf, 0);
                        ushort version = ReadUInt16LittleEndian(headerBuf, 4);
                        ushort opcode = ReadUInt16LittleEndian(headerBuf, 6);
                        uint payloadLen = ReadUInt32LittleEndian(headerBuf, 8);

                        if (magic != IPC_MAGIC || version != IPC_VERSION) break;
                        if (payloadLen > MaxControlPayloadBytes) break;

                        if (payloadLen > 0)
                        {
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

                if (token.IsCancellationRequested) break;

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
                    long handleRaw = unchecked((long)ReadUInt64LittleEndian(payload, 4));
                    uint logW = ReadUInt32LittleEndian(payload, 12);
                    uint logH = ReadUInt32LittleEndian(payload, 16);

                    _ = _hostElement.Dispatcher.RunAsync(Windows.UI.Core.CoreDispatcherPriority.Normal, () =>
                    {
                        try
                        {
                            _worldLayer.MountSurface(new IntPtr(handleRaw), logW, logH);
                            EnsureRenderingSubscribed();
                            MarkLayoutDirty();
                            TryUpdateVisualTransform(force: true);
                            OnStatusChanged?.Invoke($"Attached World Canvas: {logW}x{logH}");
                        }
                        catch (Exception ex) { Debug.WriteLine($"[OverlayPump] MountWorldSurface threw: {ex}"); }
                    });
                    break;
                }
                case IpcOpcode.MonitorLocalAttached:
                {
                    if (payloadLen < 24) break;
                    long handleRaw = unchecked((long)ReadUInt64LittleEndian(payload, 8));
                    uint logW = ReadUInt32LittleEndian(payload, 16);
                    uint logH = ReadUInt32LittleEndian(payload, 20);

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
                    Debug.WriteLine($"[OverlayPump] Unknown opcode: {(ushort)opcode}");
                    break;
            }
        }

        private void ClearAllSurfaces()
        {
            UnsubscribeRendering();
            _worldLayer?.Clear();
            _mlLayer?.Clear();
            ResetTransformCache();
        }

        public void MarkLayoutDirty()
        {
            _layoutDirty = true;
            _nextWindowSampleTicks = 0;
        }

        private void OnHostSizeChanged(object sender, SizeChangedEventArgs e) => MarkLayoutDirty();

        private void OnRendering(object sender, object e) => TryUpdateVisualTransform(force: false);

        private void TryUpdateVisualTransform(bool force)
        {
            if (_worldLayer == null || !_worldLayer.IsMounted || _hwnd == IntPtr.Zero) return;

            long now = DateTime.UtcNow.Ticks;
            if (!force && now < _nextWindowSampleTicks) return;

            if (!ScreenInterop.TryGetWindowScreenRect(_hwnd, out var winRect))
            {
                _nextWindowSampleTicks = now + IdlePollTicks;
                return;
            }

            bool windowChanged = !_hasWindowRect || !SameRect(_lastWindowRect, winRect);
            if (windowChanged)
            {
                _lastWindowRect = winRect;
                _hasWindowRect = true;
                _fastPollUntilTicks = now + FastTrackingTicks;
            }

            if (_layoutDirty || force)
            {
                RefreshLayoutMetrics();
            }

            _nextWindowSampleTicks = now + (now < _fastPollUntilTicks ? FastPollTicks : IdlePollTicks);

            double scale = _cachedScale > 0 ? _cachedScale : 1.0;
            float viewportX = (float)(winRect.left + _cachedHostOrigin.X * scale);
            float viewportY = (float)(winRect.top + _cachedHostOrigin.Y * scale);
            float logicalDipW = (float)(_worldLayer.LogicalW / scale);
            float logicalDipH = (float)(_worldLayer.LogicalH / scale);

            if (_hasAppliedTransform
                && NearlyEqual(_lastViewportX, viewportX, 0.1f)
                && NearlyEqual(_lastViewportY, viewportY, 0.1f)
                && Math.Abs(_lastScale - scale) < 0.01
                && NearlyEqual(_lastWorldDipW, logicalDipW, 0.1f)
                && NearlyEqual(_lastWorldDipH, logicalDipH, 0.1f))
            {
                return;
            }

            try
            {
                _worldLayer.SetFixedTransform((float)(-viewportX / scale), (float)(-viewportY / scale), logicalDipW, logicalDipH);
                _lastViewportX = viewportX;
                _lastViewportY = viewportY;
                _lastScale = scale;
                _lastWorldDipW = logicalDipW;
                _lastWorldDipH = logicalDipH;
                _hasAppliedTransform = true;
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[OverlayPump] UpdateVisualTransform failed: {ex.Message}");
            }
        }

        private void RefreshLayoutMetrics()
        {
            double scale = _hostElement.XamlRoot?.RasterizationScale ?? 1.0;
            if (scale <= 0) scale = 1.0;

            Point hostOrigin = new Point(0, 0);
            try
            {
                hostOrigin = _hostElement.TransformToVisual(null).TransformPoint(new Point(0, 0));
            }
            catch { }

            _cachedScale = scale;
            _cachedHostOrigin = hostOrigin;
            _layoutDirty = false;
        }

        private void ResetTransformCache()
        {
            _layoutDirty = true;
            _hasWindowRect = false;
            _hasAppliedTransform = false;
            _cachedHostOrigin = new Point(0, 0);
            _cachedScale = 1.0;
            _lastViewportX = 0;
            _lastViewportY = 0;
            _lastScale = 1.0;
            _lastWorldDipW = 0;
            _lastWorldDipH = 0;
            _nextWindowSampleTicks = 0;
            _fastPollUntilTicks = 0;
        }

        private void SubscribeSizeChanged()
        {
            if (_sizeChangedSubscribed) return;
            _hostElement.SizeChanged += OnHostSizeChanged;
            _sizeChangedSubscribed = true;
        }

        private void UnsubscribeSizeChanged()
        {
            if (!_sizeChangedSubscribed) return;
            try { _hostElement.SizeChanged -= OnHostSizeChanged; } catch { }
            _sizeChangedSubscribed = false;
        }

        private void EnsureRenderingSubscribed()
        {
            if (_renderingSubscribed) return;
            Windows.UI.Xaml.Media.CompositionTarget.Rendering += OnRendering;
            _renderingSubscribed = true;
        }

        private void UnsubscribeRendering()
        {
            if (!_renderingSubscribed) return;
            try { Windows.UI.Xaml.Media.CompositionTarget.Rendering -= OnRendering; } catch { }
            _renderingSubscribed = false;
        }

        private static bool SameRect(ScreenInterop.RECT a, ScreenInterop.RECT b)
        {
            return a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom;
        }

        private static bool NearlyEqual(float a, float b, float epsilon)
        {
            return Math.Abs(a - b) < epsilon;
        }

        private static ushort ReadUInt16LittleEndian(byte[] buffer, int offset)
        {
            return (ushort)(buffer[offset] | (buffer[offset + 1] << 8));
        }

        private static uint ReadUInt32LittleEndian(byte[] buffer, int offset)
        {
            return (uint)(buffer[offset]
                | (buffer[offset + 1] << 8)
                | (buffer[offset + 2] << 16)
                | (buffer[offset + 3] << 24));
        }

        private static ulong ReadUInt64LittleEndian(byte[] buffer, int offset)
        {
            uint lo = ReadUInt32LittleEndian(buffer, offset);
            uint hi = ReadUInt32LittleEndian(buffer, offset + 4);
            return lo | ((ulong)hi << 32);
        }

        private static void WriteUInt16LittleEndian(byte[] buffer, int offset, ushort value)
        {
            buffer[offset] = (byte)value;
            buffer[offset + 1] = (byte)(value >> 8);
        }

        private static void WriteUInt32LittleEndian(byte[] buffer, int offset, uint value)
        {
            buffer[offset] = (byte)value;
            buffer[offset + 1] = (byte)(value >> 8);
            buffer[offset + 2] = (byte)(value >> 16);
            buffer[offset + 3] = (byte)(value >> 24);
        }

        private static byte[] BuildRegisterMonitorV2Payload()
        {
            byte[] payload = new byte[23];
            WriteUInt32LittleEndian(payload, 0, (uint)Process.GetCurrentProcess().Id);
            payload[4] = 2;
            WriteUInt32LittleEndian(payload, 5, 0);
            WriteUInt32LittleEndian(payload, 9, 0);
            WriteUInt32LittleEndian(payload, 13, 0);
            payload[17] = 1;
            WriteUInt32LittleEndian(payload, 18, 0);
            payload[22] = 1;
            return payload;
        }

        private static async Task WriteIpcMessageAsync(NamedPipeClientStream stream, IpcOpcode opcode, byte[] payload, CancellationToken token)
        {
            int payloadLength = payload?.Length ?? 0;
            byte[] msg = System.Buffers.ArrayPool<byte>.Shared.Rent(HeaderSize + payloadLength);
            try
            {
                WriteUInt32LittleEndian(msg, 0, IPC_MAGIC);
                WriteUInt16LittleEndian(msg, 4, IPC_VERSION);
                WriteUInt16LittleEndian(msg, 6, (ushort)opcode);
                WriteUInt32LittleEndian(msg, 8, (uint)payloadLength);

                if (payloadLength > 0)
                    Array.Copy(payload, 0, msg, HeaderSize, payloadLength);

                await stream.WriteAsync(msg, 0, HeaderSize + payloadLength, token);
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
            var cts = _cts;
            if (cts == null) return;
            _cts = null;
            cts.Cancel();

            UnsubscribeRendering();
            UnsubscribeSizeChanged();

            try { _pipeStream?.Dispose(); } catch { }
            _pipeStream = null;

            _ = _hostElement.Dispatcher.RunAsync(Windows.UI.Core.CoreDispatcherPriority.Normal, () =>
            {
                ClearAllSurfaces();
                try { ElementCompositionPreview.SetElementChildVisual(_hostElement, null); } catch { }
                if (_rootVisual != null) { _rootVisual.Dispose(); _rootVisual = null; }
            });
        }

        public void Dispose() => Stop();

        private class OverlayLayer : IDisposable
        {
            private readonly Compositor _compositor;
            private readonly ContainerVisual _parent;
            private readonly bool _isTopLayer;

            private ICompositionSurface _surface;
            private CompositionSurfaceBrush _brush;
            private SpriteVisual _visual;
            private bool _hasTransform;
            private float _lastOffsetX;
            private float _lastOffsetY;
            private float _lastWidth;
            private float _lastHeight;

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

                try
                {
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
                finally
                {
                    ScreenInterop.CloseHandleQuietly(handle);
                }
            }

            public void SetFixedTransform(float offsetX, float offsetY, float width = -1, float height = -1)
            {
                if (_visual == null) return;

                if (width < 0) width = LogicalW;
                if (height < 0) height = LogicalH;

                if (_hasTransform
                    && NearlyEqual(_lastOffsetX, offsetX, 0.01f)
                    && NearlyEqual(_lastOffsetY, offsetY, 0.01f)
                    && NearlyEqual(_lastWidth, width, 0.01f)
                    && NearlyEqual(_lastHeight, height, 0.01f))
                {
                    return;
                }

                _visual.Size = new Vector2(width, height);
                _visual.Offset = new Vector3(offsetX, offsetY, 0f);
                _lastOffsetX = offsetX;
                _lastOffsetY = offsetY;
                _lastWidth = width;
                _lastHeight = height;
                _hasTransform = true;
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
                LogicalW = 0;
                LogicalH = 0;
                _hasTransform = false;
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
