#![windows_subsystem = "windows"]

mod goxlr;
mod hook;

use tokio::sync::mpsc;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let (tx, rx) = mpsc::unbounded_channel::<hook::VolumeEvent>();

    std::thread::Builder::new()
        .name("ll-keyboard-hook".into())
        .spawn(move || {
            hook::run_hook(tx);
        })
        .expect("failed to spawn hook thread");

    goxlr::run_client(rx).await;
}
