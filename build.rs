fn main() {
    slint_build::compile_with_config("ui/main.slint",
        slint_build::CompilerConfiguration::new().with_style("cupertino-dark".into()).with_debug_info(true))
        .expect("compile Simple Modern Equalizer interface");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        embed_windows_icon();
    }
}

fn embed_windows_icon() {
    const ICON: &str = "ui/icons/app-icon.ico";
    println!("cargo:rerun-if-changed={ICON}");
    let ico = std::fs::read(ICON).expect("read application icon");
    let count = u16::from_le_bytes(ico[4..6].try_into().unwrap());
    let mut group = ico[..6].to_vec();
    let mut resources = Vec::new();
    // MSVC accepts a .res file directly, so embedding needs no separate resource compiler.
    write_resource(&mut resources, 0, 0, 0, &[]);
    for index in 0..count {
        let offset = 6 + usize::from(index) * 16;
        let entry = &ico[offset..offset + 16];
        let size = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
        let start = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
        let id = index + 1;
        group.extend_from_slice(&entry[..12]);
        group.extend_from_slice(&id.to_le_bytes());
        write_resource(&mut resources, 3, id, 0x0409, &ico[start..start + size]);
    }
    write_resource(&mut resources, 14, 1, 0x0409, &group);
    let path = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("app-icon.res");
    std::fs::write(&path, resources).expect("write Windows icon resource");
    println!("cargo:rustc-link-arg-bin=simpledarkeq={}", path.display());
}

fn write_resource(output: &mut Vec<u8>, kind: u16, id: u16, language: u16, data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_le_bytes());
    output.extend_from_slice(&32_u32.to_le_bytes());
    for word in [0xffff, kind, 0xffff, id] {
        output.extend_from_slice(&word.to_le_bytes());
    }
    output.extend_from_slice(&0_u32.to_le_bytes());
    let flags: u16 = if kind == 0 { 0 } else { 0x1030 };
    output.extend_from_slice(&flags.to_le_bytes());
    output.extend_from_slice(&language.to_le_bytes());
    output.extend_from_slice(&[0; 8]);
    output.extend_from_slice(data);
    output.resize(output.len().next_multiple_of(4), 0);
}
