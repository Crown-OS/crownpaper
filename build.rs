use std::path::Path;

use naga::back::spv;
use naga::valid::{Capabilities, ValidationFlags, Validator};

const SHADERS: &[&str] = &["blur_focus"];

fn main() {
    let out_dir = std::env::var("OUT_DIR").expect("cargo always sets OUT_DIR");
    for name in SHADERS {
        let source_path = format!("src/shaders/{name}.wgsl");
        println!("cargo:rerun-if-changed={source_path}");
        let source = std::fs::read_to_string(&source_path).expect("shader source is readable");
        let words = compile_to_spirv(&source).unwrap_or_else(|e| panic!("{source_path}: {e}"));
        let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        std::fs::write(Path::new(&out_dir).join(format!("{name}.spv")), bytes)
            .expect("OUT_DIR is writable");
    }
}

fn compile_to_spirv(source: &str) -> Result<Vec<u32>, String> {
    let module = naga::front::wgsl::parse_str(source).map_err(|e| e.emit_to_string(source))?;
    let info = Validator::new(ValidationFlags::all(), Capabilities::empty())
        .validate(&module)
        .map_err(|e| e.emit_to_string(source))?;
    // Matches wgpu-hal's Vulkan writer: wgpu flips Y through the viewport, so
    // the coordinate space must not be adjusted in the shader as well.
    let options = spv::Options {
        flags: spv::WriterFlags::LABEL_VARYINGS,
        ..Default::default()
    };
    spv::write_vec(&module, &info, &options, None).map_err(|e| e.to_string())
}
