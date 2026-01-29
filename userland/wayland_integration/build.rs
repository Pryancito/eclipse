/// Build script for wayland_integration
/// Detects and links against libwayland and wlroots if available

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    
    // Register custom cfg flags
    println!("cargo::rustc-check-cfg=cfg(has_libwayland)");
    println!("cargo::rustc-check-cfg=cfg(has_wlroots)");
    println!("cargo::rustc-check-cfg=cfg(has_wayland_client)");
    
    // Try to find libwayland-server
    if cfg!(feature = "libwayland") {
        match pkg_config::Config::new()
            .atleast_version("1.18.0")
            .probe("wayland-server")
        {
            Ok(library) => {
                println!("cargo:rustc-cfg=has_libwayland");
                println!("cargo:warning=Found libwayland-server version {}", 
                    library.version);
                
                // Also check for wayland-client
                if let Ok(client) = pkg_config::probe_library("wayland-client") {
                    println!("cargo:rustc-cfg=has_wayland_client");
                    println!("cargo:warning=Found wayland-client version {}", 
                        client.version);
                }
            }
            Err(e) => {
                println!("cargo:warning=libwayland-server not found: {}. Using fallback implementation.", e);
                println!("cargo:warning=Install with: sudo apt-get install libwayland-dev");
            }
        }
    }
    
    // Try to find wlroots
    if cfg!(feature = "wlroots") {
        match pkg_config::Config::new()
            .atleast_version("0.16.0")
            .probe("wlroots")
        {
            Ok(library) => {
                println!("cargo:rustc-cfg=has_wlroots");
                println!("cargo:warning=Found wlroots version {}", library.version);
            }
            Err(e) => {
                println!("cargo:warning=wlroots not found: {}. Using fallback implementation.", e);
                println!("cargo:warning=Install with: sudo apt-get install libwlroots-dev");
            }
        }
    }
    
    // Check for additional Wayland protocol libraries
    let _ = pkg_config::probe_library("wayland-protocols");
    let _ = pkg_config::probe_library("wayland-scanner");
}
