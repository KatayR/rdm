//! `rdm browser install / uninstall`: the extension files and the native messaging host
//! registration for Chrome and Brave (current user only).

use std::path::{Path, PathBuf};

use serde_json::json;

/// Fixed by the public key in the extension's manifest.
pub const EXTENSION_ID: &str = "hmkendmmfmfkmjcocflneimlmlgjbdmm";
pub const HOST_NAME: &str = "com.rdm.host";

/// The extension ships inside the binary, so the installed copy always matches it.
const EXTENSION_FILES: [(&str, &str); 8] = [
    ("manifest.json", include_str!("../../extension/manifest.json")),
    ("background.js", include_str!("../../extension/background.js")),
    ("catch.js", include_str!("../../extension/catch.js")),
    ("content.js", include_str!("../../extension/content.js")),
    ("rules.json", include_str!("../../extension/rules.json")),
    ("sniff.js", include_str!("../../extension/sniff.js")),
    ("types.js", include_str!("../../extension/types.js")),
    ("video.js", include_str!("../../extension/video.js")),
];

/// Where each browser looks for native messaging hosts.
#[cfg(windows)]
const BROWSERS: [(&str, &str); 2] = [
    ("Chrome", r"Software\Google\Chrome\NativeMessagingHosts"),
    ("Brave", r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts"),
];
#[cfg(target_os = "macos")]
const BROWSERS: [(&str, &str); 2] = [
    ("Chrome", "Library/Application Support/Google/Chrome/NativeMessagingHosts"),
    ("Brave", "Library/Application Support/BraveSoftware/Brave-Browser/NativeMessagingHosts"),
];

pub fn install(home: &Path) -> Result<(), String> {
    let host = host_exe()?;
    let extension_dir = home.join("extension");
    std::fs::create_dir_all(&extension_dir).map_err(|e| e.to_string())?;
    // The manifest last: a copy of the extension that runs in the browser loads the new
    // files when it sees the manifest's version change (background.js), and they must all
    // be there by then.
    for (name, content) in EXTENSION_FILES.iter().skip(1).chain(EXTENSION_FILES.first()) {
        std::fs::write(extension_dir.join(name), content).map_err(|e| e.to_string())?;
    }
    let manifest = serde_json::to_string_pretty(&json!({
        "name": HOST_NAME,
        "description": "RDM (Rusty Download Manager)",
        "path": host,
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{EXTENSION_ID}/")],
    }))
    .expect("plain JSON");
    let manifest_path = home.join(format!("{HOST_NAME}.json"));
    std::fs::write(&manifest_path, manifest).map_err(|e| e.to_string())?;
    for (browser, location) in BROWSERS {
        register(location, &manifest_path).map_err(|e| format!("{browser}: {e}"))?;
        println!("registered the host for {browser}");
    }
    println!(
        "\nLoad the extension once in each browser:\n  \
         1. open chrome://extensions (Brave: brave://extensions)\n  \
         2. switch on Developer mode\n  \
         3. Load unpacked -> {}\n\
         Its ID must read {EXTENSION_ID}.",
        extension_dir.display()
    );
    Ok(())
}

pub fn uninstall(home: &Path) -> Result<(), String> {
    for (browser, location) in BROWSERS {
        unregister(location).map_err(|e| format!("{browser}: {e}"))?;
        println!("removed the host for {browser}");
    }
    let manifest = home.join(format!("{HOST_NAME}.json"));
    if manifest.exists() {
        std::fs::remove_file(manifest).map_err(|e| e.to_string())?;
    }
    println!("Remove the extension in each browser's extensions page; its files stay in {}", home.join("extension").display());
    Ok(())
}

/// `rdm-host` next to this executable.
fn host_exe() -> Result<PathBuf, String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let host = me.with_file_name(format!("rdm-host{}", std::env::consts::EXE_SUFFIX));
    if !host.exists() {
        return Err(format!("{} is missing; build both binaries", host.display()));
    }
    Ok(host)
}

#[cfg(windows)]
fn register(location: &str, manifest: &Path) -> std::io::Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(format!(r"{location}\{HOST_NAME}"))?;
    key.set_value("", &manifest.as_os_str())
}

#[cfg(windows)]
fn unregister(location: &str) -> std::io::Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    match RegKey::predef(HKEY_CURRENT_USER).delete_subkey(format!(r"{location}\{HOST_NAME}")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(target_os = "macos")]
fn register(location: &str, manifest: &Path) -> std::io::Result<()> {
    let dir = dirs::home_dir().ok_or(std::io::ErrorKind::NotFound)?.join(location);
    std::fs::create_dir_all(&dir)?;
    std::fs::copy(manifest, dir.join(format!("{HOST_NAME}.json"))).map(|_| ())
}

#[cfg(target_os = "macos")]
fn unregister(location: &str) -> std::io::Result<()> {
    let file = dirs::home_dir().ok_or(std::io::ErrorKind::NotFound)?.join(location).join(format!("{HOST_NAME}.json"));
    match std::fs::remove_file(file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use sha2::{Digest, Sha256};

    use super::*;

    /// The ID Chrome derives from the manifest key: the first 32 hex digits of the key's
    /// SHA-256, written with the letters a to p.
    #[test]
    fn extension_id_matches_the_manifest_key() {
        let manifest: serde_json::Value = serde_json::from_str(EXTENSION_FILES[0].1).unwrap();
        let der = base64::engine::general_purpose::STANDARD.decode(manifest["key"].as_str().unwrap()).unwrap();
        let id: String =
            Sha256::digest(&der)[..16].iter().flat_map(|b| [b >> 4, b & 15]).map(|n| (b'a' + n) as char).collect();
        assert_eq!(id, EXTENSION_ID);
    }

    /// The setup makes the browsers forget the helper before it replaces the programs
    /// (app/installer/hooks.nsh), by the keys written here.
    #[cfg(windows)]
    #[test]
    fn the_setup_removes_the_keys_written_here() {
        let hooks = include_str!("../../app/installer/hooks.nsh");
        assert_eq!(hooks.matches("DeleteRegKey HKCU").count(), BROWSERS.len());
        for (_, key) in BROWSERS {
            let line = format!(r#"DeleteRegKey HKCU "{key}\{HOST_NAME}""#);
            assert!(hooks.contains(&line), "{line}");
        }
    }

    /// One version for the programs and the extension they ship.
    #[test]
    fn extension_version_is_the_programs() {
        let manifest: serde_json::Value = serde_json::from_str(EXTENSION_FILES[0].1).unwrap();
        assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn every_script_the_manifest_names_is_installed() {
        let manifest: serde_json::Value = serde_json::from_str(EXTENSION_FILES[0].1).unwrap();
        let mut named: Vec<&str> = vec![manifest["background"]["service_worker"].as_str().unwrap()];
        for script in manifest["content_scripts"].as_array().unwrap() {
            named.extend(script["js"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()));
        }
        // Scripts the service worker loads itself: importScripts("a.js", "b.js").
        let background = EXTENSION_FILES[1].1;
        let imports = background.split("importScripts(").nth(1).and_then(|s| s.split(')').next()).unwrap();
        named.extend(imports.split(',').map(|s| s.trim().trim_matches('"')));
        // Files a script reads from the extension's own folder: getURL("a.json").
        for (_, script) in EXTENSION_FILES {
            named.extend(script.split("chrome.runtime.getURL(\"").skip(1).filter_map(|s| s.split('"').next()));
        }
        assert!(named.contains(&"rules.json"), "{named:?}");
        for name in named {
            assert!(EXTENSION_FILES.iter().any(|(file, _)| *file == name), "{name} is not installed");
        }
    }
}
