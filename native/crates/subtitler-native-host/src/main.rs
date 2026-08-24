use std::io::{self, Write};
use subtitler_native_host::{run_native_host, HostDispatcher};

fn main() {
    let dispatcher = HostDispatcher::new();
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();

    if let Err(error) = run_native_host(&mut input, &mut output, &dispatcher) {
        // Native Messaging reserves stdout for framed protocol responses. Do
        // not include request payloads, media URLs, or transcript text here.
        let _ = writeln!(io::stderr(), "Subtitler native host stopped: {error}");
        std::process::exit(1);
    }
}
