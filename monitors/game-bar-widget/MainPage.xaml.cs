using Windows.UI.Xaml.Controls;

namespace OverlayWidget
{
    /// <summary>
    /// 普通桌面前台启动入口（仅用于调试）。
    /// 真正的 widget 内容在 <see cref="MainWidget"/>。
    /// </summary>
    public sealed partial class MainPage : Page
    {
        public MainPage()
        {
            this.InitializeComponent();
        }
    }
}
