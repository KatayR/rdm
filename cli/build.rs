//! Links the Visual C++ runtime into `rdm` and `rdm-host`, as tauri-build does for the
//! app: the programs then start on a Windows without that runtime installed.

fn main() {
    static_vcruntime::metabuild();
}
