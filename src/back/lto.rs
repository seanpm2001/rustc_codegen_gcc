// FIXME: if any of object files in incremental link cannot be used for link-time optimization, the linker plugin issues a warning and uses nolto-rel.
// => Maybe it's the symbol file?
// => There's a least the rust_alloc file.
//
// TODO: vérifier si les symbols sont dans les fichiers .ltrans0.ltrans.o.
// Ils ne sont pas là.
//
// FIXME: compile the object files (including the ones from sysroot) directly to the executable to see if this works.
// TODO TODO TODO: WE'RE MAKING PROGRESS HERE!
// Compiling with gcc and removing the -r flag compiles correctly!
// For now, adding these environment variables is needed:
// COLLECT_GCC=gcc GCC_EXEC_PREFIX=/opt/gcc/lib/gcc/x86_64-pc-linux-gnu
// I had to copy lto1, crtbegin.o from the system to /opt/…
// I had to copy liblto_plugin.so to /opt/gcc/lib/gcc
// I compile a .o as an executable.
//
// TODO: you do the command with -v -save-temps and then extract the lto1 line from the output and run that under the debugger

use std::ffi::CString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use gccjit::OutputKind;
use object::read::archive::ArchiveFile;
use rustc_codegen_ssa::back::lto::{LtoModuleCodegen, SerializedModule};
use rustc_codegen_ssa::back::symbol_export;
use rustc_codegen_ssa::back::write::{CodegenContext, FatLTOInput};
use rustc_codegen_ssa::traits::*;
use rustc_codegen_ssa::{looks_like_rust_object_file, ModuleCodegen, ModuleKind};
use rustc_data_structures::memmap::Mmap;
use rustc_errors::{FatalError, Handler};
use rustc_hir::def_id::LOCAL_CRATE;
use rustc_middle::dep_graph::WorkProduct;
use rustc_middle::middle::exported_symbols::{SymbolExportInfo, SymbolExportLevel};
use rustc_session::config::{CrateType, Lto};
use tempfile::{TempDir, tempdir};

use crate::back::write::save_temp_bitcode;
use crate::errors::{
    DynamicLinkingWithLTO, LoadBitcode, LtoBitcodeFromRlib, LtoDisallowed, LtoDylib,
};
use crate::{GccCodegenBackend, GccContext};

/// We keep track of the computed LTO cache keys from the previous
/// session to determine which CGUs we can reuse.
pub const THIN_LTO_KEYS_INCR_COMP_FILE_NAME: &str = "thin-lto-past-keys.bin";

pub fn crate_type_allows_lto(crate_type: CrateType) -> bool {
    match crate_type {
        CrateType::Executable | CrateType::Dylib | CrateType::Staticlib | CrateType::Cdylib => true,
        CrateType::Rlib | CrateType::ProcMacro => false,
    }
}

struct LtoData {
    symbols_below_threshold: Vec<CString>,
    upstream_modules: Vec<(SerializedModule<ModuleBuffer>, CString)>,
    tmp_path: TempDir,
}

