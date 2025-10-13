use {once_cell::sync::OnceCell, serde_derive::Deserialize};

#[derive(Deserialize, Debug)]
pub struct Config {
    // port for service connection with proxy-client
    pub service_port: u16,
    // port for bridge connections with proxy-client
    pub bridge_port: u16,
    // port exposed to the public for client connections
    pub client_port: u16,
}

impl Config {
    pub fn new(path: &str) -> anyhow::Result<Self> {
        anyhow::Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
    }
}

static CONFIG: OnceCell<Config> = OnceCell::new();

pub fn init(path: &str) -> &Config {
    if let Ok(cfg) = Config::new(path) {
        CONFIG.set(cfg).unwrap();
    } else {
        panic!("cannot find config.toml");
    }
    get()
}

pub fn get<'a>() -> &'a Config {
    CONFIG.get().unwrap()
}
