use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

pub fn run(page: Result<String, String>) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("Messages Gateway")
        .with_inner_size(LogicalSize::new(760.0, 760.0))
        .with_min_inner_size(LogicalSize::new(460.0, 540.0))
        .build(&event_loop)?;
    let builder = match page {
        Ok(url) => {
            let origin = reqwest::Url::parse(&url)?.origin();
            WebViewBuilder::new().with_url(url).with_navigation_handler(move |target| {
                reqwest::Url::parse(&target).is_ok_and(|url| url.origin() == origin)
            })
        }
        Err(message) => WebViewBuilder::new()
            .with_html("<!doctype html><html lang='zh-CN'><meta charset='utf-8'><style>body{font:14px system-ui;background:#f4f4f6;padding:36px;color:#1c1c21}h1{font-size:20px}p{line-height:1.7;white-space:pre-wrap}</style><h1>无法启动网关</h1><p id='error'></p><p>检查端口是否已被占用，或已有网关窗口正在运行。</p></html>")
            .with_initialization_script(format!("addEventListener('DOMContentLoaded',()=>document.getElementById('error').textContent={});", serde_json::to_string(&message)?)),
    }.with_clipboard(true);
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window)?;
    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder.build_gtk(window.default_vbox().ok_or("window has no GTK container")?)?
    };
    event_loop.run(move |event, _, control_flow| {
        let _ = (&window, &webview); // Keep both native handles alive for the event loop.
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            *control_flow = ControlFlow::Exit;
        }
    });
}
