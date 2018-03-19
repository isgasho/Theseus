use xmas_elf;
use xmas_elf::ElfFile;
use xmas_elf::sections::{SectionHeader, SectionData, ShType};
use xmas_elf::sections::{SHF_WRITE, SHF_ALLOC, SHF_EXECINSTR};
use core::slice;
use core::ops::DerefMut;
use alloc::{Vec, BTreeMap, BTreeSet, String};
use alloc::arc::Arc;
use alloc::string::ToString;
use memory::{VirtualMemoryArea, VirtualAddress, MappedPages, EntryFlags, ActivePageTable, allocate_pages_by_bytes};
use goblin::elf::reloc::*;
use kernel_config::memory::PAGE_SIZE;
use util::round_up_power_of_two;

pub mod metadata;
use self::metadata::*;

// Can also try this crate: https://crates.io/crates/goblin
// ELF RESOURCE: http://www.cirosantilli.com/elf-hello-world


pub struct ElfProgramSegment {
    /// the VirtualMemoryAddress that will represent the virtual mapping of this Program segment.
    /// Provides starting virtual address, size in memory, mapping flags, and a text description.
    pub vma: VirtualMemoryArea,
    /// the offset of this segment into the file.
    /// This plus the physical address of the Elf file is the physical address of this Program segment.
    pub offset: usize,
}


/// parses an elf executable file as a slice of bytes starting at the given `start_addr`,
/// which must be a VirtualAddress currently mapped into the kernel's address space.
pub fn parse_elf_executable(start_addr: VirtualAddress, size: usize) -> Result<(Vec<ElfProgramSegment>, VirtualAddress), &'static str> {
    debug!("Parsing Elf executable: start_addr {:#x}, size {:#x}({})", start_addr, size, size);
    let start_addr = start_addr as *const u8;
    if start_addr.is_null() {
        return Err("start_addr was null!");
    }

    // SAFE: checked for null
    let byte_slice = unsafe { slice::from_raw_parts(start_addr, size) };
    let elf_file = try!(ElfFile::new(byte_slice));
    // debug!("Elf File: {:?}", elf_file);

    // check that elf_file is an executable type 
    {
        use xmas_elf::header::Type;
        let typ = elf_file.header.pt2.type_().as_type();
        if typ != Type::Executable {
            error!("parse_elf_executable(): ELF file has wrong type {:?}, must be an Executable Elf File!", typ);
            return Err("not a relocatable elf file");
        }
    } 

    let mut prog_sects: Vec<ElfProgramSegment> = Vec::new();
    for prog in elf_file.program_iter() {
        // debug!("   prog: {}", prog);
        use xmas_elf::program::Type;
        let typ = prog.get_type();
        if typ != Ok(Type::Load) {
            warn!("Program type in ELF file wasn't LOAD, {}", prog);
            return Err("Program type in ELF file wasn't LOAD");
        }
        let flags = EntryFlags::from_elf_program_flags(prog.flags());
        use memory::*;
        if !flags.contains(EntryFlags::PRESENT) {
            warn!("Program flags in ELF file wasn't Read, so EntryFlags wasn't PRESENT!! {}", prog);
            return Err("Program flags in ELF file wasn't Read, so EntryFlags wasn't PRESENT!");
        }
        // TODO: how to get name of program section?
        // could infer it based on perms, like .text or .data
        prog_sects.push(ElfProgramSegment {
            vma: VirtualMemoryArea::new(prog.virtual_addr() as VirtualAddress, prog.mem_size() as usize, flags, "test_name"),
            offset: prog.offset() as usize,
        });
    }

    let entry_point = elf_file.header.pt2.entry_point() as VirtualAddress;

    Ok((prog_sects, entry_point))
}



// pub struct ElfTextSection {
//     /// The full demangled name of this text section
//     pub demangled_name: String,
//     // /// The offset where this section exists within the ElfFile.
//     // pub offset: usize,
//     /// the slice including the actual data of this text section
//     pub data: [u8]
//     /// The size in bytes of this text section
//     pub size: usize,
//     /// The flags to be used when mapping this section into memory
//     pub flags: EntryFlags,
// }

/// A representation of a demangled symbol, e.g., my_crate::module::func_name.
/// If the symbol wasn't originally mangled, `symbol` == `full`. 
struct DemangledSymbol {
    // symbol: String,
    full: String, 
    hash: Option<String>,
}

fn demangle_symbol(s: &str) -> DemangledSymbol {
    use rustc_demangle::demangle;
    let demangled = demangle(s);
    let without_hash: String = format!("{:#}", demangled); // the fully-qualified symbol, no hash
    // let symbol_only: Option<String> = without_hash.rsplit("::").next().map(|s| s.to_string()); // last thing after "::", excluding the hash
    let with_hash: String = format!("{}", demangled); // the fully-qualified symbol, with the hash
    let hash_only: Option<String> = with_hash.find::<&str>(without_hash.as_ref())
        .and_then(|index| {
            let hash_start = index + 2 + without_hash.len();
            with_hash.get(hash_start..).map(|s| s.to_string())
        }); // + 2 to skip the "::" separator
    
    DemangledSymbol {
        // symbol: symbol_only.unwrap_or(without_hash.clone()),
        full: without_hash,
        hash: hash_only,
    }
}



