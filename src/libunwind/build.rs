fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    llvm_libunwind::compile_cpp();
    llvm_libunwind::compile_c();
}

mod llvm_libunwind {
    use std::path::Path;
    use std::env;

    fn setup_options(cfg: &mut cc::Build) {
        cfg.no_default_flags(true);
        cfg.warnings(false);

        cfg.flag("-nostdlib");
        cfg.flag("-ffreestanding");
        cfg.flag("-fno-PIC");
        cfg.flag("-Isrc");
        cfg.flag("-isystem../include");
        if let Ok(extra_include) = env::var("CLANG_EXTRA_INCLUDE_DIR") {
            cfg.flag(&("-isystem".to_owned() + &extra_include));
        }
        cfg.flag("-fno-stack-protector");
        cfg.flag("--target=armv7-none-eabihf");
        cfg.flag("-O2");

        cfg.flag("-std=c99");
        cfg.flag("-fstrict-aliasing");
        cfg.flag("-fvisibility=hidden");
        cfg.flag_if_supported("-fvisibility-global-new-delete-hidden");
        cfg.define("_LIBUNWIND_IS_BAREMETAL", Some("1"));
        cfg.define("_LIBUNWIND_NO_HEAP", Some("1"));
        cfg.define("_LIBUNWIND_HAS_NO_THREADS", Some("1"));
        cfg.define("NDEBUG", Some("1"));
        // libunwind expects a __LITTLE_ENDIAN__ macro to be set for LE archs, cf. #65765
        cfg.define("__LITTLE_ENDIAN__", Some("1"));
        cfg.define("_LIBUNWIND_DISABLE_VISIBILITY_ANNOTATIONS", None);
        cfg.flag("-U_FORTIFY_SOURCE");
        cfg.define("_FORTIFY_SOURCE", Some("0"));
    }

    pub fn compile_c() {
        let cfg = &mut cc::Build::new();
        setup_options(cfg);
        cfg.compiler("clang");
        cfg.flag("-funwind-tables");

        let unwind_sources = vec![
            "Unwind-sjlj.c",
            "UnwindLevel1-gcc-ext.c",
            "UnwindLevel1.c",
            "UnwindRegistersRestore.S",
            "UnwindRegistersSave.S",
        ];

        let root = Path::new("../llvm_libunwind");
        cfg.include(root.join("include"));
        for src in unwind_sources {
            println!("cargo:rerun-if-changed={}", src);
            cfg.file(root.join("src").join(src));
        }

        cfg.compile("unwind_c");
    }

    /// Compile the libunwind C/C++ source code.
    pub fn compile_cpp() {
        let cfg = &mut cc::Build::new();
        setup_options(cfg);
        cfg.compiler("clang++");
        cfg.cpp(true);
        cfg.cpp_set_stdlib(None);

        // c++ options
        cfg.flag("-std=c++11");
        cfg.flag("-nostdinc++");
        cfg.flag("-fno-exceptions");
        cfg.flag("-fno-rtti");
        cfg.flag("-fstrict-aliasing");
        cfg.flag("-funwind-tables");
        cfg.flag("-fvisibility=hidden");
        cfg.flag_if_supported("-fvisibility-global-new-delete-hidden");

        let unwind_sources = vec![
            "Unwind-EHABI.cpp",
            "Unwind-seh.cpp",
            "libunwind.cpp"
        ];

        let root = Path::new("../llvm_libunwind");
        cfg.include(root.join("include"));
        for src in unwind_sources {
            println!("cargo:rerun-if-changed={}", src);
            cfg.file(root.join("src").join(src));
        }

        cfg.compile("unwind_cpp");
    }
}
