use std::{env, fs, io, path::PathBuf};

const SERVER_KEY: &str = "server";

pub fn load_server() -> Option<String> {
    let path = config_path()?;
    let text = fs::read_to_string(path).ok()?;
    parse_server(&text)
}

pub fn save_server(server: &str) -> io::Result<()> {
    let path = config_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "HOME/XDG_CONFIG_HOME is unavailable",
        )
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format!("{SERVER_KEY}={}\n", server.trim()))
}

fn config_path() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir).join("orsiktop/config"));
    }
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".config/orsiktop/config"))
}

fn parse_server(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        if key.trim() != SERVER_KEY {
            return None;
        }
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_saved_server() {
        assert_eq!(
            parse_server("server=http://10.0.0.7:9090\n"),
            Some("http://10.0.0.7:9090".to_string())
        );
    }
}
