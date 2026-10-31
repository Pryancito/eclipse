//! SilverSurfer — the Eclipse OS browser.
//!
//! One xdg window on labwc. Servo paints the page; GStreamer plays the
//! media when this binary was built with `media-gstreamer`; lunarfile is
//! the file dialog.

fn main() {
    #[cfg(not(feature = "engine"))]
    {
        eprintln!("silversurfer: este binario se compiló sin el motor Servo");
        std::process::exit(1);
    }
    #[cfg(feature = "engine")]
    if let Err(err) = silversurfer::launch() {
        eprintln!("silversurfer: {err}");
        std::process::exit(1);
    }
}