pub fn parse_elf_kernel_crate(mapped_pages: MappedPages, size: usize, module_name: &String, active_table: &mut ActivePageTable, log: bool)
    -> Result<LoadedCrate, &'static str>
{
    // all kernel module crate names must start with "__k_"
    const KERNEL_MODULE_NAME_PREFIX: &'static str = "__k_";

    let start_addr = mapped_pages.start_address() as usize as *const u8;
    debug!("Parsing Elf kernel crate: {:?}, start_addr {:#x}, size {:#x}({})", module_name, start_addr as usize, size, size);
    if start_addr.is_null() {
        error!("parse_elf_kernel_crate(): start_addr is null!");
        return Err("start_addr for parse_elf_kernel_crate is null!");
    }
    if !module_name.starts_with("__k_") {
        error!("parse_elf_kernel_crate(): error parsing crate: {}, name must start with {}.", module_name, KERNEL_MODULE_NAME_PREFIX);
        return Err("module_name didn't start with __k_");
    }

    // SAFE: checked for null
    let byte_slice = unsafe { slice::from_raw_parts(start_addr, size) };
    // debug!("BYTE SLICE: {:?}", byte_slice);
    let elf_file = try!(ElfFile::new(byte_slice)); // returns Err(&str) if ELF parse fails

    // check that elf_file is a relocatable type 
    {
        use xmas_elf::header::Type;
        let typ = elf_file.header.pt2.type_().as_type();
        if typ != Type::Relocatable {
            error!("parse_elf_kernel_crate(): module {} was of type {:?}, must be a Relocatable Elf File!", module_name, typ);
            return Err("not a relocatable elf file");
        }
    } 


    // For us to properly load the ELF file, it must NOT have been stripped,
    // meaning that it must still have its symbol table section. Otherwise, relocations will not work.
    use xmas_elf::sections::SectionData::SymbolTable64;
    let symtab_data = match find_first_section_by_type(&elf_file, ShType::SymTab).ok_or("no symtab section").and_then(|s| s.get_data(&elf_file)) {
        Ok(SymbolTable64(symtab)) => Ok(symtab),
        _ => {
            error!("parse_elf_kernel_crate(): can't load file: no symbol table found. Was file stripped?");
            Err("cannot load: no symbol table found. Was file stripped?")
        }
    };
    let symtab = try!(symtab_data);
    // debug!("symtab: {:?}", symtab);

    // iterate through the symbol table so we can find which sections are global (publicly visible)
    // we keep track of them here in a list
    let global_sections: BTreeSet<usize> = {
        let mut globals: BTreeSet<usize> = BTreeSet::new();
        use xmas_elf::symbol_table::Entry;
        for entry in symtab.iter() {
            if let Ok(typ) = entry.get_type() {
                if typ == xmas_elf::symbol_table::Type::Func || typ == xmas_elf::symbol_table::Type::Object {
                    use xmas_elf::symbol_table::Visibility;
                    match entry.get_other() {
                        Visibility::Default => {
                            if let Ok(bind) = entry.get_binding() {
                                if bind == xmas_elf::symbol_table::Binding::Global {
                                    globals.insert(entry.shndx() as usize);
                                }
                            }
                        }
                        _ => {
                            continue;
                        }
                    }
                }
            }
        }   
        globals 
    };

    // Calculate how many bytes (and thus how many pages) we need for each of the three section types,
    // which are text (present | exec), rodata (present | noexec), data/bss (present | writable)
    let (text_bytecount, rodata_bytecount, data_bytecount): (usize, usize, usize) = {
        let (mut text, mut rodata, mut data) = (0, 0, 0);
        for sec in elf_file.section_iter() {
            let sec_typ = sec.get_type();
            // look for .text, .rodata, .data, and .bss sections
            if sec_typ == Ok(ShType::ProgBits) || sec_typ == Ok(ShType::NoBits) {
                let size = sec.size() as usize;
                if (size == 0) || (sec.flags() & SHF_ALLOC == 0) {
                    continue; // skip non-allocated sections (they're useless)
                }

                let align = sec.align() as usize;
                let addend = round_up_power_of_two(size, align);
                if log { info!("section {:?} needs {:#X}({}) bytes", sec.get_name(&elf_file), addend, addend); }

                // filter flags for ones we care about (we already checked that it's loaded (SHF_ALLOC))
                let write: bool = sec.flags() & SHF_WRITE     == SHF_WRITE;
                let exec:  bool = sec.flags() & SHF_EXECINSTR == SHF_EXECINSTR;
                if exec {
                    // trace!("  Looking at sec with size {:#X} align {:#X} --> addend {:#X}", size, align, addend);
                    text += addend;
                }
                else if write {
                    // .bss sections have the same flags (write and alloc) as data, so combine them
                    data += addend;
                }
                else {
                    rodata += addend;
                }
            }
        }
        (text, rodata, data)
    };

    if log {
        debug!("    crate {} needs {:#X} text bytes, {:#X} rodata bytes, {:#X} data bytes", module_name, text_bytecount, rodata_bytecount, data_bytecount);
    }

    // create a closure here to allocate N contiguous virtual memory pages
    // and map them to random frames as writable, returns Result<MappedPages, &'static str>
    let (text_pages, rodata_pages, data_pages): (Result<MappedPages, &'static str>,
                                                 Result<MappedPages, &'static str>, 
                                                 Result<MappedPages, &'static str>) = {
        use memory::FRAME_ALLOCATOR;
        let mut frame_allocator = try!(FRAME_ALLOCATOR.try().ok_or("couldn't get FRAME_ALLOCATOR")).lock();

        let mut allocate_pages_closure = |size_in_bytes: usize| {
            let allocated_pages = try!(allocate_pages_by_bytes(size_in_bytes).ok_or("Couldn't allocate_pages_by_bytes, out of virtual address space"));

            // Right now we're just simply copying small sections to the new memory,
            // so we have to map those pages to real (randomly chosen) frames first. 
            // because we're copying bytes to the newly allocated pages, we need to make them writeable too, 
            // and then change the page permissions (by using remap) later. 
            active_table.map_allocated_pages(allocated_pages, EntryFlags::PRESENT | EntryFlags::WRITABLE, frame_allocator.deref_mut())
        };

        // we must allocate these pages separately because they will have different flags later
        (
            if text_bytecount   > 0 { allocate_pages_closure(text_bytecount)   } else { Err("no text sections present")   }, 
            if rodata_bytecount > 0 { allocate_pages_closure(rodata_bytecount) } else { Err("no rodata sections present") }, 
            if data_bytecount   > 0 { allocate_pages_closure(data_bytecount)   } else { Err("no data sections present")   }
        )
    };


    // First, we need to parse all the sections and load the text and data sections
    let mut loaded_sections: BTreeMap<usize, Arc<LoadedSection>> = BTreeMap::new(); // map section header index (shndx) to LoadedSection
    let mut text_offset:   usize = 0;
    let mut rodata_offset: usize = 0;
    let mut data_offset:   usize = 0;

                
    const TEXT_PREFIX:   &'static str = ".text.";
    const RODATA_PREFIX: &'static str = ".rodata.";
    const DATA_PREFIX:   &'static str = ".data.";
    const BSS_PREFIX:    &'static str = ".bss.";


    for (shndx, sec) in elf_file.section_iter().enumerate() {
        // the PROGBITS sections (.text, .rodata, .data) and the NOBITS (.bss) sections are what we care about
        let sec_typ = sec.get_type();
        // look for PROGBITS (.text, .rodata, .data) and NOBITS (.bss) sections
        if sec_typ == Ok(ShType::ProgBits) || sec_typ == Ok(ShType::NoBits) {

            // even if we're using the next section's data (for a zero-sized section),
            // we still want to use this current section's actual name and flags!
            let sec_flags = sec.flags();
            let sec_name = match sec.get_name(&elf_file) {
                Ok(name) => name,
                Err(_e) => {
                    error!("parse_elf_kernel_crate: couldn't get section name for section [{}]: {:?}\n    error: {}", shndx, sec, _e);
                    return Err("couldn't get section name");
                }
            };
            

            let sec = if sec.size() == 0 {
                // This is a very rare case of a zero-sized section. 
                // A section of size zero shouldn't necessarily be removed, as they are sometimes referenced in relocations,
                // typically the zero-sized section itself is a reference to the next section in the list of section headers).
                // Thus, we need to use the *current* section's name with the *next* section's (the next section's) information,
                // i.e., its  size, alignment, and actual data
                match elf_file.section_header((shndx + 1) as u16) { // get the next section
                    Ok(sec_hdr) => sec_hdr,
                    _ => {
                        error!("parse_elf_kernel_crate(): Couldn't get next section for zero-sized section {}", shndx);
                        return Err("couldn't get next section for a zero-sized section");
                    }
                }
            }
            else {
                // this is the normal case, a non-zero sized section, so just use the current section
                sec
            };

            // get the relevant section info, i.e., size, alignment, and data contents
            let sec_size  = sec.size()  as usize;
            let sec_align = sec.align() as usize;
            let sec_data  = if sec_name.starts_with(BSS_PREFIX) { // .bss section must have Empty data
                match sec.get_data(&elf_file) {
                    Ok(SectionData::Empty) => &[0], // an empty slice, we won't use it anyway
                    _ => {
                        error!("parse_elf_kernel_crate(): .bss section [{}] {} had data that wasn't Empty. {:?}", shndx, sec_name, sec.get_data(&elf_file));
                        return Err(".bss section had data that wasn't Empty");
                    }
                }
            } else {
                match sec.get_data(&elf_file) {
                    Ok(SectionData::Undefined(sec_data)) => sec_data,
                    _ => {
                        error!("parse_elf_kernel_crate(): Couldn't get data (expected \"Undefined\" data) for section [{}] {}: {:?}", shndx, sec_name, sec.get_data(&elf_file));
                        return Err("couldn't get sec_data in .text, .data, or .rodata section");
                    }
                }
                
            };
            


            if sec_name.starts_with(TEXT_PREFIX) {
                if let Some(name) = sec_name.get(TEXT_PREFIX.len() ..) {
                    let demangled = demangle_symbol(name);
                    if log { trace!("Found [{}] .text section: name {:?}, with_hash {:?}, size={:#x}", shndx, name, demangled.full, sec_size); }
                    assert!(sec_flags & (SHF_ALLOC | SHF_WRITE | SHF_EXECINSTR) == (SHF_ALLOC | SHF_EXECINSTR), ".text section had wrong flags!");

                    if let Ok(ref tp) = text_pages {
                        let dest_addr = tp.start_address() + text_offset;
                        if log { trace!("       dest_addr: {:#X}, text_pages: {:#X} text_offset: {:#X}", dest_addr, tp.start_address(), text_offset); }
                        
                        // here: we're ready to copy the text section to the proper address
                        // SAFE: we have allocated the pages containing section_vaddr and mapped them above
                        let dest: &mut [u8] = unsafe {
                            slice::from_raw_parts_mut(dest_addr as *mut u8, sec_size) 
                        };
                        dest.copy_from_slice(sec_data);

                        loaded_sections.insert(shndx, 
                            Arc::new( LoadedSection::Text(TextSection{
                                // symbol: demangled.symbol,
                                abs_symbol: demangled.full,
                                hash: demangled.hash,
                                virt_addr: dest_addr,
                                size: sec_size,
                                global: global_sections.contains(&shndx),
                            }))
                        );

                        text_offset += round_up_power_of_two(sec_size, sec_align);
                    }
                    else {
                        return Err("no text_pages were allocated");
                    }
                }
                else {
                    error!("Failed to get the .text section's name after \".text.\": {:?}", sec_name);
                    return Err("Failed to get the .text section's name after \".text.\"!");
                }
            }

            else if sec_name.starts_with(RODATA_PREFIX) {
                if let Some(name) = sec_name.get(RODATA_PREFIX.len() ..) {
                    let demangled = demangle_symbol(name);
                    if log { trace!("Found [{}] .rodata section: name {:?}, demangled {:?}, size={:#x}", shndx, name, demangled.full, sec_size); }
                    assert!(sec_flags & (SHF_ALLOC | SHF_WRITE | SHF_EXECINSTR) == (SHF_ALLOC), ".rodata section had wrong flags!");

                    if let Ok(ref rp) = rodata_pages {
                        let dest_addr = rp.start_address() + rodata_offset;
                        if log { trace!("       dest_addr: {:#X}, rodata_pages: {:#X} rodata_offset: {:#X}", dest_addr, rp.start_address(), rodata_offset); }
                        
                        // here: we're ready to copy the rodata section to the proper address
                        // SAFE: we have allocated the pages containing section_vaddr and mapped them above
                        let dest: &mut [u8] = unsafe {
                            slice::from_raw_parts_mut(dest_addr as *mut u8, sec_size) 
                        };
                        dest.copy_from_slice(sec_data);

                        loaded_sections.insert(shndx, 
                            Arc::new( LoadedSection::Rodata(RodataSection{
                                // symbol: demangled.symbol,
                                abs_symbol: demangled.full,
                                hash: demangled.hash,
                                virt_addr: dest_addr,
                                size: sec_size,
                                global: global_sections.contains(&shndx),
                            }))
                        );

                        rodata_offset += round_up_power_of_two(sec_size, sec_align);
                    }
                    else {
                        return Err("no rodata_pages were allocated");
                    }
                }
                else {
                    error!("Failed to get the .rodata section's name after \".rodata.\": {:?}", sec_name);
                    return Err("Failed to get the .rodata section's name after \".rodata.\"!");
                }
            }

            else if sec_name.starts_with(DATA_PREFIX) {
                if let Some(name) = sec_name.get(DATA_PREFIX.len() ..) {
                    let demangled = demangle_symbol(name);
                    if log { trace!("Found [{}] .data section: name {:?}, with_hash {:?}, size={:#x}", shndx, name, demangled.full, sec_size); }
                    assert!(sec_flags & (SHF_ALLOC | SHF_WRITE | SHF_EXECINSTR) == (SHF_ALLOC | SHF_WRITE), ".data section had wrong flags!");
                    
                    if let Ok(ref dp) = data_pages {
                        let dest_addr = dp.start_address() + data_offset;
                        if log { trace!("       dest_addr: {:#X}, data_pages: {:#X} data_offset: {:#X}", dest_addr, dp.start_address(), data_offset); }

                        // here: we're ready to copy the data/bss section to the proper address
                        // SAFE: we have allocated the pages containing section_vaddr and mapped them above
                        let dest: &mut [u8] = unsafe {
                            slice::from_raw_parts_mut(dest_addr as *mut u8, sec_size) 
                        };
                        dest.copy_from_slice(sec_data);

                        loaded_sections.insert(shndx, 
                            Arc::new( LoadedSection::Data(DataSection{
                                // symbol: demangled.symbol,
                                abs_symbol: demangled.full,
                                hash: demangled.hash,
                                virt_addr: dest_addr,
                                size: sec_size,
                                global: global_sections.contains(&shndx),
                            }))
                        );

                        data_offset += round_up_power_of_two(sec_size, sec_align);
                    }
                    else {
                        return Err("no data_pages were allocated for .data section");
                    }
                }
                
                else {
                    error!("Failed to get the .data section's name after \".data.\": {:?}", sec_name);
                    return Err("Failed to get the .data section's name after \".data.\"!");
                }
            }

            else if sec_name.starts_with(BSS_PREFIX) {
                if let Some(name) = sec_name.get(BSS_PREFIX.len() ..) {
                    let demangled = demangle_symbol(name);
                    if log { trace!("Found [{}] .bss section: name {:?}, with_hash {:?}, size={:#x}", shndx, name, demangled.full, sec_size); }
                    assert!(sec_flags & (SHF_ALLOC | SHF_WRITE | SHF_EXECINSTR) == (SHF_ALLOC | SHF_WRITE), ".bss section had wrong flags!");
                    
                    // we still use DataSection to represent the .bss sections, since they have the same flags
                    if let Ok(ref dp) = data_pages { 
                        let dest_addr = dp.start_address() + data_offset;
                        if log { trace!("       dest_addr: {:#X}, data_pages: {:#X} data_offset: {:#X}", dest_addr, dp.start_address(), data_offset); }

                        // here: we're ready to fill the bss section with zeroes at the proper address
                        // SAFE: we have allocated the pages containing section_vaddr and mapped them above
                        unsafe {
                            ::core::intrinsics::write_bytes(dest_addr as *mut u8, 0, sec_size);
                        }

                        loaded_sections.insert(shndx, 
                            Arc::new( LoadedSection::Data(DataSection{
                                // symbol: demangled.symbol,
                                abs_symbol: demangled.full,
                                hash: demangled.hash,
                                virt_addr: dest_addr,
                                size: sec_size,
                                global: global_sections.contains(&shndx),
                            }))
                        );

                        data_offset += round_up_power_of_two(sec_size, sec_align);
                    }
                    else {
                        return Err("no data_pages were allocated for .bss section");
                    }
                }
                
                else {
                    error!("Failed to get the .bss section's name after \".bss.\": {:?}", sec_name);
                    return Err("Failed to get the .bss section's name after \".bss.\"!");
                }
            }

            else {
                // some special sections are fine to ignore
                if  sec_name.starts_with(".note")   ||   // ignore GNU note sections
                    sec_name.starts_with(".gcc")    ||   // ignore gcc special sections for now
                    sec_name.starts_with(".debug")  ||   // ignore debug special sections for now
                    sec_name == ".text"                  // ignore the header .text section (with no content)
                {
                    continue;    
                }

                error!("unhandled PROGBITS/NOBITS section [{}], name: {}, sec: {:?}", shndx, sec_name, sec);
                continue;
            }

        
        }
    }  // end of handling PROGBITS sections: text, data, rodata, bss


    if log {
        debug!("=========== moving on to the relocations for module {} =========", module_name);
    }


    // Second, we need to fix up the sections we just loaded with proper relocation info
    for sec in elf_file.section_iter() {

        if let Ok(ShType::Rela) = sec.get_type() {
            // skip null section and any empty sections
            let sec_size = sec.size() as usize;
            if sec_size == 0 { continue; }

            // offset is the destination 
            use xmas_elf::sections::SectionData::Rela64;
            use xmas_elf::symbol_table::Entry;
            if log { trace!("Found Rela section name: {:?}, type: {:?}, target_sec_index: {:?}", sec.get_name(&elf_file), sec.get_type(), sec.info()); }

            // currently not using eh_frame, gcc, note, and debug sections
            if let Ok(name) = sec.get_name(&elf_file) {
                if  name.starts_with(".rela.eh_frame")   || 
                    name.starts_with(".rela.note")   ||   // ignore GNU note sections
                    name.starts_with(".rela.gcc")    ||   // ignore gcc special sections for now
                    name.starts_with(".rela.debug")       // ignore debug special sections for now
                {
                    continue;
                }
            }

            // the target section is where we write the relocation data to.
            // the source section is where we get the data from. 
            // There is one target section per rela section, and one source section per entry in this rela section.
            // The "info" field in the Rela section specifies which section is the target of the relocation.
            
            // check if this Rela sections has a valid target section (one that we've already loaded)
            if let Some(target_sec) = loaded_sections.get(&(sec.info() as usize)) {
                if let Ok(Rela64(rela_arr)) = sec.get_data(&elf_file) {
                    for r in rela_arr {
                        if log { trace!("      Rela64 offset: {:#X}, addend: {:#X}, symtab_index: {}, type: {:#X}", r.get_offset(), r.get_addend(), r.get_symbol_table_index(), r.get_type()); }

                        // common to all relocations: calculate the relocation destination and get the source section
                        let dest_offset = r.get_offset() as usize;
                        let dest_ptr: usize = target_sec.virt_addr() + dest_offset;
                        let source_sec_entry: &Entry = &symtab[r.get_symbol_table_index() as usize];
                        let source_sec_shndx: u16 = source_sec_entry.shndx(); 
                        if log { 
                            let source_sec_header = source_sec_entry.get_section_header(&elf_file, r.get_symbol_table_index() as usize)
                                                                    .and_then(|s| s.get_name(&elf_file));
                            trace!("             relevant section [{}]: {:?}", source_sec_shndx, source_sec_header);
                            // trace!("             Entry name {} {:?} vis {:?} bind {:?} type {:?} shndx {} value {} size {}", 
                            //     source_sec_entry.name(), source_sec_entry.get_name(&elf_file), 
                            //     source_sec_entry.get_other(), source_sec_entry.get_binding(), source_sec_entry.get_type(), 
                            //     source_sec_entry.shndx(), source_sec_entry.value(), source_sec_entry.size());
                        }

                        use xmas_elf::sections::{SHN_UNDEF, SHN_LORESERVE, SHN_LOPROC, SHN_HIPROC, SHN_LOOS, SHN_HIOS, SHN_ABS, SHN_COMMON, SHN_XINDEX, SHN_HIRESERVE};

                        let source_sec: Result<Arc<LoadedSection>, &'static str> = match source_sec_shndx {
                            SHN_LORESERVE | SHN_LOPROC | SHN_HIPROC | SHN_LOOS | SHN_HIOS | SHN_COMMON | SHN_XINDEX | SHN_HIRESERVE => {
                                error!("Unsupported source section shndx {} in symtab entry {}", source_sec_shndx, r.get_symbol_table_index());
                                Err("Unsupported source section shndx")
                            }
                            SHN_ABS  => {
                                error!("No support for SHN_ABS source section shndx ({}), found in symtab entry {}", source_sec_shndx, r.get_symbol_table_index());
                                Err("Unsupported source section shndx SHN_ABS!!")
                            }
                            // match anything else, i.e., a valid source section shndx
                            shndx => {
                                // first, we try to get the relevant section based on its shndx only
                                let loaded_sec = if shndx == SHN_UNDEF { None } else { loaded_sections.get(&(shndx as usize)) };
                                match loaded_sec {
                                    Some(sec) => Ok(sec.clone()), // yay, we found the source_sec 
                                    None => { 
                                        // second, if we couldn't get the section based on its shndx, it means that the source section wasn't in this module.
                                        // Thus, we *have* to to get the source section's name and check our list of loaded external crates to see if it's there.
                                        // At this point, there's no other way to search for the source section besides its name
                                        match source_sec_entry.get_name(&elf_file) {
                                            Ok(source_sec_name) => {
                                                // search for the symbol's demangled name in the kernel's symbol map
                                                let demangled = demangle_symbol(source_sec_name);
                                                match metadata::get_symbol(demangled.full).upgrade() {
                                                    Some(sec) => Ok(sec), 
                                                    None => {
                                                        // if we couldn't get the source section based on its shndx, nor based on its name, then that's an error
                                                        let source_sec_header = source_sec_entry.get_section_header(&elf_file, r.get_symbol_table_index() as usize)
                                                                                                .and_then(|s| s.get_name(&elf_file));
                                                        error!("Could not resolve source section for symbol relocation for symtab[{}] name={:?} header={:?}", 
                                                                shndx, source_sec_name, source_sec_header);
                                                        Err("Could not resolve source section for symbol relocation")
                                                    }
                                                }
                                            }
                                            Err(_e) => {
                                                error!("Couldn't get source section [{}]'s name when necessary for non-local relocation entry", shndx);
                                                Err("Couldn't get source section's name when necessary for non-local relocation entry")
                                            }
                                        }
                                    }
                                }
                            }
                        };

                        let source_sec = try!(source_sec);
                        
                        

                        // There is a great, succint table of relocation types here
                        // https://docs.rs/goblin/0.0.13/goblin/elf/reloc/index.html
                        match r.get_type() {
                            R_X86_64_32 => {
                                let source_val = source_sec.virt_addr().wrapping_add(r.get_addend() as usize);
                                if log { trace!("                    dest_ptr: {:#X}, source_val: {:#X} ({:?})", dest_ptr, source_val, source_sec); }
                                unsafe {
                                    *(dest_ptr as *mut u32) = source_val as u32;
                                }
                            }
                            R_X86_64_64 => {
                                let source_val = source_sec.virt_addr().wrapping_add(r.get_addend() as usize);
                                if log { trace!("                    dest_ptr: {:#X}, source_val: {:#X} ({:?})", dest_ptr, source_val, source_sec); }
                                unsafe {
                                    *(dest_ptr as *mut u64) = source_val as u64;
                                }
                            }
                            R_X86_64_PC32 => {
                                // trace!("                 dest_ptr: {:#X}, source_sec_vaddr: {:#X}, addend: {:#X}", dest_ptr, source_sec.virt_addr(), r.get_addend());
                                let source_val = source_sec.virt_addr().wrapping_add(r.get_addend() as usize).wrapping_sub(dest_ptr);
                                if log { trace!("                    dest_ptr: {:#X}, source_val: {:#X} ({:?})", dest_ptr, source_val, source_sec); }
                                unsafe {
                                    *(dest_ptr as *mut u32) = source_val as u32;
                                }
                            }
                            R_X86_64_PC64 => {
                                let source_val = source_sec.virt_addr().wrapping_add(r.get_addend() as usize).wrapping_sub(dest_ptr);
                                if log { trace!("                    dest_ptr: {:#X}, source_val: {:#X} ({:?})", dest_ptr, source_val, source_sec); }
                                unsafe {
                                    *(dest_ptr as *mut u64) = source_val as u64;
                                }
                            }
                            // R_X86_64_GOTPCREL => { 
                            //     unimplemented!(); // if we stop using the large code model, we need to create a Global Offset Table
                            // }
                            _ => {
                                error!("found unsupported relocation {:?}\n  --> Are you building kernel crates with code-model=large?", r);
                                return Err("found unsupported relocation type");
                            }
                        }   
                    }
                }
                else {
                    error!("Found Rela section that wasn't able to be parsed as Rela64: {:?}", sec);
                    return Err("Found Rela section that wasn't able to be parsed as Rela64");
                }
            }
            else {
                error!("Skipping Rela section {:?} for target section that wasn't loaded!", sec.get_name(&elf_file));
                continue;
            }
        }
    }

    
    // since we initially mapped the pages as writable, we need to remap them properly according to each section
    let mut all_pages: Vec<MappedPages> = Vec::with_capacity(3); // max 3, for text, rodata, data/bss
    if let Ok(tp) = text_pages { 
        try!(active_table.remap(&tp, EntryFlags::PRESENT)); // present and not noexec
        all_pages.push(tp);
    }
    if let Ok(rp) = rodata_pages { 
        try!(active_table.remap(&rp, EntryFlags::PRESENT | EntryFlags::NO_EXECUTE)); // present (just readable)
        all_pages.push(rp);
    }
    if let Ok(dp) = data_pages { 
        try!(active_table.remap(&dp, EntryFlags::PRESENT | EntryFlags::WRITABLE | EntryFlags::NO_EXECUTE)); // read/write
        all_pages.push(dp);
    }
    
    // extract just the sections from the section map
    let (_keys, values): (Vec<usize>, Vec<Arc<LoadedSection>>) = loaded_sections.into_iter().unzip();
    let kernel_module_name_prefix_end = KERNEL_MODULE_NAME_PREFIX.len();


    Ok(LoadedCrate {
        crate_name: String::from(module_name.get(kernel_module_name_prefix_end..).unwrap()), 
        sections: values,
        mapped_pages: all_pages,
    })

}



