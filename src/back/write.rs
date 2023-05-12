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

        println!("Module name: {}", module_name);
        let should_combine_object_files = module_name == "test_rust.3ab6d383-cgu.0";

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
                let lto_context = context.new_child_context();
                println!("Compiling {:?} with bitcode: {:?}", module_name, bc_out);
                lto_context.add_command_line_option("-flto");
                // TODO: seems like this might not be needed here.
                if should_combine_object_files {
                    unimplemented!();
                    lto_context.add_driver_option("-Wl,-r");
                    lto_context.compile_to_file(OutputKind::Executable, bc_out.to_str().expect("path to str"));
                }
                else {
                    lto_context.compile_to_file(OutputKind::ObjectFile, bc_out.to_str().expect("path to str"));
                }
            }

            if config.emit_obj == EmitObj::ObjectCode(BitcodeSection::Full) {
                let _timer = cgcx
                    .prof
                    .generic_activity_with_arg("GCC_module_codegen_embed_bitcode", &*module.name);
                // TODO: maybe we should call embed_bitcode to have the proper iOS fixes?
                //embed_bitcode(cgcx, llcx, llmod, &config.bc_cmdline, data);
                if should_combine_object_files {
                    // TODO: properly implement should_combine_object_files to see if this fixes the "undefined reference" error.
                    unimplemented!();
                }

                println!("Compiling {:?} with bitcode and asm: {:?}", module_name, bc_out);
                let lto_context = context.new_child_context();
                // FIXME: it seems to use the LTO from the distro instead of the one I compiled.
                lto_context.add_command_line_option("-flto");
                lto_context.add_command_line_option("-ffat-lto-objects");
                lto_context.compile_to_file(OutputKind::ObjectFile, bc_out.to_str().expect("path to str"));
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
                    let lto_context = &context;
                    lto_context.add_command_line_option("-flto");
                    lto_context.add_driver_option("-Wl,-r");
                    lto_context.add_driver_option("-nostdlib");
                    println!("Output file: {:?}", obj_out);
                    // NOTE: this doesn't actually generate an executable. With the above flags, it combines the .o files together in another .o.
                    lto_context.compile_to_file(OutputKind::Executable, obj_out.to_str().expect("path to str"));
                }
                else {
                    context.compile_to_file(OutputKind::ObjectFile, obj_out.to_str().expect("path to str"));
                }
            }

            EmitObj::Bitcode => {
                //debug!("copying bitcode {:?} to obj {:?}", bc_out, obj_out);
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
