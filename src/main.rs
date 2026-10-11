use clap::Parser;

/// A headless music server for OpenSubsonic clients.
#[derive(Parser)]
#[command(name = "wn-server", version = wn_server::VERSION)]
struct Cli {}

fn main() {
    Cli::parse();
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    #[test]
    fn version_flag_prints_name_and_version() {
        let version = Cli::command().render_version();
        assert_eq!(version, format!("wn-server {}\n", wn_server::VERSION));
    }
}
