use std::io::Write;
use std::os::unix::net::UnixStream;

fn main() -> std::process::ExitCode {
    let cmd = match std::env::args().nth(1).as_deref() {
        Some("in") => "in",
        Some("out") => "out",
        Some("reset") => "reset",
        _ => {
            eprintln!("usage: niri-zoomctl <in|out|reset>");
            return std::process::ExitCode::FAILURE;
        }
    };

    let path = niri_zoom::socket_path();
    match UnixStream::connect(&path) {
        Ok(mut stream) => {
            if let Err(e) = writeln!(stream, "{cmd}") {
                eprintln!("niri-zoomctl: failed to write to daemon: {e}");
                return std::process::ExitCode::FAILURE;
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!(
                "niri-zoomctl: could not connect to {} ({e}). Is niri-zoomd running?",
                path.display()
            );
            std::process::ExitCode::FAILURE
        }
    }
}
