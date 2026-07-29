use webcodex::{server_binary_action, ServerBinaryAction};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match server_binary_action(std::env::args().skip(1)) {
        ServerBinaryAction::Run => webcodex::run_server().await,
        ServerBinaryAction::Exit {
            code,
            stdout,
            stderr,
        } => {
            if !stdout.is_empty() {
                print!("{stdout}");
            }
            if !stderr.is_empty() {
                eprint!("{stderr}");
            }
            std::process::exit(code);
        }
    }
}
