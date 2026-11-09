use std::process::Command;

fn main() {
    slint_build::compile("ui/main.slint").unwrap();

    #[cfg(target_os = "windows")]
    {
        println!("cargo:rerun-if-changed=assets/resource.rc");
        println!("cargo:rerun-if-changed=assets/tray.ico");
        println!("cargo:rerun-if-changed=assets/app.ico");

        let out_dir = std::env::var("OUT_DIR").unwrap();
        let res_path = format!("{}/resource.o", out_dir);
        let status = Command::new("windres")
            .args(&["-i", "assets/resource.rc", "-O", "coff", "-o", &res_path])
            .status();

        match status {
            Ok(s) if s.success() => {
                println!("cargo:rustc-link-arg={}", res_path);
            }
            Err(e) => {
                eprintln!("Warning: windres failed: {:?}", e);
            }
            _ => {
                eprintln!("Warning: windres returned non-zero exit code");
            }
        }
    }
}