fn prepare_lto(cgcx: &CodegenContext<GccCodegenBackend>, diag_handler: &Handler) -> Result<LtoData, FatalError> {
    let export_threshold = match cgcx.lto {
        // We're just doing LTO for our one crate
        Lto::ThinLocal => SymbolExportLevel::Rust,

        // We're doing LTO for the entire crate graph
        Lto::Fat | Lto::Thin => symbol_export::crates_export_threshold(&cgcx.crate_types),

        Lto::No => panic!("didn't request LTO but we're doing LTO"),
    };

    // TODO: handle error.
    let tmp_path = tempdir().expect("temp dir");

    let symbol_filter = &|&(ref name, info): &(String, SymbolExportInfo)| {
        if info.level.is_below_threshold(export_threshold) || info.used {
            Some(CString::new(name.as_str()).unwrap())
        } else {
            None
        }
    };
    let exported_symbols = cgcx.exported_symbols.as_ref().expect("needs exported symbols for LTO");
    let mut symbols_below_threshold = {
        let _timer = cgcx.prof.generic_activity("LLVM_lto_generate_symbols_below_threshold");
        exported_symbols[&LOCAL_CRATE].iter().filter_map(symbol_filter).collect::<Vec<CString>>()
    };
    info!("{} symbols to preserve in this crate", symbols_below_threshold.len());

    // If we're performing LTO for the entire crate graph, then for each of our
    // upstream dependencies, find the corresponding rlib and load the bitcode
    // from the archive.
    //
    // We save off all the bytecode and LLVM module ids for later processing
    // with either fat or thin LTO
    let mut upstream_modules = Vec::new();
    if cgcx.lto != Lto::ThinLocal {
        // Make sure we actually can run LTO
        for crate_type in cgcx.crate_types.iter() {
            if !crate_type_allows_lto(*crate_type) {
                diag_handler.emit_err(LtoDisallowed);
                return Err(FatalError);
            } else if *crate_type == CrateType::Dylib {
                if !cgcx.opts.unstable_opts.dylib_lto {
                    diag_handler.emit_err(LtoDylib);
                    return Err(FatalError);
                }
            }
        }

        if cgcx.opts.cg.prefer_dynamic && !cgcx.opts.unstable_opts.dylib_lto {
            diag_handler.emit_err(DynamicLinkingWithLTO);
            return Err(FatalError);
        }

        for &(cnum, ref path) in cgcx.each_linked_rlib_for_lto.iter() {
            let exported_symbols =
                cgcx.exported_symbols.as_ref().expect("needs exported symbols for LTO");
            {
                let _timer =
                    cgcx.prof.generic_activity("LLVM_lto_generate_symbols_below_threshold");
                symbols_below_threshold
                    .extend(exported_symbols[&cnum].iter().filter_map(symbol_filter));
            }

            let archive_data = unsafe {
                Mmap::map(File::open(&path).expect("couldn't open rlib"))
                    .expect("couldn't map rlib")
            };
            let archive = ArchiveFile::parse(&*archive_data).expect("wanted an rlib");
            let obj_files = archive
                .members()
                .filter_map(|child| {
                    child.ok().and_then(|c| {
                        std::str::from_utf8(c.name()).ok().map(|name| (name.trim(), c))
                    })
                })
                .filter(|&(name, _)| looks_like_rust_object_file(name));
            for (name, child) in obj_files {
                info!("adding bitcode from {}", name);
                let path = tmp_path.path().join(name);
                match save_as_file(child.data(&*archive_data).expect("corrupt rlib"), &path) {
                    Ok(()) => {
                        let buffer = ModuleBuffer::new(path);
                        let module = SerializedModule::Local(buffer);
                        upstream_modules.push((module, CString::new(name).unwrap()));
                    }
                    Err(e) => {
                        diag_handler.emit_err(e);
                        return Err(FatalError);
                    }
                }
            }
        }
    }

    // __llvm_profile_counter_bias is pulled in at link time by an undefined reference to
    // __llvm_profile_runtime, therefore we won't know until link time if this symbol
    // should have default visibility.
    symbols_below_threshold.push(CString::new("__llvm_profile_counter_bias").unwrap());
    Ok(LtoData {
        symbols_below_threshold,
        upstream_modules,
        tmp_path,
    })
}

fn save_as_file(obj: &[u8], path: &Path) -> Result<(), LtoBitcodeFromRlib> {
    // TODO: handle error.
    fs::write(path, obj).expect("write object file to temp dir");
    Ok(())
}

/// Performs fat LTO by merging all modules into a single one and returning it
/// for further optimization.
pub(crate) fn run_fat(
    cgcx: &CodegenContext<GccCodegenBackend>,
    modules: Vec<FatLTOInput<GccCodegenBackend>>,
    cached_modules: Vec<(SerializedModule<ModuleBuffer>, WorkProduct)>,
) -> Result<LtoModuleCodegen<GccCodegenBackend>, FatalError> {
    let diag_handler = cgcx.create_diag_handler();
    let lto_data = prepare_lto(cgcx, &diag_handler)?;
    let symbols_below_threshold =
        lto_data.symbols_below_threshold.iter().map(|c| c.as_ptr()).collect::<Vec<_>>();
    fat_lto(cgcx, &diag_handler, modules, cached_modules, lto_data.upstream_modules, lto_data.tmp_path,
        //&symbols_below_threshold,
    )
}