// Parses the nano_core symbol file that represents the already loaded (and currently running) nano_core code.
// Basically, just searches for global (public) symbols, which are added to the system map and the crate metadata.
pub fn parse_nano_core_symbols(mapped_pages: MappedPages, size: usize) -> Result<LoadedCrate, &'static str> {
    use util::c_str::CStr;

    let start_addr = mapped_pages.start_address() as usize as *const u8;
    debug!("Parsing nano_core symbols: start_addr {:#x}, size {:#x}({}), MappedPages: {:?}", start_addr as usize, size, size, mapped_pages);
    if size > (mapped_pages.size_in_pages() * PAGE_SIZE) {
        error!("parse_nano_core_symbols(): size {:#X}({}) exceeds the bounds of the given MappedPages: {:?}", size, size, mapped_pages);
        return Err("parse_nano_core_symbols(): size exceeds the bounds of the given MappedPages!");
    }

    // SAFE: checked for size bounds
    let bytes = unsafe { 
        *((start_addr as usize + size - 1) as *mut u8) = 0u8; // put null byte at the end
        slice::from_raw_parts(start_addr, size)
    };
    let symbol_cstr = try!( CStr::from_bytes_with_nul(bytes).map_err(|e| {
        error!("parse_nano_core_symbols(): error casting memory to CStr: {:?}", e);
        "FromBytesWithNulError occurred when casting nano_core symbol memory to CStr"
    }));
    let symbol_str = try!(symbol_cstr.to_str().map_err(|e| {
        error!("parse_nano_core_symbols(): error with CStr::to_str(): {:?}", e);
        "Utf8Error occurred when parsing nano_core symbols CStr"
    }));

    // debug!("========================= NANO_CORE SYMBOL STRING ========================\n{}", symbol_str);

    let mut sections: Vec<Arc<LoadedSection>> = Vec::new();

    let mut text_shndx:   Option<usize> = None;
    let mut data_shndx:   Option<usize> = None;
    let mut rodata_shndx: Option<usize> = None;
    let mut bss_shndx:    Option<usize> = None;

    for (_line_num, line) in symbol_str.lines().enumerate() {
        let line = line.trim();
        // skip empty lines
        if line.is_empty() { continue; }

        // debug!("Looking at line: {:?}", line);

        // find the .text, .data, .rodata, and .bss section indices
        if line.contains(".text") && line.contains("PROGBITS") {
            text_shndx = get_section_index(line);
        }
        else if line.contains(".data") && line.contains("PROGBITS") {
            data_shndx = get_section_index(line);
        }
        else if line.contains(".rodata") && line.contains("PROGBITS") {
            rodata_shndx = get_section_index(line);
        }
        else if line.contains(".bss") && line.contains("NOBITS") {
            bss_shndx = get_section_index(line);
        }

        
        // find a symbol table entry, either "GLOBAL DEFAULT" or "GLOBAL HIDDEN"
        if line.contains("GLOBAL ") {
            // we need the following items from a symbol table entry:
            // * Value (address),  column 1
            // * Size,             column 2
            // * Ndx,              column 6
            // * Name (mangled),   column 7
            let mut tokens   = line.split_whitespace();
            let _num         = try!(tokens.next().ok_or("parse_nano_core_symbols(): couldn't get column 0"));
            let sec_vaddr    = try!(tokens.next().ok_or("parse_nano_core_symbols(): couldn't get column 1"));
            let sec_size     = try!(tokens.next().ok_or("parse_nano_core_symbols(): couldn't get column 2"));
            let _typ         = try!(tokens.next().ok_or("parse_nano_core_symbols(): couldn't get column 3"));
            let _bind        = try!(tokens.next().ok_or("parse_nano_core_symbols(): couldn't get column 4"));
            let _vis         = try!(tokens.next().ok_or("parse_nano_core_symbols(): couldn't get column 5"));
            let sec_ndx      = try!(tokens.next().ok_or("parse_nano_core_symbols(): couldn't get column 6"));
            let name_mangled = try!(tokens.next().ok_or("parse_nano_core_symbols(): couldn't get column 7"));

            
            let sec_vaddr = try!(usize::from_str_radix(sec_vaddr, 16).map_err(|e| {
                error!("parse_nano_core_symbols(): error parsing virtual address Value at line {}: {:?}\n    line: {}", _line_num, e, line);
                "parse_nano_core_symbols(): couldn't parse virtual address Value"
            })); 
            let sec_size  = try!(usize::from_str_radix(sec_size, 10).map_err(|e| {
                error!("parse_nano_core_symbols(): error parsing size at line {}: {:?}\n    line: {}", _line_num, e, line);
                "parse_nano_core_symbols(): couldn't parse size"
            })); 
            // while vaddr and size are required, ndx isn't. If ndx is not a number (like "ABS"), then we just skip that entry. 
            let sec_ndx   = usize::from_str_radix(sec_ndx, 10).ok(); 
            if sec_ndx.is_none() {
                // trace!("parse_nano_core_symbols(): skipping line {}: {}", _line_num, line);
                continue;
            }

            let demangled = demangle_symbol(name_mangled);
            // debug!("parse_nano_core_symbols(): name: {}, demangled: {}, vaddr: {:#X}, size: {:#X}", name_mangled, demangled.full, sec_vaddr, sec_size);


            let new_section = {
                if sec_ndx == text_shndx {
                    Some(LoadedSection::Text(TextSection{
                        // symbol: demangled.symbol,
                        abs_symbol: demangled.full,
                        hash: demangled.hash,
                        virt_addr: sec_vaddr,
                        size: sec_size,
                        global: true,
                    }))
                }
                else if sec_ndx == rodata_shndx {
                    Some(LoadedSection::Rodata(RodataSection{
                        // symbol: demangled.symbol,
                        abs_symbol: demangled.full,
                        hash: demangled.hash,
                        virt_addr: sec_vaddr,
                        size: sec_size,
                        global: true,
                    }))
                }
                else if (sec_ndx == data_shndx) || (sec_ndx == bss_shndx) {
                    Some(LoadedSection::Data(DataSection{
                        // symbol: demangled.symbol,
                        abs_symbol: demangled.full,
                        hash: demangled.hash,
                        virt_addr: sec_vaddr,
                        size: sec_size,
                        global: true,
                    }))
                }
                else {
                    None
                }
            };

            if let Some(sec) = new_section {
                sections.push(Arc::new(sec));
            }
        }  

    }

    Ok(LoadedCrate {
        crate_name: String::from("nano_core"), 
        sections: sections,
        mapped_pages: vec![mapped_pages],
    })

}



fn get_section_index<'a>(s: &str) -> Option<usize> {
    let open  = s.find("[");
    let close = s.find("]");
    open.and_then(|start| close.and_then(|end| s.get((start + 1) .. end)))
        .and_then(|t| t.trim().parse::<usize>().ok())
}




/// Finds a section of the given `ShType` and returns the "first" one 
/// based on the (potentially random) ordering of sections in the given `ElfFile`.
pub fn find_first_section_by_type<'a>(elf_file: &'a ElfFile, typ: ShType) -> Option<SectionHeader<'a>> {
    for sec in elf_file.section_iter() {
        if let Ok(sec_type) = sec.get_type() {
            if typ == sec_type {
                return Some(sec);
            }
        }
    }

    None
}
