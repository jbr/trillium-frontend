use trillium_client::Client;
use trillium_frontend::frontend;
use trillium_logger::Logger;
use trillium_smol::ClientConfig;

pub fn main() {
    env_logger::init();
    trillium_smol::run((
        Logger::new(),
        frontend!("./examples/client")
            .with_client(Client::new(ClientConfig::default()))
            .with_index_file("index.html"),
    ));
}
