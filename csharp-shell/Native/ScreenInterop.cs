using System;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using Windows.UI.Core;

namespace OverlayWidget.Native
{
    /// <summary>
    /// CoreWindow 的 native HWND 互操作。
    ///
    /// UWP / WinRT 的 CoreWindow 内部包了一个真正的 HWND，但 CoreWindow 的托管
    /// API 不直接暴露。<c>ICoreWindowInterop</c> 是 windows.ui.core.h 里声明的
    /// 标准 COM 接口，UWP host 进程内 QueryInterface 必然成功。
    ///
    /// 用法（必须在 UI 线程）：
    /// <code>
    ///   var cw = Windows.UI.Core.CoreWindow.GetForCurrentThread();
    ///   var ip = (ICoreWindowInterop)(object)cw;
    ///   IntPtr hwnd = ip.GetWindowHandle();
    /// </code>
    ///
    /// vtable 顺序必须与 windows.ui.core.h 严格一致：
    ///   1. get_WindowHandle
    ///   2. put_MessageHandled
    /// 我们只用 #1，但 #2 也必须声明保证 vtable slot 对齐，否则 IUnknown
    /// 之外的方法槽位会错位。
    /// </summary>
    [ComImport]
    [Guid("45D64A29-A63E-4CB6-B498-5781D298CB4F")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface ICoreWindowInterop
    {
        [MethodImpl(MethodImplOptions.InternalCall, MethodCodeType = MethodCodeType.Runtime)]
        IntPtr GetWindowHandle();

        [MethodImpl(MethodImplOptions.InternalCall, MethodCodeType = MethodCodeType.Runtime)]
        void SetMessageHandled([In, MarshalAs(UnmanagedType.U1)] bool value);
    }

    /// <summary>
    /// 屏幕坐标互操作。把 widget 的逻辑变成"取景框"：
    /// renderer 的 canvas 是当前显示器分辨率（固定的 screen-space），
    /// widget 只是把自己屏幕矩形对应的子区域显示出来。
    ///
    /// 关键 API：
    /// - <see cref="GetCoreWindowHwnd"/>：拿当前线程 CoreWindow 的 HWND
    /// - <see cref="GetWindowScreenRect"/>：HWND 在物理屏幕的像素矩形
    /// - <see cref="GetMonitorRectForWindow"/>：HWND 所在显示器的整屏像素矩形
    ///
    /// 这些 user32 入口在 UWP AppContainer 的允许列表里（同 GetSystemMetrics）。
    /// </summary>
    internal static class ScreenInterop
    {
        // MONITOR_DEFAULTTONEAREST：HWND 不在任一 monitor（罕见）也兜底返最近的
        private const uint MONITOR_DEFAULTTONEAREST = 0x00000002;

        [StructLayout(LayoutKind.Sequential)]
        public struct RECT
        {
            public int left;
            public int top;
            public int right;
            public int bottom;

            public int Width => right - left;
            public int Height => bottom - top;

            public override string ToString() =>
                $"({left},{top})-({right},{bottom}) {Width}x{Height}";
        }

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        public struct MONITORINFO
        {
            public uint cbSize;     // 必须填 sizeof(MONITORINFO) = 40
            public RECT rcMonitor;  // 整屏物理像素矩形（多显示器下含 origin offset）
            public RECT rcWork;     // 去掉任务栏的工作区
            public uint dwFlags;    // 1 = MONITORINFOF_PRIMARY
        }

        [DllImport("user32.dll", SetLastError = true, ExactSpelling = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);

        [DllImport("user32.dll", ExactSpelling = true)]
        private static extern IntPtr MonitorFromWindow(IntPtr hwnd, uint dwFlags);

        [DllImport("user32.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool GetMonitorInfoW(IntPtr hMonitor, ref MONITORINFO lpmi);

        /// <summary>
        /// 当前线程 CoreWindow 的 HWND。必须在 UI 线程调，
        /// 在 widget host 进程内 ICoreWindowInterop QI 必然成功。
        /// 失败返 IntPtr.Zero。
        /// </summary>
        public static IntPtr GetCoreWindowHwnd()
        {
            try
            {
                CoreWindow cw = CoreWindow.GetForCurrentThread();
                if (cw == null) return IntPtr.Zero;
                ICoreWindowInterop ip = (ICoreWindowInterop)(object)cw;
                return ip.GetWindowHandle();
            }
            catch (Exception)
            {
                return IntPtr.Zero;
            }
        }

        /// <summary>
        /// HWND 在物理屏幕的像素矩形（带 origin，多显示器下可能为负）。失败返默认值。
        /// </summary>
        public static bool TryGetWindowScreenRect(IntPtr hwnd, out RECT rect)
        {
            rect = default;
            if (hwnd == IntPtr.Zero) return false;
            try
            {
                return GetWindowRect(hwnd, out rect);
            }
            catch (Exception)
            {
                return false;
            }
        }

        /// <summary>
        /// HWND 所在显示器的整屏物理像素矩形。HWND=Zero 或失败返 false。
        /// 多显示器下 rcMonitor 的 left/top 不是 0（虚拟屏 origin offset），
        /// 算 widget 的子区域时要用 (winRect.left - monRect.left, ...) 做 src 偏移。
        /// </summary>
        public static bool TryGetMonitorRectForWindow(IntPtr hwnd, out RECT monitorRect)
        {
            monitorRect = default;
            if (hwnd == IntPtr.Zero) return false;
            try
            {
                IntPtr mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                if (mon == IntPtr.Zero) return false;

                MONITORINFO mi = default;
                mi.cbSize = (uint)Marshal.SizeOf<MONITORINFO>();
                if (!GetMonitorInfoW(mon, ref mi)) return false;

                monitorRect = mi.rcMonitor;
                return true;
            }
            catch (Exception)
            {
                return false;
            }
        }
    }
}