fn fat_lto(cgcx: &CodegenContext<GccCodegenBackend>, diag_handler: &Handler, modules: Vec<FatLTOInput<GccCodegenBackend>>, cached_modules: Vec<(SerializedModule<ModuleBuffer>, WorkProduct)>, mut serialized_modules: Vec<(SerializedModule<ModuleBuffer>, CString)>, tmp_path: TempDir,
    //symbols_below_threshold: &[*const libc::c_char],
) -> Result<LtoModuleCodegen<GccCodegenBackend>, FatalError> {
    let _timer = cgcx.prof.generic_activity("LLVM_fat_lto_build_monolithic_module");
    info!("going for a fat lto");

    // Sort out all our lists of incoming modules into two lists.
    //
    // * `serialized_modules` (also and argument to this function) contains all
    //   modules that are serialized in-memory.
    // * `in_memory` contains modules which are already parsed and in-memory,
    //   such as from multi-CGU builds.
    //
    // All of `cached_modules` (cached from previous incremental builds) can
    // immediately go onto the `serialized_modules` modules list and then we can
    // split the `modules` array into these two lists.
    let mut in_memory = Vec::new();
    serialized_modules.extend(cached_modules.into_iter().map(|(buffer, wp)| {
        info!("pushing cached module {:?}", wp.cgu_name);
        (buffer, CString::new(wp.cgu_name).unwrap())
    }));
    for module in modules {
        match module {
            FatLTOInput::InMemory(m) => in_memory.push(m),
            FatLTOInput::Serialized { name, buffer } => {
                info!("pushing serialized module {:?}", name);
                let buffer = SerializedModule::Local(buffer);
                serialized_modules.push((buffer, CString::new(name).unwrap()));
            }
        }
    }

    // Find the "costliest" module and merge everything into that codegen unit.
    // All the other modules will be serialized and reparsed into the new
    // context, so this hopefully avoids serializing and parsing the largest
    // codegen unit.
    //
    // Additionally use a regular module as the base here to ensure that various
    // file copy operations in the backend work correctly. The only other kind
    // of module here should be an allocator one, and if your crate is smaller
    // than the allocator module then the size doesn't really matter anyway.
    let costliest_module = in_memory
        .iter()
        .enumerate()
        .filter(|&(_, module)| module.kind == ModuleKind::Regular)
        .map(|(i, module)| {
            //let cost = unsafe { llvm::LLVMRustModuleCost(module.module_llvm.llmod()) };
            // TODO
            (0, i)
        })
        .max();

    // If we found a costliest module, we're good to go. Otherwise all our
    // inputs were serialized which could happen in the case, for example, that
    // all our inputs were incrementally reread from the cache and we're just
    // re-executing the LTO passes. If that's the case deserialize the first
    // module and create a linker with it.
    let module: ModuleCodegen<GccContext> = match costliest_module {
        Some((_cost, i)) => in_memory.remove(i),
        None => {
            unimplemented!("Incremental");
            /*assert!(!serialized_modules.is_empty(), "must have at least one serialized module");
            let (buffer, name) = serialized_modules.remove(0);
            info!("no in-memory regular modules to choose from, parsing {:?}", name);
            ModuleCodegen {
                module_llvm: GccContext::parse(cgcx, &name, buffer.data(), diag_handler)?,
                name: name.into_string().unwrap(),
                kind: ModuleKind::Regular,
            }*/
        }
    };
    let mut serialized_bitcode = Vec::new();
    {
        let context = &module.module_llvm.context;
        println!("Base module: {}", module.name);
        info!("using {:?} as a base module", module.name);

        // For all other modules we codegened we'll need to link them into our own
        // bitcode. All modules were codegened in their own LLVM context, however,
        // and we want to move everything to the same LLVM context. Currently the
        // way we know of to do that is to serialize them to a string and them parse
        // them later. Not great but hey, that's why it's "fat" LTO, right?
        for module in in_memory {
            // TODO: add the proper file extension here?
            let path = tmp_path.path().to_path_buf().join(&module.name);
            let path = path.to_str().expect("path");
            module.module_llvm.context.compile_to_file(OutputKind::ObjectFile, path);
            let buffer = ModuleBuffer::new(PathBuf::from(path));
            let llmod_id = CString::new(&module.name[..]).unwrap();
            serialized_modules.push((SerializedModule::Local(buffer), llmod_id));
        }
        // Sort the modules to ensure we produce deterministic results.
        serialized_modules.sort_by(|module1, module2| module1.1.cmp(&module2.1));

        // For all serialized bitcode files we parse them and link them in as we did
        // above. Like above, though, we don't know much about the memory management
        // here so we err on the side of being save and persist everything with
        // the original module.
        //let mut linker = Linker::new(context);
        for (bc_decoded, name) in serialized_modules {
            let _timer = cgcx
                .prof
                .generic_activity_with_arg_recorder("LLVM_fat_lto_link_module", |recorder| {
                    recorder.record_arg(format!("{:?}", name))
                });
            info!("linking {:?}", name);
            match bc_decoded {
                SerializedModule::Local(ref module_buffer) => {
                    module.module_llvm.context.add_driver_option(module_buffer.0.to_str().expect("path"));
                },
                SerializedModule::FromRlib(_) => unimplemented!("from rlib"),
                SerializedModule::FromUncompressedFile(_) => unimplemented!("from uncompressed file"),
            }
            // let data = bc_decoded.data();
            /*linker
                .add(data)
                .map_err(|()| write::llvm_err(diag_handler, LoadBitcode { name }))?;*/
            serialized_bitcode.push(bc_decoded);
        }
        save_temp_bitcode(cgcx, &module, "lto.input");

        // Internalize everything below threshold to help strip out more modules and such.
        unsafe {
            //let ptr = symbols_below_threshold.as_ptr();
            /*llvm::LLVMRustRunRestrictionPass(
                llmod,
                ptr as *const *const libc::c_char,
                symbols_below_threshold.len() as libc::size_t,
            );*/
            save_temp_bitcode(cgcx, &module, "lto.after-restriction");
        }
    }

    // TODO: delete the directory at the correct place instead of not deleting it.
    std::mem::forget(tmp_path);

    Ok(LtoModuleCodegen::Fat { module, _serialized_bitcode: serialized_bitcode })
}

