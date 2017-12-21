//! The Mach 32/64 bit backend for transforming an artifact to a valid, mach-o object file.

use {Artifact, Target, Object, Ctx};
use artifact::{Decl, Definition};

use failure::Error;
use ordermap::OrderMap;
use string_interner::{DefaultStringInterner};
//use std::collections::HashMap;
use std::io::{Seek, Cursor, BufWriter, Write};
use std::io::SeekFrom::*;
use scroll::{Pwrite, IOwrite};
use scroll::ctx::SizeWith;

use goblin::mach::cputype;
use goblin::mach::segment::{Section, Segment};
use goblin::mach::load_command::SymtabCommand;
use goblin::mach::header::{Header, MH_OBJECT, MH_SUBSECTIONS_VIA_SYMBOLS};
use goblin::mach::symbols::Nlist;
use goblin::mach::relocation::{RelocationInfo, RelocType, SIZEOF_RELOCATION_INFO};

struct CpuType(cputype::CpuType);

impl From<Target> for CpuType {
    fn from(target: Target) -> CpuType {
        use self::Target::*;
        use mach::cputype::*;
        CpuType(match target {
            X86_64 => CPU_TYPE_X86_64,
            X86 => CPU_TYPE_X86,
            ARM64 => CPU_TYPE_ARM64,
            ARMv7 => CPU_TYPE_ARM,
            Unknown => 0
        })
    }
}

pub type SectionIndex = usize;
pub type StrtableOffset = usize;

const CODE_SECTION_INDEX: SectionIndex = 0;
const DATA_SECTION_INDEX: SectionIndex = 1;

/// A builder for creating a 32/64 bit Mach-o Nlist symbol
#[derive(Debug)]
pub struct SymbolBuilder {
    name: StrtableOffset,
    section: Option<SectionIndex>,
    global: bool,
    import: bool,
    offset: usize,
}

impl SymbolBuilder {
    /// Create a new symbol with `typ`
    pub fn new(name: StrtableOffset) -> Self {
        SymbolBuilder {
            name,
            section: None,
            global: false,
            import: false,
            offset: 0,
        }
    }
    /// The section this symbol belongs to
    pub fn section(mut self, section_index: SectionIndex) -> Self {
        self.section = Some(section_index); self
    }
    /// Is this symbol global?
    pub fn global(mut self, global: bool) -> Self {
        self.global = global; self
    }
    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = offset; self
    }
    pub fn get_offset(&self) -> usize {
        self.offset
    }
    /// Is this symbol an import?
    pub fn import(mut self) -> Self {
        self.import = true; self
    }
    /// Finalize and create the symbol
    /// The n_value (offset into section) is still unset, and needs to be generated by the client
    pub fn create(self) -> Nlist {
        use goblin::mach::symbols::{N_EXT, N_UNDF, N_SECT, NO_SECT};
        let n_strx = self.name;
        let mut n_sect = 0;
        let mut n_type = N_UNDF;
        let mut n_value = self.offset as u64;
        let n_desc = 0;
        if self.global {
            n_type |= N_EXT;
        } else {
            n_type &= !N_EXT;
        }
        if let Some(idx) = self.section {
            n_sect = idx + 1; // add 1 because n_sect expects ordinal
            n_type |= N_SECT;
        }

        if self.import {
            n_sect = NO_SECT as usize;
            // FIXME: this is broken i believe; we need to make it both undefined + global for imports
            n_type = N_EXT;
            n_value = 0;
        } else {
            n_type |= N_SECT;
        }

        Nlist {
            n_strx,
            n_type,
            n_sect,
            n_desc,
            n_value
        }
    }
}

/// An index into the symbol table
pub type SymbolIndex = usize;

/// Mach relocation builder
#[derive(Debug)]
pub struct RelocationBuilder {
    symbol: SymbolIndex,
    relocation_offset: usize,
    absolute: bool,
    r_type: RelocType,
}

