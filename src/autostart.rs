use crate::storage;
use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

// #######################################################
// manage the desktop login entry
// #######################################################

pub fn enabled(entry: &Path) -> bool {
    storage::read_text(entry).is_ok_and(|text| {
        !text
            .lines()
            .filter_map(|line| line.split_once('='))
            .any(|(key, value)| {
                matches!(
                    (key.trim(), value.trim()),
                    ("Hidden", "true") | ("X-GNOME-Autostart-enabled", "false")
                )
            })
    })
}

pub fn set_enabled(entry: &Path, enabled: bool) -> io::Result<()> {
    if !enabled {
        return match fs::remove_file(entry) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        };
    }

    // The executable inside an AppImage mount disappears when the app exits.
    let appimage = env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file());
    let executable = match appimage {
        Some(path) => path,
        None => env::current_exe()?,
    };
    let desktop = desktop_entry(&executable)?;
    if let Some(directory) = entry.parent() {
        fs::create_dir_all(directory)?;
    }
    storage::write_text(entry, desktop.as_bytes())
}

// #######################################################
// write a launcher for the current executable
// #######################################################

fn desktop_entry(executable: &Path) -> io::Result<String> {
    let path = executable
        .to_str()
        .filter(|s| !s.chars().any(char::is_control))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Unsupported characters in the executable path.",
            )
        })?;
    let mut exec = String::new();
    for char in path.chars() {
        // Escape Exec quoting and then the desktop file's value escaping.
        exec.push_str(match char {
            '\\' => r"\\\\",
            '"' => r#"\\""#,
            '$' => r"\\$",
            '\u{60}' => "\\\\\u{60}",
            '%' => "%%",
            _ => {
                exec.push(char);
                continue;
            }
        });
    }

    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=Simple Modern Equalizer\n\
         Icon=simple-modern-equalizer\nTerminal=false\n\
         TryExec={}\nExec=/usr/bin/env \"{exec}\" --autostart\n",
        path.replace('\\', r"\\"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_entry_can_be_enabled_and_removed() {
        let directory =
            env::temp_dir().join(format!("simple-eq-autostart-test-{}", std::process::id()));
        let entry = directory.join("autostart/simple-modern-equalizer.desktop");
        assert!(!enabled(&entry));
        set_enabled(&entry, true).unwrap();
        assert!(enabled(&entry));
        assert!(
            fs::read_to_string(&entry)
                .unwrap()
                .contains(" --autostart\n")
        );
        storage::write_text(&entry, b"[Desktop Entry]\nHidden=true\n").unwrap();
        assert!(!enabled(&entry));
        set_enabled(&entry, false).unwrap();
        set_enabled(&entry, false).unwrap();
        assert!(!entry.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn executable_path_escapes_desktop_entry_syntax() {
        let desktop = desktop_entry(Path::new("/tmp/Equalizer 100% $q\"\\test.AppImage")).unwrap();
        assert!(desktop.contains(concat!(
            "TryExec=/tmp/Equalizer 100% $q\"\\\\test.AppImage\n",
            "Exec=/usr/bin/env \"/tmp/Equalizer 100%% \\\\$q\\\\\"\\\\\\\\test.AppImage\" --autostart\n",
        )));
        assert!(desktop_entry(Path::new("/tmp/eq\nIcon=other")).is_err());
    }
}