/*pub(crate) struct Linker<'a>(&'a mut llvm::Linker<'a>);

impl<'a> Linker<'a> {
    pub(crate) fn new(llmod: &'a llvm::Module) -> Self {
        unsafe { Linker(llvm::LLVMRustLinkerNew(llmod)) }
    }

    pub(crate) fn add(&mut self, bytecode: &[u8]) -> Result<(), ()> {
        unsafe {
            if llvm::LLVMRustLinkerAdd(
                self.0,
                bytecode.as_ptr() as *const libc::c_char,
                bytecode.len(),
            ) {
                Ok(())
            } else {
                Err(())
            }
        }
    }
}

impl Drop for Linker<'_> {
    fn drop(&mut self) {
        unsafe {
            llvm::LLVMRustLinkerFree(&mut *(self.0 as *mut _));
        }
    }
}*/

pub struct ModuleBuffer(PathBuf);

unsafe impl Send for ModuleBuffer {}
unsafe impl Sync for ModuleBuffer {}

impl ModuleBuffer {
    pub fn new(path: PathBuf) -> ModuleBuffer {
        ModuleBuffer(path)
    }
}

impl ModuleBufferMethods for ModuleBuffer {
    fn data(&self) -> &[u8] {
        unimplemented!("data not needed for GCC codegen");
        /*unsafe {
            let ptr = llvm::LLVMRustModuleBufferPtr(self.0);
            let len = llvm::LLVMRustModuleBufferLen(self.0);
            slice::from_raw_parts(ptr, len)
        }*/
    }
}