impl RelocationBuilder {
    /// Create a relocation for `symbol`, starting at `relocation_offset`
    pub fn new(symbol: SymbolIndex, relocation_offset: usize, r_type: RelocType) -> Self {
        RelocationBuilder {
            symbol,
            relocation_offset,
            absolute: false,
            r_type,
        }
    }
    /// This is an absolute relocation
    pub fn absolute(mut self) -> Self {
        self.absolute = true; self
    }
    /// Finalize and create the relocation
    pub fn create(self) -> RelocationInfo {
        // it basically goes sort of backwards than what you'd expect because C bitfields are bonkers
        let r_symbolnum: u32 = self.symbol as u32;
        let r_pcrel: u32 = if self.absolute { 0 } else { 1 } << 24;
        let r_length: u32 = if self.absolute { 3 } else { 2 } << 25;
        let r_extern: u32 = 1 << 27;
        let r_type = (self.r_type as u32) << 28;
        // r_symbolnum, 24 bits, r_pcrel 1 bit, r_length 2 bits, r_extern 1 bit, r_type 4 bits
        let r_info = r_symbolnum | r_pcrel | r_length | r_extern | r_type;
        RelocationInfo {
            r_address: self.relocation_offset as i32,
            r_info,
        }
    }
}

/// Helper to build sections
#[derive(Debug, Clone)]
pub struct SectionBuilder {
    addr: usize,
    align: usize,
    offset: usize,
    size: usize,
    sectname: &'static str,
    segname: &'static str,
}

impl SectionBuilder {
    /// Create a new section builder with `sectname`, `segname` and `size`
    pub fn new(sectname: &'static str, segname: &'static str, size: usize) -> Self {
        SectionBuilder {
            addr: 0,
            align: 4,
            offset: 0,
            size,
            sectname,
            segname,
        }
    }
    /// Set the vm address of this section
    pub fn addr(mut self, addr: usize) -> Self {
        self.addr = addr; self
    }
    /// Set the file offset of this section
    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = offset; self
    }
    /// Set the alignment of this section
    pub fn align(mut self, align: usize) -> Self {
        self.align = align; self
    }
    /// Finalize and create the actual Mach-o section
    pub fn create(self) -> Section {
        let mut sectname = [0u8; 16];
        sectname.pwrite(self.sectname, 0).unwrap();
        let mut segname = [0u8; 16];
        segname.pwrite(self.segname, 0).unwrap();
        Section {
            sectname,
            segname,
            addr: self.addr as u64,
            size: self.size as u64,
            offset: self.offset as u32,
            align: self.align as u32,
            // FIXME, client needs to set after all offsets known
            reloff: 0,
            nreloc: 0,
            flags: 2147484672
        }
    }
}

type ArtifactCode<'a> = Vec<Definition<'a>>;
type ArtifactData<'a> = Vec<Definition<'a>>;

type StrTableIndex = usize;
type StrTable = DefaultStringInterner;
type Symbols = OrderMap<StrTableIndex, SymbolBuilder>;
type Relocations = Vec<Vec<RelocationInfo>>;

/// A mach object symbol table
#[derive(Debug, Default)]
pub struct SymbolTable {
    symbols: Symbols,
    strtable: StrTable,
    indexes: OrderMap<StrTableIndex, SymbolIndex>,
    strtable_size: StrtableOffset,
}

/// The kind of symbol this is
pub enum SymbolType {
    /// Which `section` this is defined in, and at what `offset`
    Defined { section: SectionIndex, offset: usize, global: bool },
    /// An undefined symbol (an import)
    Undefined,
}

