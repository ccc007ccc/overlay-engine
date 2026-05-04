using System;
using Windows.ApplicationModel;
using Windows.ApplicationModel.Activation;
using Windows.UI.Xaml;
using Windows.UI.Xaml.Controls;
using Windows.UI.Xaml.Navigation;
using Microsoft.Gaming.XboxGameBar;

namespace OverlayWidget
{
    /// <summary>
    /// 应用入口。同时支持普通桌面前台启动（MainPage，仅用于调试）和
    /// Game Bar widget 激活（MainWidget）。
    /// </summary>
    sealed partial class App : Application
    {
        private XboxGameBarWidget mainWidget = null;

        public App()
        {
            this.InitializeComponent();
            this.Suspending += OnSuspending;
        }

        protected override void OnActivated(IActivatedEventArgs args)
        {
            XboxGameBarWidgetActivatedEventArgs widgetArgs = null;
            if (args.Kind == ActivationKind.Protocol)
            {
                var protocolArgs = args as IProtocolActivatedEventArgs;
                string scheme = protocolArgs.Uri.Scheme;
                if (scheme.Equals("ms-gamebarwidget"))
                {
                    widgetArgs = args as XboxGameBarWidgetActivatedEventArgs;
                }
            }

            if (widgetArgs == null) return;

            // Activation Notes (摘自 sample)：
            //   IsLaunchActivation == true 表示 Game Bar 启动新 widget 实例
            //   —— 必须新建一个 XboxGameBarWidget 并保活。
            //   否则是后续激活（Game Bar 通过 URI 给当前 widget 发消息），
            //   不要重复创建。
            if (widgetArgs.IsLaunchActivation)
            {
                var rootFrame = new Frame();
                rootFrame.NavigationFailed += OnNavigationFailed;
                Window.Current.Content = rootFrame;

                // 创建 Game Bar widget 对象 —— 这一步建立与 Game Bar 的连接
                mainWidget = new XboxGameBarWidget(
                    widgetArgs,
                    Window.Current.CoreWindow,
                    rootFrame);
                // 把 widget 实例作为 navigation parameter 传给 MainWidget，
                // 让它能调 TryResizeWindowAsync / 读 MaxWindowSize 等 API
                rootFrame.Navigate(typeof(MainWidget), mainWidget);

                Window.Current.Closed += MainWidgetWindow_Closed;
                Window.Current.Activate();
            }
            else
            {
                // 后续激活：可以解析 URI 并改变 widget 行为，当前不需要
            }
        }

        private void MainWidgetWindow_Closed(object sender, Windows.UI.Core.CoreWindowEventArgs e)
        {
            mainWidget = null;
            Window.Current.Closed -= MainWidgetWindow_Closed;
        }

        protected override void OnLaunched(LaunchActivatedEventArgs e)
        {
            // 普通桌面启动（双击 tile 或 shell:AppsFolder 启动）—— 仅用于调试
            Frame rootFrame = Window.Current.Content as Frame;

            if (rootFrame == null)
            {
                rootFrame = new Frame();
                rootFrame.NavigationFailed += OnNavigationFailed;

                if (e.PreviousExecutionState == ApplicationExecutionState.Terminated)
                {
                    // 可选：恢复挂起前的状态
                }

                Window.Current.Content = rootFrame;
            }

            if (e.PrelaunchActivated == false)
            {
                if (rootFrame.Content == null)
                {
                    rootFrame.Navigate(typeof(MainPage), e.Arguments);
                }
                Window.Current.Activate();
            }
        }

        void OnNavigationFailed(object sender, NavigationFailedEventArgs e)
        {
            throw new Exception("Failed to load Page " + e.SourcePageType.FullName);
        }

        private void OnSuspending(object sender, SuspendingEventArgs e)
        {
            // Game Bar 维持 widget 连接时不会触发 Suspend；
            // 一旦走到这里就清理 widget 对象。
            var deferral = e.SuspendingOperation.GetDeferral();
            mainWidget = null;
            deferral.Complete();
        }
    }
}
