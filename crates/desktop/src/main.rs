#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
mod platform;
mod window;

use gpui_kit::{
    component::{Theme, ThemeMode},
    *,
};
use pi_qq_client_core::{Command, store::Store};

const APP_NAME: &str = "Pi QQ Herdr Client";

fn main() {
    if let Err(error) = launch() {
        eprintln!("{APP_NAME}: {error}");
        // A packaged window-only build must still surface initialization failures.
        #[cfg(windows)]
        {
            let _ = rfd::MessageDialog::new()
                .set_title(APP_NAME)
                .set_description(error.to_string())
                .set_level(rfd::MessageLevel::Error)
                .show();
        }
    }
}
fn launch() -> anyhow::Result<()> {
    let store = Store::open()?;
    let Some(_lock) = store.lock()? else {
        store.request_show()?;
        return Ok(());
    };
    platform::init()?;
    let client = pi_qq_client_core::start(store)?;
    let quit = client.commands.clone();
    let background = std::env::args().any(|a| a == "--background");
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            Theme::change(ThemeMode::Light, None, cx);
            cx.on_app_quit(move |_| {
                let _ = quit.send(Command::Shutdown);
                async {}
            })
            .detach();
            let bounds = Bounds::centered(None, size(px(440.), px(600.)), cx);
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(400.), px(540.))),
                    titlebar: Some(TitlebarOptions {
                        title: Some(APP_NAME.into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                cx,
                move |window, cx| {
                    cx.new(|cx| window::ClientWindow::new(client, background, window, cx))
                },
            )
            .expect("无法打开客户端窗口");
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            if !background {
                cx.activate(true);
            }
        });
    Ok(())
}