impl SymbolTable {
    /// Create a new symbol table. The first strtable entry (like ELF) is always nothing
    pub fn new() -> Self {
        let mut strtable = StrTable::default();
        strtable.get_or_intern("");
        let strtable_size = 1;
        SymbolTable {
            symbols: Symbols::new(),
            strtable,
            strtable_size,
            indexes: OrderMap::new(),
        }
    }
    /// The number of symbols in this table
    pub fn len(&self) -> usize {
        self.symbols.len()
    }
    /// Returns size of the string table, in bytes
    pub fn sizeof_strtable(&self) -> usize {
        self.strtable_size
    }
    /// Lookup this symbols offset in the segment
    pub fn offset(&self, symbol_name: &str) -> Option<usize> {
        self.strtable.get(symbol_name)
         .and_then(|idx| self.symbols.get(&idx))
         .and_then(|sym| Some(sym.get_offset()))
    }
    /// Lookup this symbols ordinal index in the symbol table, if it has one
    pub fn index(&self, symbol_name: &str) -> Option<SymbolIndex> {
         self.strtable.get(symbol_name)
         .and_then(|idx| self.indexes.get(&idx).cloned())
    }
    /// Insert a new symbol into this objects symbol table
    pub fn insert(&mut self, symbol_name: &str, kind: SymbolType) {
        // mach-o requires _ prefixes on every symbol, we will allow this to be configurable later
        //let name = format!("_{}", symbol_name);
        let name = symbol_name;
        // 1 for null terminator and 1 for _ prefix (defered until write time);
        let name_len = name.len() + 1 + 1;
        let last_index = self.strtable.len();
        let name_index = self.strtable.get_or_intern(name);
        debug!("{}: {} <= {}", symbol_name, last_index, name_index);
        // the string is new: NB: relies on name indexes incrementing in sequence, starting at 0
        if name_index == last_index {
            debug!("Inserting new symbol: {}", self.strtable.resolve(name_index).unwrap());
            // TODO: add code offset into symbol n_value
            let builder = match kind {
                SymbolType::Undefined => SymbolBuilder::new(self.strtable_size).global(true).import(),
                SymbolType::Defined { section, offset, global } => SymbolBuilder::new(self.strtable_size).global(global).offset(offset). section(section)
            };
            // insert the builder for this symbol, using its strtab index
            self.symbols.insert(name_index, builder);
            // now create the symbols index, and using strtab name as lookup
            self.indexes.insert(name_index, self.symbols.len() - 1);
            // NB do not move this, otherwise all offsets will be off
            self.strtable_size += name_len;
        }
    }
}

#[derive(Debug)]
/// A Mach-o program segment
pub struct SegmentBuilder {
    /// The sections that belong to this program segment; currently only 2 (text + data)
    pub sections: [SectionBuilder; SegmentBuilder::NSECTIONS],
    /// A stupid offset value I need to refactor out
    pub offset: usize,
    size: usize,
}

impl SegmentBuilder {
    pub const NSECTIONS: usize = 2;
    /// The size of this segment's _data_, in bytes
    pub fn size(&self) -> usize {
        self.size
    }
    /// The size of this segment's _load command_, including its associated sections, in bytes
    pub fn load_command_size(ctx: &Ctx) -> usize {
        Segment::size_with(&ctx) + (Self::NSECTIONS * Section::size_with(&ctx))
    }
    fn _section_data_file_offset(ctx: &Ctx) -> usize {
        // section data
        Header::size_with(&ctx.container) + Self::load_command_size(ctx)
    }
    fn build_section(symtab: &mut SymbolTable, sectname: &'static str, segname: &'static str, offset: &mut usize, addr: &mut usize, symbol_offset: &mut usize, section: SectionIndex, definitions: &[Definition]) -> SectionBuilder {
        let mut local_size = 0;
        for def in definitions {
            local_size += def.data.len();
            symtab.insert(def.name, SymbolType::Defined { section, offset: *symbol_offset, global: def.prop.global });
            *symbol_offset += def.data.len();
        }
        let section = SectionBuilder::new(sectname, segname, local_size).offset(*offset).addr(*addr);
        *offset += local_size;
        *addr += local_size;
        section
    }
    /// Create a new program segment from an `artifact`, symbol table, and context
    // FIXME: this is pub(crate) for now because we can't leak pub(crate) Definition
    pub(crate) fn new(artifact: &Artifact, code: &[Definition], data: &[Definition], symtab: &mut SymbolTable, ctx: &Ctx) -> Self {
        let mut offset = Header::size_with(&ctx.container);
        let mut size = 0;
        let mut symbol_offset = 0;
        let text = Self::build_section(symtab, "__text", "__TEXT", &mut offset, &mut size, &mut symbol_offset, CODE_SECTION_INDEX, &code);
        let data = Self::build_section(symtab, "__data", "__DATA", &mut offset, &mut size, &mut symbol_offset, DATA_SECTION_INDEX, &data);
        for (ref import, _) in artifact.imports() {
            symtab.insert(import, SymbolType::Undefined);
        }
        // FIXME re add assert
        //assert_eq!(offset, Header::size_with(&ctx.container) + Self::load_command_size(ctx));
        debug!("Segment Size: {} Symtable LoadCommand Offset: {}", size, offset);
        let sections = [text, data];
        SegmentBuilder {
            size,
            sections,
            offset,
        }
    }
}

/// A Mach-o object file container
#[derive(Debug)]
pub struct Mach<'a> {
    ctx: Ctx,
    target: Target,
    symtab: SymbolTable,
    segment: SegmentBuilder,
    relocations: Relocations,
    code: ArtifactCode<'a>,
    data: ArtifactData<'a>,
    _p: ::std::marker::PhantomData<&'a ()>,
}

