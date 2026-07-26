use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let output_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is not set"));
    let icon_path = output_dir.join("codex-image-fix.ico");
    let green_icon_path = output_dir.join("status-green.ico");
    let gray_icon_path = output_dir.join("status-gray.ico");
    let amber_icon_path = output_dir.join("status-amber.ico");
    let red_icon_path = output_dir.join("status-red.ico");
    let source_path = output_dir.join("codex-image-fix.rc");
    let resource_path = output_dir.join("codex-image-fix.res");
    let manifest_path = output_dir.join("codex-image-fix.manifest");
    fs::write(&icon_path, make_icon()).expect("failed to write generated application icon");
    for (path, color) in [
        (&green_icon_path, (36, 172, 106)),
        (&gray_icon_path, (125, 135, 145)),
        (&amber_icon_path, (214, 145, 22)),
        (&red_icon_path, (205, 57, 57)),
    ] {
        fs::write(path, make_status_icon(color)).expect("failed to write generated status icon");
    }
    fs::write(&manifest_path, application_manifest())
        .expect("failed to write application manifest");
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is not set");
    let version_numbers = version_numbers(&version);
    fs::write(
        &source_path,
        format!(
            "1 ICON \"{}\"\r\n2 ICON \"{}\"\r\n3 ICON \"{}\"\r\n4 ICON \"{}\"\r\n5 ICON \"{}\"\r\n1 24 \"{}\"\r\n\r\n1 VERSIONINFO\r\nFILEVERSION {}\r\nPRODUCTVERSION {}\r\nFILEFLAGSMASK 0x3fL\r\nFILEFLAGS 0x0L\r\nFILEOS 0x40004L\r\nFILETYPE 0x1L\r\nFILESUBTYPE 0x0L\r\nBEGIN\r\n  BLOCK \"StringFileInfo\"\r\n  BEGIN\r\n    BLOCK \"040904B0\"\r\n    BEGIN\r\n      VALUE \"CompanyName\", \"comidea.org\\0\"\r\n      VALUE \"FileDescription\", \"Comidea Codex Image Bridge\\0\"\r\n      VALUE \"FileVersion\", \"{}\\0\"\r\n      VALUE \"InternalName\", \"CodexImageFix\\0\"\r\n      VALUE \"OriginalFilename\", \"CodexImageFix.exe\\0\"\r\n      VALUE \"ProductName\", \"Comidea Codex Image Bridge\\0\"\r\n      VALUE \"ProductVersion\", \"{}\\0\"\r\n      VALUE \"LegalCopyright\", \"comidea.org\\0\"\r\n    END\r\n  END\r\n  BLOCK \"VarFileInfo\"\r\n  BEGIN\r\n    VALUE \"Translation\", 0x0409, 1200\r\n  END\r\nEND\r\n",
            resource_path_string(&icon_path),
            resource_path_string(&green_icon_path),
            resource_path_string(&gray_icon_path),
            resource_path_string(&amber_icon_path),
            resource_path_string(&red_icon_path),
            resource_path_string(&manifest_path),
            version_numbers,
            version_numbers,
            version,
            version,
        ),
    )
    .expect("failed to write Windows resource source");

    let compiler = find_resource_compiler();
    let output = Command::new(&compiler)
        .arg("/nologo")
        .arg(format!("/fo{}", resource_path.display()))
        .arg(&source_path)
        .output()
        .unwrap_or_else(|error| panic!("failed to start {}: {error}", compiler.display()));
    if !output.status.success() {
        panic!(
            "Windows resource compiler failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    println!(
        "cargo:rustc-link-arg-bin=CodexImageFix={}",
        resource_path.display()
    );
}

