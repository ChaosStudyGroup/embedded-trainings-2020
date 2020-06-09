use core::{
    cmp,
    convert::{TryFrom, TryInto},
    mem,
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use std::{
    collections::{btree_map, BTreeMap},
    env, fs,
    io::{self, Write as _},
    path::Path,
    process,
    rc::Rc,
};

use anyhow::{anyhow, bail};
use arrayref::array_ref;
use gimli::{
    read::{CfaRule, DebugFrame, UnwindSection},
    BaseAddresses, EndianSlice, LittleEndian, RegisterRule, UninitializedUnwindContext,
};
use probe_rs::{
    flashing::{self, Format},
    Core, CoreRegisterAddress, Probe,
};
use probe_rs_rtt::{Rtt, ScanRegion};
use xmas_elf::{program::Type, sections::SectionData, symbol_table::Entry, ElfFile};

fn main() -> Result<(), anyhow::Error> {
    notmain().map(|code| process::exit(code))
}

fn notmain() -> Result<i32, anyhow::Error> {
    env_logger::init();

    let args = env::args().skip(1 /* program name */).collect::<Vec<_>>();

    if args.len() != 1 {
        bail!("expected exactly one argument")
    }

    let path = &args[0];

    let bytes = fs::read(path)?;
    let elf = ElfFile::new(&bytes).map_err(|s| anyhow!("{}", s))?;

    // sections used in cortex-m-rt
    // NOTE we won't load `.uninit` so it is not included here
    // NOTE we don't load `.bss` because the app (cortex-m-rt) will zero it
    let candidates = [".vector_table", ".text", ".rodata"];

    let text = elf
        .section_iter()
        .zip(0..)
        .filter_map(|(sect, shndx)| {
            if sect.get_name(&elf) == Ok(".text") {
                Some(shndx)
            } else {
                None
            }
        })
        .next();

    let mut debug_frame = None;
    let mut range_names = None;
    let mut rtt = None;
    let mut sections = vec![];
    let mut dotdata = None;
    let mut registers = None;
    for sect in elf.section_iter() {
        if let Ok(name) = sect.get_name(&elf) {
            if name == ".debug_frame" {
                debug_frame = Some(sect.raw_data(&elf));
                continue;
            }

            if name == ".symtab" {
                if let Ok(symtab) = sect.get_data(&elf) {
                    let (rn, rtt_) = range_names_from(&elf, symtab, text)?;
                    range_names = Some(rn);
                    rtt = rtt_;
                }
            }

            let size = sect.size();
            // skip empty sections
            if candidates.contains(&name) && size != 0 {
                let start = sect.address();
                if size % 4 != 0 || start % 4 != 0 {
                    // we could support unaligned sections but let's not do that now
                    bail!("section `{}` is not 4-byte aligned", name);
                }

                let start = start.try_into()?;
                let data = sect
                    .raw_data(&elf)
                    .chunks_exact(4)
                    .map(|chunk| u32::from_le_bytes(*array_ref!(chunk, 0, 4)))
                    .collect::<Vec<_>>();

                if name == ".vector_table" {
                    registers = Some(Registers {
                        vtor: start,
                        // Initial stack pointer
                        sp: data[0],
                        // Reset handler
                        pc: data[1],
                    });
                } else if name == ".data" {
                    // don't put .data in the `sections` variable; it is specially handled
                    dotdata = Some(Data {
                        phys: start,
                        virt: start,
                        data,
                    });
                    continue;
                }

                sections.push(Section { start, data });
            }
        }
    }

    if let Some(data) = dotdata.as_mut() {
        // patch up `.data` physical address
        let mut patched = false;
        for ph in elf.program_iter() {
            if ph.get_type() == Ok(Type::Load) {
                if u32::try_from(ph.virtual_addr())? == data.virt {
                    patched = true;
                    data.phys = ph.physical_addr().try_into()?;
                    break;
                }
            }
        }

        if !patched {
            bail!("couldn't extract `.data` physical address from the ELF");
        }
    }

    let registers = registers.ok_or_else(|| anyhow!("`.vector_table` section is missing"))?;

    let probes = Probe::list_all();
    if probes.is_empty() {
        // TODO improve error message
        bail!("nRF52840 Development Kit appears to not be connected")
    }
    log::debug!("found {} probes", probes.len());
    let probe = probes[0].open()?;
    log::info!("opened probe");
    let sess = probe.attach("nRF52840_xxAA")?;
    log::info!("started session");
    let core = sess.attach_to_core(0)?;
    log::info!("attached to core");

    core.reset_and_halt()?;
    log::info!("reset and halted the core");

    // load program into memory
    for section in &sections {
        core.write_32(section.start, &section.data)?;
    }
    if let Some(section) = dotdata {
        core.write_32(section.phys, &section.data)?;
    }

    // adjust registers
    // this is the link register reset value; it indicates the end of the call stack
    if registers.vtor >= 0x2000_0000 {
        // program lives in RAM
        core.write_core_reg(LR, LR_END)?;
        core.write_core_reg(SP, registers.sp)?;
        core.write_core_reg(PC, registers.pc)?;
        core.write_word_32(VTOR, registers.vtor)?;

        log::info!("loaded program into RAM");

        core.run()?;
    } else {
        // XXX the device may have already loaded SP and PC at this point in this case?

        // program lives in Flash
        flashing::download_file(&sess, Path::new(path), Format::Elf)?;

        log::info!("flashed program");

        core.reset()?;
    }

    // run

    let core = Rc::new(core);
    let mut rtt = Rtt::attach_region(
        core.clone(),
        &sess,
        &ScanRegion::Exact(rtt.ok_or_else(|| anyhow!("RTT control block not found"))?),
    )?;
    let channel = rtt
        .up_channels()
        .take(0)
        .ok_or_else(|| anyhow!("RTT up channel 0 not found"))?;

    static CONTINUE: AtomicBool = AtomicBool::new(true);

    ctrlc::set_handler(|| {
        CONTINUE.store(false, Ordering::Relaxed);
    })?;

    // wait for breakpoint
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut read_buf = [0; 1024];
    let mut was_halted = false;
    while CONTINUE.load(Ordering::Relaxed) {
        let n = channel.read(&mut read_buf)?;

        if n != 0 {
            stdout.write_all(&read_buf[..n])?;
        }

        let is_halted = core.core_halted()?;

        if is_halted && was_halted {
            break;
        }
        was_halted = is_halted;
    }
    drop(stdout);

    // Ctrl-C was pressed; stop the microcontroller
    if !CONTINUE.load(Ordering::Relaxed) {
        core.halt()?;
    }

    let pc = core.read_core_reg(PC)?;

    let debug_frame = debug_frame.ok_or_else(|| anyhow!("`.debug_frame` section not found"))?;

    let range_names = range_names.ok_or_else(|| anyhow!("`.symtab` section not found"))?;

    // print backtrace
    backtrace(&core, pc, debug_frame, &range_names)?;

    core.reset_and_halt()?;

    Ok(0)
}

fn backtrace(
    core: &Core,
    mut pc: u32,
    debug_frame: &[u8],
    range_names: &RangeNames,
) -> Result<(), anyhow::Error> {
    fn gimli2probe(reg: &gimli::Register) -> CoreRegisterAddress {
        CoreRegisterAddress(reg.0)
    }

    struct Registers<'c> {
        cache: BTreeMap<u16, u32>,
        core: &'c Core,
    }

    impl<'c> Registers<'c> {
        fn new(lr: u32, sp: u32, core: &'c Core) -> Self {
            let mut cache = BTreeMap::new();
            cache.insert(LR.0, lr);
            cache.insert(SP.0, sp);
            Self { cache, core }
        }

        fn get(&mut self, reg: CoreRegisterAddress) -> Result<u32, anyhow::Error> {
            Ok(match self.cache.entry(reg.0) {
                btree_map::Entry::Occupied(entry) => *entry.get(),
                btree_map::Entry::Vacant(entry) => *entry.insert(self.core.read_core_reg(reg)?),
            })
        }

        fn insert(&mut self, reg: CoreRegisterAddress, val: u32) {
            self.cache.insert(reg.0, val);
        }

        fn update_cfa(
            &mut self,
            rule: &CfaRule<EndianSlice<LittleEndian>>,
        ) -> Result</* cfa_changed: */ bool, anyhow::Error> {
            match rule {
                CfaRule::RegisterAndOffset { register, offset } => {
                    let cfa = (i64::from(self.get(gimli2probe(register))?) + offset) as u32;
                    let ok = self.cache.get(&SP.0) != Some(&cfa);
                    self.cache.insert(SP.0, cfa);
                    Ok(ok)
                }

                // NOTE not encountered in practice so far
                CfaRule::Expression(_) => todo!("CfaRule::Expression"),
            }
        }

        fn update(
            &mut self,
            reg: &gimli::Register,
            rule: &RegisterRule<EndianSlice<LittleEndian>>,
        ) -> Result<(), anyhow::Error> {
            match rule {
                RegisterRule::Undefined => unreachable!(),

                RegisterRule::Offset(offset) => {
                    let cfa = self.get(SP)?;
                    let addr = (i64::from(cfa) + offset) as u32;
                    self.cache.insert(reg.0, self.core.read_word_32(addr)?);
                }

                _ => unimplemented!(),
            }

            Ok(())
        }
    }

    let mut debug_frame = DebugFrame::new(debug_frame, LittleEndian);
    // 32-bit ARM -- this defaults to the host's address size which is likely going to be 8
    debug_frame.set_address_size(mem::size_of::<u32>() as u8);

    let sp = core.read_core_reg(SP)?;
    let lr = core.read_core_reg(LR)?;

    // statically linked binary -- there are no relative addresses
    let bases = &BaseAddresses::default();
    let ctx = &mut UninitializedUnwindContext::new();

    let mut frame = 0;
    let mut registers = Registers::new(lr, sp, core);
    println!("stack backtrace:");
    loop {
        println!(
            "{:>4}: {:#010x} - {}",
            frame,
            pc,
            range_names
                .binary_search_by(|rn| if rn.0.contains(&pc) {
                    cmp::Ordering::Equal
                } else if pc < rn.0.start {
                    cmp::Ordering::Greater
                } else {
                    cmp::Ordering::Less
                })
                .map(|idx| &*range_names[idx].1)
                .unwrap_or("<unknown>")
        );

        let fde = debug_frame.fde_for_address(bases, pc.into(), DebugFrame::cie_from_offset)?;
        let uwt_row = fde.unwind_info_for_address(&debug_frame, bases, ctx, pc.into())?;

        let cfa_changed = registers.update_cfa(uwt_row.cfa())?;

        for (reg, rule) in uwt_row.registers() {
            registers.update(reg, rule)?;
        }

        let lr = registers.get(LR)?;
        if lr == LR_END {
            break;
        }

        if !cfa_changed && lr == pc {
            println!("error: the stack appears to be corrupted beyond this point");
            return Ok(());
        }

        if lr > 0xffff_fff0 {
            println!("      <exception entry>");

            let sp = registers.get(SP)?;
            let stacked = Stacked::read(core, sp)?;

            registers.insert(LR, stacked.lr);
            // adjust the stack pointer for stacked registers
            registers.insert(SP, sp + mem::size_of::<Stacked>() as u32);
            pc = stacked.pc;
        } else {
            if lr & 1 == 0 {
                bail!("bug? LR ({:#010x}) didn't have the Thumb bit set", lr)
            }
            pc = lr & !1;
        }

        frame += 1;
    }

    Ok(())
}

