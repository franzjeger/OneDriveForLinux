//! Dev tool: renders every tray icon state to PNG for visual inspection.
//! Usage: cargo run -p tray --example render_icons [out_dir]

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "icon-preview".into());
    std::fs::create_dir_all(&out).expect("create output dir");
    tray::render_icon_previews(std::path::Path::new(&out)).expect("render previews");
    println!("Icons written to {out}/");
}
