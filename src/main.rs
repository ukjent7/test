#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
mod cache;
mod desktop;
mod error;
mod forwarding;
mod history;
mod protocol;
mod settings;
mod zen;

use std::{env, net::SocketAddr};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let headless = env::args().any(|argument| argument == "--headless");
    let runtime = tokio::runtime::Runtime::new()?;
    let (listener, router) = match runtime.block_on(app::prepare()) {
        Ok(server) => server,
        Err(error) if !headless => return desktop::run(Err(error.to_string())),
        Err(error) => return Err(error),
    };
    let address = listener.local_addr()?;
    eprintln!("messages-gateway listening on {address}");
    let server = async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
    };
    if headless {
        runtime.block_on(server)?;
    } else {
        runtime.spawn(server);
        desktop::run(Ok(format!("http://{address}/")))?;
    }
    Ok(())
}