fn resource_path_string(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn version_numbers(version: &str) -> String {
    let mut parts = version
        .split('.')
        .map(|part| {
            part.split('-')
                .next()
                .unwrap_or_default()
                .parse::<u16>()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    parts.resize(4, 0);
    format!("{},{},{},{}", parts[0], parts[1], parts[2], parts[3])
}

fn application_manifest() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="1.0.0.0" processorArchitecture="*" name="comidea.org.CodexImageFix" type="win32"/>
  <description>Comidea Codex Image Bridge</description>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2,PerMonitor</dpiAwareness>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
      <activeCodePage xmlns="http://schemas.microsoft.com/SMI/2019/WindowsSettings">UTF-8</activeCodePage>
    </windowsSettings>
  </application>
</assembly>
"#
}

fn find_resource_compiler() -> PathBuf {
    if let Some(path) = env::var_os("RC") {
        return PathBuf::from(path);
    }
    let architecture = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86") => "x86",
        _ => "x64",
    };
    if let Some(program_files) = env::var_os("ProgramFiles(x86)") {
        let bin_root = PathBuf::from(program_files).join("Windows Kits/10/bin");
        let mut versions = fs::read_dir(&bin_root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        versions.sort();
        for version in versions.into_iter().rev() {
            let candidate = version.join(architecture).join("rc.exe");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    Path::new("rc.exe").to_owned()
}

fn make_icon() -> Vec<u8> {
    let sizes = [16u32, 32, 64, 128];
    let images = sizes.map(make_icon_image);
    let directory_size = 6 + sizes.len() * 16;
    let mut icon = Vec::with_capacity(directory_size + images.iter().map(Vec::len).sum::<usize>());
    push_u16(&mut icon, 0);
    push_u16(&mut icon, 1);
    push_u16(&mut icon, sizes.len() as u16);

    let mut offset = directory_size as u32;
    for (size, image) in sizes.into_iter().zip(images.iter()) {
        icon.push(size as u8);
        icon.push(size as u8);
        icon.push(0);
        icon.push(0);
        push_u16(&mut icon, 1);
        push_u16(&mut icon, 32);
        push_u32(&mut icon, image.len() as u32);
        push_u32(&mut icon, offset);
        offset += image.len() as u32;
    }
    for image in images {
        icon.extend(image);
    }
    icon
}

fn make_icon_image(size: u32) -> Vec<u8> {
    let pixel_bytes = size * size * 4;
    let mask_stride = size.div_ceil(32) * 4;
    let mask_bytes = mask_stride * size;
    let mut image = Vec::with_capacity((40 + pixel_bytes + mask_bytes) as usize);
    push_u32(&mut image, 40);
    push_i32(&mut image, size as i32);
    push_i32(&mut image, (size * 2) as i32);
    push_u16(&mut image, 1);
    push_u16(&mut image, 32);
    push_u32(&mut image, 0);
    push_u32(&mut image, pixel_bytes);
    push_i32(&mut image, 0);
    push_i32(&mut image, 0);
    push_u32(&mut image, 0);
    push_u32(&mut image, 0);

    let mut alpha = vec![0u8; (size * size) as usize];
    for y in (0..size).rev() {
        for x in 0..size {
            let (red, green, blue, pixel_alpha) = icon_pixel(size, x, y);
            image.extend([blue, green, red, pixel_alpha]);
            alpha[(y * size + x) as usize] = pixel_alpha;
        }
    }
    for y in (0..size).rev() {
        let mut row = vec![0u8; mask_stride as usize];
        for x in 0..size {
            if alpha[(y * size + x) as usize] < 128 {
                row[(x / 8) as usize] |= 1 << (7 - (x % 8));
            }
        }
        image.extend(row);
    }
    image
}

fn make_status_icon(color: (u8, u8, u8)) -> Vec<u8> {
    let sizes = [16u32, 32];
    let images = sizes.map(|size| make_status_icon_image(size, color));
    let directory_size = 6 + sizes.len() * 16;
    let mut icon = Vec::with_capacity(directory_size + images.iter().map(Vec::len).sum::<usize>());
    push_u16(&mut icon, 0);
    push_u16(&mut icon, 1);
    push_u16(&mut icon, sizes.len() as u16);
    let mut offset = directory_size as u32;
    for (size, image) in sizes.into_iter().zip(images.iter()) {
        icon.push(size as u8);
        icon.push(size as u8);
        icon.extend([0, 0]);
        push_u16(&mut icon, 1);
        push_u16(&mut icon, 32);
        push_u32(&mut icon, image.len() as u32);
        push_u32(&mut icon, offset);
        offset += image.len() as u32;
    }
    for image in images {
        icon.extend(image);
    }
    icon
}

fn make_status_icon_image(size: u32, color: (u8, u8, u8)) -> Vec<u8> {
    let pixel_bytes = size * size * 4;
    let mask_stride = size.div_ceil(32) * 4;
    let mut image = Vec::with_capacity((40 + pixel_bytes + mask_stride * size) as usize);
    push_u32(&mut image, 40);
    push_i32(&mut image, size as i32);
    push_i32(&mut image, (size * 2) as i32);
    push_u16(&mut image, 1);
    push_u16(&mut image, 32);
    push_u32(&mut image, 0);
    push_u32(&mut image, pixel_bytes);
    push_i32(&mut image, 0);
    push_i32(&mut image, 0);
    push_u32(&mut image, 0);
    push_u32(&mut image, 0);

    let mut alpha = vec![0u8; (size * size) as usize];
    for y in (0..size).rev() {
        for x in 0..size {
            let (red, green, blue, pixel_alpha) = status_pixel(size, x, y, color);
            image.extend([blue, green, red, pixel_alpha]);
            alpha[(y * size + x) as usize] = pixel_alpha;
        }
    }
    for y in (0..size).rev() {
        let mut row = vec![0u8; mask_stride as usize];
        for x in 0..size {
            if alpha[(y * size + x) as usize] < 128 {
                row[(x / 8) as usize] |= 1 << (7 - (x % 8));
            }
        }
        image.extend(row);
    }
    image
}

fn status_pixel(size: u32, x: u32, y: u32, color: (u8, u8, u8)) -> (u8, u8, u8, u8) {
    const SAMPLES: u32 = 4;
    let mut inside = 0u32;
    let mut border = 0u32;
    for sample_y in 0..SAMPLES {
        for sample_x in 0..SAMPLES {
            let px = x as f32 + (sample_x as f32 + 0.5) / SAMPLES as f32;
            let py = y as f32 + (sample_y as f32 + 0.5) / SAMPLES as f32;
            let dx = px / size as f32 - 0.5;
            let dy = py / size as f32 - 0.5;
            let distance = (dx * dx + dy * dy).sqrt();
            if distance <= 0.41 {
                inside += 1;
                if distance >= 0.35 {
                    border += 1;
                }
            }
        }
    }
    if inside == 0 {
        return (0, 0, 0, 0);
    }
    let body = inside - border;
    let red = (u32::from(color.0) * body + 255 * border) / inside;
    let green = (u32::from(color.1) * body + 255 * border) / inside;
    let blue = (u32::from(color.2) * body + 255 * border) / inside;
    let alpha = inside * 255 / (SAMPLES * SAMPLES);
    (red as u8, green as u8, blue as u8, alpha as u8)
}

fn icon_pixel(size: u32, x: u32, y: u32) -> (u8, u8, u8, u8) {
    const SAMPLES: u32 = 4;
    let mut background = 0u32;
    let mut letter = 0u32;
    let mut accent = 0u32;
    for sample_y in 0..SAMPLES {
        for sample_x in 0..SAMPLES {
            let normalized_x = (x as f32 + (sample_x as f32 + 0.5) / SAMPLES as f32) / size as f32;
            let normalized_y = (y as f32 + (sample_y as f32 + 0.5) / SAMPLES as f32) / size as f32;
            if !inside_rounded_square(normalized_x, normalized_y) {
                continue;
            }
            background += 1;
            let dx = normalized_x - 0.47;
            let dy = normalized_y - 0.50;
            let distance = (dx * dx + dy * dy).sqrt();
            let in_letter =
                (0.17..=0.31).contains(&distance) && !(normalized_x > 0.47 && dy.abs() < 0.115);
            if in_letter {
                letter += 1;
            } else if (0.70..=0.84).contains(&normalized_x) && (0.44..=0.56).contains(&normalized_y)
            {
                accent += 1;
            }
        }
    }
    if background == 0 {
        return (0, 0, 0, 0);
    }
    let base = (18u32, 110u32, 135u32);
    let white = (255u32, 255u32, 255u32);
    let mint = (151u32, 226u32, 198u32);
    let base_samples = background - letter - accent;
    let red = (base.0 * base_samples + white.0 * letter + mint.0 * accent) / background;
    let green = (base.1 * base_samples + white.1 * letter + mint.1 * accent) / background;
    let blue = (base.2 * base_samples + white.2 * letter + mint.2 * accent) / background;
    let alpha = background * 255 / (SAMPLES * SAMPLES);
    (red as u8, green as u8, blue as u8, alpha as u8)
}

fn inside_rounded_square(x: f32, y: f32) -> bool {
    let nearest_x = x.clamp(0.22, 0.78);
    let nearest_y = y.clamp(0.22, 0.78);
    let dx = x - nearest_x;
    let dy = y - nearest_y;
    (0.06..=0.94).contains(&x) && (0.06..=0.94).contains(&y) && dx * dx + dy * dy <= 0.16 * 0.16
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend(value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend(value.to_le_bytes());
}

fn push_i32(bytes: &mut Vec<u8>, value: i32) {
    bytes.extend(value.to_le_bytes());
}
