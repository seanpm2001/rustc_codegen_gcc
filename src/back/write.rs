use std::{env, fs};

use gccjit::OutputKind;
use rustc_codegen_ssa::{CompiledModule, ModuleCodegen};
use rustc_codegen_ssa::back::link::ensure_removed;
use rustc_codegen_ssa::back::write::{BitcodeSection, CodegenContext, EmitObj, ModuleConfig};
use rustc_errors::Handler;
use rustc_fs_util::link_or_copy;
use rustc_session::config::OutputType;
use rustc_span::fatal_error::FatalError;
use rustc_target::spec::SplitDebuginfo;

use crate::{GccCodegenBackend, GccContext};
use crate::errors::CopyBitcode;

pub(crate) unsafe fn codegen(cgcx: &CodegenContext<GccCodegenBackend>, diag_handler: &Handler, module: ModuleCodegen<GccContext>, config: &ModuleConfig) -> Result<CompiledModule, FatalError> {
    let _timer = cgcx.prof.generic_activity_with_arg("GCC_module_codegen", &*module.name);
    {
        let context = &module.module_llvm.context;

        let module_name = module.name.clone();

        // println!("Module name: {}", module_name);
        // let should_combine_object_files = module_name == "test_rust.3ab6d383-cgu.0"; // With debug info.
        let should_combine_object_files = module_name == "test_rust.8bd2b20a-cgu.0";

        let module_name = Some(&module_name[..]);

        let bc_out = cgcx.output_filenames.temp_path(OutputType::Bitcode, module_name);
        let obj_out = cgcx.output_filenames.temp_path(OutputType::Object, module_name);

        if config.bitcode_needed() {
            let _timer = cgcx
                .prof
                .generic_activity_with_arg("GCC_module_codegen_make_bitcode", &*module.name);

            // TODO
            /*if let Some(bitcode_filename) = bc_out.file_name() {
                cgcx.prof.artifact_size(
                    "llvm_bitcode",
                    bitcode_filename.to_string_lossy(),
                    data.len() as u64,
                );
            }*/

            // println!("{:?}: Emit bc: {}, Emit obj: {:?}", module_name, config.emit_bc, config.emit_obj == EmitObj::Bitcode);

            if config.emit_bc || config.emit_obj == EmitObj::Bitcode {
                let _timer = cgcx
                    .prof
                    .generic_activity_with_arg("GCC_module_codegen_emit_bitcode", &*module.name);
                // println!("Compiling {:?} with bitcode: {:?}", module_name, bc_out);
                // FIXME FIXME FIXME: the undefined symbols might also be caused by not embedding the bitcode since the function is in asm.
                // FIXME: some symbols like _RNvXsV_NtCsgpzv4I1UvGY_4core3fmtRNtNtNtB7_5alloc6layout6LayoutNtB5_5Debug3fmtCscFQYr8hhBzE_9hashbrown are defined in the object file, but not the *ltrans*.o object files.
                // TODO TODO TODO: check if core::panicking::panic_bounds_check (_RNvNtCsgpzv4I1UvGY_4core9panicking18panic_bounds_check) is compiled with bitcode.
                // TODO: so maybe we need ffat-objects here?
                // TODO: use the flag -save-temps on a manual invoke of gcc to link to keep the *.ltrans*.o files and to inspect them.
                context.add_command_line_option("-flto");
                context.add_driver_option("-flto");
                // TODO: use flto-partition=one?
                context.add_command_line_option("-flto-partition=one");
                context.add_driver_option("-flto-partition=one");
                if should_combine_object_files {
                    unimplemented!(); // TODO: remove this line.
                    context.add_driver_option("-Wl,-r");
                    context.add_driver_option("-nostdlib");
                    println!("Output file: {:?}", obj_out);
                    // NOTE: this doesn't actually generate an executable. With the above flags, it combines the .o files together in another .o.
                    context.compile_to_file(OutputKind::Executable, obj_out.to_str().expect("path to str"));
                }
                else {
                    context.compile_to_file(OutputKind::ObjectFile, bc_out.to_str().expect("path to str"));
                }
            }

            if config.emit_obj == EmitObj::ObjectCode(BitcodeSection::Full) {
                // FIXME: lto1 fails with the following error:
                /*
  = note: lto1: internal compiler error: decompressed stream: Destination buffer is too small
          0x11e5a6b lto_uncompression_zstd
          	../../../gcc/gcc/lto-compress.cc:171
          0x11e5a6b lto_end_uncompression(lto_compression_stream*, lto_compression)
          	../../../gcc/gcc/lto-compress.cc:406
          0x11e4052 lto_get_section_data(lto_file_decl_data*, lto_section_type, char const*, int, unsigned long*, bool)
          	../../../gcc/gcc/lto-section-in.cc:168
          0xde7930 lto_file_finalize
          	../../../gcc/gcc/lto/lto-common.cc:2280
          0xde7930 lto_create_files_from_ids
          	../../../gcc/gcc/lto/lto-common.cc:2298
          0xde7930 lto_file_read
          	../../../gcc/gcc/lto/lto-common.cc:2353
          0xde7930 read_cgraph_and_symbols(unsigned int, char const**)
          	../../../gcc/gcc/lto/lto-common.cc:2801
          0xdcccdf lto_main()
          	../../../gcc/gcc/lto/lto.cc:654
          Please submit a full bug report, with preprocessed source (by using -freport-bug).
          Please include the complete backtrace with any bug report.
          See <https://gcc.gnu.org/bugs/> for instructions.
          lto-wrapper: fatal error: cc returned 1 exit status
          compilation terminated.
          /usr/bin/ld: error: lto-wrapper failed
          collect2: error: ld returned 1 exit status
                */
                let _timer = cgcx
                    .prof
                    .generic_activity_with_arg("GCC_module_codegen_embed_bitcode", &*module.name);
                // TODO(antoyo): maybe we should call embed_bitcode to have the proper iOS fixes?
                //embed_bitcode(cgcx, llcx, llmod, &config.bc_cmdline, data);
                if should_combine_object_files {
                    unimplemented!();
                }

                // println!("Compiling {:?} with bitcode and asm: {:?}", module_name, bc_out);
                context.add_command_line_option("-flto");
                context.add_driver_option("-flto");
                // TODO: use flto-partition=one?
                context.add_command_line_option("-flto-partition=one");
                context.add_driver_option("-flto-partition=one");
                context.add_command_line_option("-ffat-lto-objects");
                context.add_driver_option("-ffat-lto-objects");
                // TODO: Send -plugin/usr/lib/gcc/x86_64-pc-linux-gnu/11.1.0/liblto_plugin.so to linker (this should be done when specifying the appropriate rustc cli argument).
                context.compile_to_file(OutputKind::ObjectFile, bc_out.to_str().expect("path to str"));
            }
        }

        if config.emit_ir {
            unimplemented!();
        }

        if config.emit_asm {
            let _timer = cgcx
                .prof
                .generic_activity_with_arg("GCC_module_codegen_emit_asm", &*module.name);
            let path = cgcx.output_filenames.temp_path(OutputType::Assembly, module_name);
            context.compile_to_file(OutputKind::Assembler, path.to_str().expect("path to str"));
        }

        match config.emit_obj {
            EmitObj::ObjectCode(_) => {
                let _timer = cgcx
                    .prof
                    .generic_activity_with_arg("GCC_module_codegen_emit_obj", &*module.name);
                if env::var("CG_GCCJIT_DUMP_MODULE_NAMES").as_deref() == Ok("1") {
                    println!("Module {}", module.name);
                }
                if env::var("CG_GCCJIT_DUMP_ALL_MODULES").as_deref() == Ok("1") || env::var("CG_GCCJIT_DUMP_MODULE").as_deref() == Ok(&module.name) {
                    println!("Dumping reproducer {}", module.name);
                    let _ = fs::create_dir("/tmp/reproducers");
                    // FIXME(antoyo): segfault in dump_reproducer_to_file() might be caused by
                    // transmuting an rvalue to an lvalue.
                    // Segfault is actually in gcc::jit::reproducer::get_identifier_as_lvalue
                    context.dump_reproducer_to_file(&format!("/tmp/reproducers/{}.c", module.name));
                    println!("Dumped reproducer {}", module.name);
                }
                if env::var("CG_GCCJIT_DUMP_TO_FILE").as_deref() == Ok("1") {
                    let _ = fs::create_dir("/tmp/gccjit_dumps");
                    let path = &format!("/tmp/gccjit_dumps/{}.c", module.name);
                    context.set_debug_info(true);
                    context.dump_to_file(path, true);
                }
                if should_combine_object_files {
                    context.add_command_line_option("-flto");
                    context.add_driver_option("-flto");
                    // TODO: use flto-partition=one?
                    context.add_command_line_option("-flto-partition=one");
                    context.add_driver_option("-flto-partition=one");

                    // let inner = context.new_child_context();
                    // context.add_driver_option("-v");
                    context.add_driver_option("-Wl,-r");
                    context.add_driver_option("-nostdlib");
                    context.add_driver_option("-fuse-linker-plugin");

                    println!("Output file: {:?}", obj_out);
                    // NOTE: this doesn't actually generate an executable. With the above flags, it combines the .o files together in another .o.
                    context.compile_to_file(OutputKind::Executable, obj_out.to_str().expect("path to str"));
                    // println!("After");
                }
                else {
                    context.compile_to_file(OutputKind::ObjectFile, obj_out.to_str().expect("path to str"));
                }
            }

            EmitObj::Bitcode => {
                debug!("copying bitcode {:?} to obj {:?}", bc_out, obj_out);
                if let Err(err) = link_or_copy(&bc_out, &obj_out) {
                    diag_handler.emit_err(CopyBitcode { err });
                }

                if !config.emit_bc {
                    //debug!("removing_bitcode {:?}", bc_out);
                    ensure_removed(diag_handler, &bc_out);
                }
            }

            EmitObj::None => {}
        }
    }

    Ok(module.into_compiled_module(
        config.emit_obj != EmitObj::None,
        cgcx.target_can_use_split_dwarf && cgcx.split_debuginfo == SplitDebuginfo::Unpacked,
        config.emit_bc,
        &cgcx.output_filenames,
    ))
}

pub(crate) fn link(_cgcx: &CodegenContext<GccCodegenBackend>, _diag_handler: &Handler, mut _modules: Vec<ModuleCodegen<GccContext>>) -> Result<ModuleCodegen<GccContext>, FatalError> {
    unimplemented!();
}

pub(crate) fn save_temp_bitcode(cgcx: &CodegenContext<GccCodegenBackend>, module: &ModuleCodegen<GccContext>, name: &str) {
    if !cgcx.save_temps {
        return;
    }
    unsafe {
        let ext = format!("{}.bc", name);
        let cgu = Some(&module.name[..]);
        let path = cgcx.output_filenames.temp_path_ext(&ext, cgu);
        unimplemented!();
        /*let cstr = path_to_c_string(&path);
        let llmod = module.module_llvm.llmod();
        llvm::LLVMWriteBitcodeToFile(llmod, cstr.as_ptr());*/
    }
}
