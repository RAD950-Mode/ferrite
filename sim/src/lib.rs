//! The simulator as a library: the radio's front panel ([`face`]), a fake radio ([`fake`]) and the interactive
//! simulator without a window ([`simcore`]). The `ferrite-sim` binary (PNGs and the desktop window) and the
//! `sim-web` page (wasm) are built on it.

pub mod face;
pub mod fake;
pub mod fakegps;
pub mod parts;
pub mod simcore;
pub mod sol950_wf;

/// An RGB image (0xRRGGBB per pixel) as a PNG.
#[cfg(not(target_arch = "wasm32"))]
pub fn save_rgb(px: &[u32], w: usize, h: usize, path: &std::path::Path) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    let out: Vec<u8> = px.iter().flat_map(|&c| [(c >> 16) as u8, (c >> 8) as u8, c as u8]).collect();
    let mut enc = png::Encoder::new(std::fs::File::create(path).unwrap(), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.write_header().unwrap().write_image_data(&out).unwrap();
    println!("{}", path.display());
}