impl<'a> Mach<'a> {
    pub fn new(artifact: &'a Artifact) -> Self {
        let target = artifact.target.clone();
        let ctx = Ctx::from(target);
        // FIXME: I believe we can avoid this partition by refactoring SegmentBuilder::new
        let (code, data): (Vec<_>, Vec<_>) = artifact.definitions().partition(|def| def.prop.function);

        let mut symtab = SymbolTable::new();
        let segment = SegmentBuilder::new(&artifact, &code, &data, &mut symtab, &ctx);
        let relocations = build_relocations(&artifact, &symtab);

        Mach {
            ctx,
            target,
            symtab,
            segment,
            relocations,
            _p: ::std::marker::PhantomData::default(),
            code,
            data,
        }
    }
    fn header(&self, sizeofcmds: usize) -> Header {
        let mut header = Header::new(&self.ctx);
        header.filetype = MH_OBJECT;
        // safe to divide up the sections into sub-sections via symbols for dead code stripping
        header.flags = MH_SUBSECTIONS_VIA_SYMBOLS;
        header.cputype = CpuType::from(self.target).0;
        header.cpusubtype = 3;
        header.ncmds = 2;
        header.sizeofcmds = sizeofcmds as u32;
        header
    }
    pub fn write<T: Write + Seek>(self, file: T) -> Result<(), Error> {
        let mut file = BufWriter::new(file);
        // FIXME: this is ugly af, need cmdsize to get symtable offset
        // construct symtab command
        let mut symtab_load_command = SymtabCommand::new();
        let segment_load_command_size = SegmentBuilder::load_command_size(&self.ctx);
        let sizeof_load_commands = segment_load_command_size + symtab_load_command.cmdsize as usize;
        let symtable_offset = self.segment.offset + sizeof_load_commands;
        let strtable_offset = symtable_offset + (self.symtab.len() * Nlist::size_with(&self.ctx));
        let relocation_offset_start = strtable_offset + self.symtab.sizeof_strtable();
        let first_section_offset = Header::size_with(&self.ctx) + sizeof_load_commands;
        // start with setting the headers dependent value
        let header = self.header(sizeof_load_commands);
        
        debug!("Symtable: {:#?}", self.symtab);
        // marshall the sections into something we can actually write
        let mut raw_sections = Cursor::new(Vec::<u8>::new());
        let mut relocation_offset = relocation_offset_start;
        let mut section_offset = first_section_offset;
        for (idx, section) in self.segment.sections.into_iter().cloned().enumerate() {
            let mut section: Section = section.create();
            section.offset = section_offset as u32;
            section_offset += section.size as usize;
            debug!("{}: Setting nrelocs", idx);
            // relocations are tied to segment/sections
            // TODO: move this also into SegmentBuilder
            if idx < self.relocations.len() {
                let nrelocs = self.relocations[idx].len();
                section.nreloc = nrelocs as _;
                section.reloff = relocation_offset as u32;
                relocation_offset += nrelocs * SIZEOF_RELOCATION_INFO;
            }
            debug!("Section: {:#?}", section);
            raw_sections.iowrite_with(section, self.ctx)?;
        }
        let raw_sections = raw_sections.into_inner();
        debug!("Raw sections len: {} - Section start: {} Strtable size: {} - Segment size: {}", raw_sections.len(), first_section_offset, self.symtab.sizeof_strtable(), self.segment.size());

        let mut segment_load_command = Segment::new(self.ctx, &raw_sections);
        segment_load_command.nsects = self.segment.sections.len() as u32;
        // FIXME: de-magic number these
        segment_load_command.initprot = 7;
        segment_load_command.maxprot = 7;
        segment_load_command.filesize = self.segment.size() as u64;
        segment_load_command.vmsize = segment_load_command.filesize;
        segment_load_command.fileoff = first_section_offset as u64;
        debug!("Segment: {:#?}", segment_load_command);

        debug!("Symtable Offset: {:#?}", symtable_offset);
        assert_eq!(symtable_offset, self.segment.offset + segment_load_command.cmdsize as usize + symtab_load_command.cmdsize as usize);
        symtab_load_command.nsyms = self.symtab.len() as u32;
        symtab_load_command.symoff = symtable_offset as u32;
        symtab_load_command.stroff = strtable_offset as u32;
        symtab_load_command.strsize = self.symtab.sizeof_strtable() as u32;

        debug!("Symtab Load command: {:#?}", symtab_load_command);

        //////////////////////////////
        // write header
        //////////////////////////////
        file.iowrite_with(header, self.ctx)?;
        debug!("SEEK: after header: {}", file.seek(Current(0))?);

        //////////////////////////////
        // write load commands
        //////////////////////////////
        file.iowrite_with(segment_load_command, self.ctx)?;
        file.write(&raw_sections)?;
        file.iowrite_with(symtab_load_command, self.ctx.le)?;
        debug!("SEEK: after load commands: {}", file.seek(Current(0))?);

        //////////////////////////////
        // write code
        //////////////////////////////
        for code in self.code {
            file.write(code.data)?;
        }
        debug!("SEEK: after code: {}", file.seek(Current(0))?);

        //////////////////////////////
        // write data
        //////////////////////////////
        for data in self.data {
            file.write(data.data)?;
        }
        debug!("SEEK: after data: {}", file.seek(Current(0))?);

        //////////////////////////////
        // write symtable
        //////////////////////////////
        for (idx, symbol) in self.symtab.symbols.into_iter() {
            let symbol = symbol.create();
            debug!("{}: {:?}", idx, symbol);
            file.iowrite_with(symbol, self.ctx)?;
        }
        debug!("SEEK: after symtable: {}", file.seek(Current(0))?);

        //////////////////////////////
        // write strtable
        //////////////////////////////
        // we need to write first, empty element - but without an underscore
        file.iowrite(0u8)?;
        for (idx, string) in self.symtab.strtable.into_iter().skip(1) {
            debug!("{}: {:?}", idx, string);
            // yup, an underscore
            file.iowrite(0x5fu8)?;
            file.write(string.as_bytes())?;
            file.iowrite(0u8)?;
        }
        debug!("SEEK: after strtable: {}", file.seek(Current(0))?);

        //////////////////////////////
        // write relocations
        //////////////////////////////
        for section_relocations in self.relocations.into_iter() {
            debug!("Relocations: {}", section_relocations.len());
            for reloc in section_relocations.into_iter() {
                debug!("  {:?}", reloc);
                file.iowrite_with(reloc, self.ctx.le)?;
            }
        }
        debug!("SEEK: after relocations: {}", file.seek(Current(0))?);

        file.iowrite(0u8)?;

        Ok(())
    }
}

