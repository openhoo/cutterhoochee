fn main() {
    // WebKit's DMA-BUF renderer can produce a blank window on NVIDIA/Xwayland.
    // Select its compatible renderer before GTK starts; preserve explicit overrides.
    #[cfg(target_os = "linux")]
    if std::path::Path::new("/sys/module/nvidia").exists()
        && std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none()
    {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    cutterhoochee_lib::run();
}
