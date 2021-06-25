fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    libc::compile();
}

mod libc {
    use std::path::Path;
    use std::env;

    pub fn compile() {
        let cfg = &mut cc::Build::new();
        cfg.no_default_flags(true);
        cfg.compiler("clang");
        cfg.cpp(false);
        cfg.warnings(false);

        cfg.flag("-nostdlib");
        cfg.flag("-ffreestanding");
        cfg.flag("-fno-PIC");
        cfg.flag("-isystem../include");
        if let Ok(extra_include) = env::var("CLANG_EXTRA_INCLUDE_DIR") {
            cfg.flag(&("-isystem".to_owned() + &extra_include));
        }
        cfg.flag("-fno-stack-protector");
        cfg.flag("--target=armv7-none-eabihf");
        cfg.flag("-O2");

        cfg.flag("-std=c99");
        cfg.flag("-fstrict-aliasing");
        cfg.flag("-funwind-tables");
        cfg.flag("-fvisibility=hidden");
        cfg.flag("-U_FORTIFY_SOURCE");
        cfg.define("_FORTIFY_SOURCE", Some("0"));

        let sources = vec![
            "printf.c"
        ];

        let root = Path::new("./");
        for src in sources {
            println!("cargo:rerun-if-changed={}", src);
            cfg.file(root.join("src").join(src));
        }

        cfg.compile("printf");
    }
}