fn build_relocations(artifact: &Artifact, symtab: &SymbolTable) -> Relocations {
    use goblin::mach::relocation::{X86_64_RELOC_BRANCH, X86_64_RELOC_SIGNED, X86_64_RELOC_GOT_LOAD};
    let mut text_relocations = Vec::new();
    debug!("Generating relocations");
    for link in artifact.links() {
        debug!("Import links for: from {} to {} at {:#x} with {:?}", link.from.name, link.to.name, link.at, link.to.decl);
        let reloc = match link.to.decl {
            &Decl::Function {..} => X86_64_RELOC_BRANCH,
            &Decl::Data {..} => X86_64_RELOC_SIGNED,
            &Decl::CString {..} => X86_64_RELOC_SIGNED,
            &Decl::FunctionImport => X86_64_RELOC_BRANCH,
            &Decl::DataImport => X86_64_RELOC_GOT_LOAD,
        };
        match (symtab.offset(link.from.name), symtab.index(link.to.name)) {
            (Some(base_offset), Some(to_symbol_index)) => {
                debug!("{} offset: {}", link.to.name, base_offset + link.at);
                let reloc = RelocationBuilder::new(to_symbol_index, base_offset + link.at, reloc).create();
                text_relocations.push(reloc);
            },
            _ => error!("Import Relocation from {} to {} at {:#x} has a missing symbol. Dumping symtab {:?}", link.from.name, link.to.name, link.at, symtab)
        }
    }
    vec![text_relocations]
}

impl<'a> Object for Mach<'a> {
    fn to_bytes(artifact: &Artifact) -> Result<Vec<u8>, Error> {
        let mach = Mach::new(&artifact);
        let mut buffer = Cursor::new(Vec::new());
        mach.write(&mut buffer)?;
        Ok(buffer.into_inner())
    }
}