/// Registers stacked on exception entry
// XXX assumes that the floating pointer registers are NOT stacked (which may not be the case for HF
// targets)
#[derive(Debug)]
struct Stacked {
    r0: u32,
    r1: u32,
    r2: u32,
    r3: u32,
    r12: u32,
    lr: u32,
    pc: u32,
    xpsr: u32,
}

impl Stacked {
    fn read(core: &Core, sp: u32) -> Result<Self, anyhow::Error> {
        let mut registers = [0; 8];
        core.read_32(sp, &mut registers)?;

        Ok(Stacked {
            r0: registers[0],
            r1: registers[1],
            r2: registers[2],
            r3: registers[3],
            r12: registers[4],
            lr: registers[5],
            pc: registers[6],
            xpsr: registers[7],
        })
    }
}
// FIXME this might already exist in the DWARF data; we should just use that
/// Map from PC ranges to demangled Rust names
type RangeNames = Vec<(Range<u32>, String)>;
type Shndx = u16;

fn range_names_from(
    elf: &ElfFile,
    sd: SectionData,
    text: Option<Shndx>,
) -> Result<(RangeNames, Option<u32>), anyhow::Error> {
    let mut range_names = vec![];
    let mut rtt = None;
    if let SectionData::SymbolTable32(entries) = sd {
        for entry in entries {
            if let Ok(name) = entry.get_name(elf) {
                if name == "_SEGGER_RTT" {
                    rtt = Some(entry.value() as u32);
                }

                if Some(entry.shndx()) == text && entry.size() != 0 {
                    let mut name = rustc_demangle::demangle(name).to_string();
                    // clear the thumb bit
                    let start = entry.value() as u32 & !1;

                    // strip the hash (e.g. `::hd881d91ced85c2b0`)
                    let hash_len = "::hd881d91ced85c2b0".len();
                    if let Some(pos) = name.len().checked_sub(hash_len) {
                        let maybe_hash = &name[pos..];
                        if maybe_hash.starts_with("::h") {
                            // FIXME avoid this allocation
                            name = name[..pos].to_string();
                        }
                    }

                    range_names.push((start..start + entry.size() as u32, name));
                }
            }
        }
    }

    range_names.sort_unstable_by(|a, b| a.0.start.cmp(&b.0.start));

    Ok((range_names, rtt))
}

const LR: CoreRegisterAddress = CoreRegisterAddress(14);
const PC: CoreRegisterAddress = CoreRegisterAddress(15);
const SP: CoreRegisterAddress = CoreRegisterAddress(13);
const VTOR: u32 = 0xE000_ED08;

const LR_END: u32 = 0xFFFF_FFFF;

/// ELF section to be loaded onto the target
#[derive(Debug)]
struct Section {
    start: u32,
    data: Vec<u32>,
}

/// The .data section has a physical address different that its virtual address; we want to load the
/// data into the physical address (which would normally be in FLASH) so that cortex-m-rt doesn't
/// corrupt the variables in .data during initialization -- it would be best if we could disable
/// cortex-m-rt's `init_data` call but there's no such feature
#[derive(Debug)]
struct Data {
    phys: u32,
    virt: u32,
    data: Vec<u32>,
}

/// Registers to update before running the program
struct Registers {
    sp: u32,
    pc: u32,
    vtor: u32,
}
