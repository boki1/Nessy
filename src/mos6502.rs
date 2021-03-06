use crate::mos6502::InterruptKind::Irq;
use crate::mos6502_addressing_modes::*;
use crate::mos6502_instruction_set::*;

use getset::{CopyGetters, Getters, MutGetters, Setters};
use std::cell::RefCell;
use std::fmt::Display;
use std::fs::File;
use std::io::prelude::*;
use std::rc::Rc;
use std::{fmt, fmt::Debug};

pub type Address = u16;
pub type Word = u16;
pub type Opcode = u8;
pub type Byte = u8;

pub type AddressingModeFn = fn(&mut Cpu) -> Result<AddressingOutput, CpuError>;
pub type InstructionFn = fn(&mut Cpu) -> Result<(), CpuError>;

/// This structure represents the registers each MOS 6502 has.
/// They include one 8-bit accumulator register (A), two 8-bit
/// index registers (X and Y), 7 1-bit processor status flag
/// bits (P), an 8-bit stack pointer (S), and a 16-bit program
/// counter (PC).
///
/// The status register (P) 7 holds the following data:
/// ---- ----\
/// NVss DIZC\
/// |||| ||||\
/// |||| |||+- Carry\
/// |||| ||+-- Zero\
/// |||| |+--- Interrupt Disable\
/// |||| +---- Decimal\
/// ||++------ No CPU effect, see: the B flag\
/// |+-------- Overflow\
/// +--------- Negative
///

/// **bit!** and **bit_setter!** - Generate a getter and a setter for a concrete bit of a field.
///
/// Example:
/// ```
///
/// impl RegisterSet {
///     #[inline]
///     fn carry(&self) -> bool {
///         (self.status & (1 << 7)) > 0
///     }
///
///     #[inline]
///     fn set_carry(&self, value: bool) {
///         self.status = (self.status & !(1 << 7)) | ((value as u8) << 7);
///     }
/// }
/// ```

macro_rules! bit_setter {
    ($n: expr, $name: ident) => {
        #[inline]
        pub(crate) fn $name(&mut self, value: bool) {
            self.status = (self.status & !(1 << $n)) | ((value as u8) << $n);
        }
    };
}

macro_rules! bit {
    ($n: expr, $name: ident) => {
        #[inline]
        pub(crate) fn $name(&self) -> bool {
            (self.status & (1 << $n)) != 0
        }
    };
}

#[derive(Debug, Copy, Clone, PartialEq, Getters, CopyGetters, Setters, MutGetters)]
#[getset(get_copy = "pub", set = "pub", get_mut = "pub")]
pub struct RegisterSet {
    accumulator: u8,
    x_index: u8,
    y_index: u8,
    stk_ptr: u8,
    prog_counter: u16,
    status: u8,
}

impl RegisterSet {
    pub(crate) fn new() -> Self {
        Self {
            accumulator: 0,
            x_index: 0,
            y_index: 0,
            stk_ptr: 0xfd,
            prog_counter: 0,
            status: 0x24,
        }
    }

    pub(crate) fn new_custompc(custom_prog_counter: Address) -> Self {
        Self {
            prog_counter: custom_prog_counter,
            ..Self::new()
        }
    }

    bit!(0, carry);
    bit!(1, zero);
    bit!(2, irq_disabled);
    bit!(3, decimal_mode);
    bit!(4, brk);
    bit!(5, unused);
    bit!(6, overflowed);
    bit!(7, negative);

    bit_setter!(0, set_carry);
    bit_setter!(1, set_zero);
    bit_setter!(2, set_irq_disabled);
    bit_setter!(3, set_decimal_mode);
    bit_setter!(4, set_brk);
    bit_setter!(5, set_unused);
    bit_setter!(6, set_overflowed);
    bit_setter!(7, set_negative);
}

impl Default for RegisterSet {
    fn default() -> Self {
        RegisterSet::new()
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Getters, CopyGetters, Setters, MutGetters, Default)]
#[getset(get_copy = "pub", set = "pub", get_mut = "pub")]
pub struct Timings {
    elapsed: u64,
    residual: u8,
}

impl Timings {
    fn next(&mut self) {
        *self.residual_mut() -= 1;
        *self.elapsed_mut() += 1;
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Getters, CopyGetters, Setters, MutGetters, Default)]
#[getset(get_copy = "pub", set = "pub", get_mut = "pub")]
pub struct InterruptHandling {
    pending_nmi: bool,
    pending_irq: bool,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum InterruptKind {
    Nmi,
    Irq,
}

/// **Jump vectors**
/// In any of this exception cases - NMI, IRQ, BRK or a RESET, the PC should jump to the
/// concrete address, also called _vector_.
/// There is no mistake in the values of `IRQ_VECTOR` and `BRK_VECTOR` - they
/// both point to 0xfffe.
pub(crate) const NMI_VECTOR: Address = 0xfffa;
pub(crate) const RESET_VECTOR: Address = 0xfffc;
pub(crate) const IRQ_VECTOR: Address = 0xfffe;
pub(crate) const BRK_VECTOR: Address = 0xfffe;

///
/// Cpu
///
/// The struct representation of the MOS 6502.
///
#[derive(Getters, CopyGetters, Setters, MutGetters)]
pub struct Cpu {
    /// **regset**
    /// The set of register that the cpu has
    // #[derive(Getters, CopyGetters, Setters, MutGetters)]
    #[getset(get_copy = "pub", get_mut = "pub")]
    regset: RegisterSet,

    /// **time**
    /// Information about the "timings" of
    /// this cpu instance. Stores information
    /// about the total elapsed clock cycles and
    /// also about the cycles remaining of the
    /// current instruction which is executing.
    #[getset(get_copy = "pub", get_mut = "pub")]
    time: Timings,

    /// **inter**
    /// Information about handling any interrupt
    /// "requests" which are pending to cpu
    /// instance.
    inter: InterruptHandling,

    /// **bus_conn**
    /// This cpu's connection to the "outer world".
    /// This field stores a mutable reference to a
    /// struct which has the trait CommunicationInterface,
    /// a.k.a can be read from and written to. This
    /// interface is stored as optional "shared pointer"
    /// using the interior mutability pattern.
    bus_conn: Option<Rc<RefCell<dyn CommunicationInterface>>>,

    /// **i**
    /// This field stores the current instruction which
    /// is being executed. It should be stored directly
    /// from the `cycle_clock()` function and not
    /// modified until `self.time.residual` is not
    /// decremented to 0. This is "residual time" is
    /// used in order to emulate clock cycle accuracy.
    /// Note the part with "emulate" and not "achieve".
    /// The current implementation is not clock cycle
    /// accurate.
    i: Option<Instruction>,
}

///
/// CpuError
/// \
/// The enum describes the variation of errors which might occur during emulation.\
/// \
/// **BusInterfaceMissing** - The cpu has tried to read or write from its
/// associated memory but hasn't found one.\
/// **CurrentInstructionMissing** - The cpu has tried to access the current
/// intstruction field expecting to value correct value bot there has not
/// been one.\
/// **ExptectedOperandMissing** - An addressing mode which expects more than
/// the given operands has been used.\
/// **FailedLoadingProgram** - While reading the input file an error has occures.\
/// **BadAddressing** - This error occures either when addressing or when addressing
/// is exptected and it has not happened.
///
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CpuError {
    BusInterfaceMissing,
    CurrentInstructionMissing,
    ExpectedOperandMissing,
    FailedLoadingProgram,
    BadAddressing,
}

impl Cpu {
    /// Acts as post-fix increment operation (pc++):
    /// Increments the Program Counter and returns the **old** value.
    #[inline]
    pub(crate) fn inc_pc(&mut self) -> Word {
        let old_pc = self.regset.prog_counter;
        self.regset.prog_counter = old_pc.wrapping_add(1);
        old_pc
    }

    pub fn pc(&self) -> Word {
        self.regset.prog_counter
    }

    pub(crate) fn interrupt_handles(&self) -> &InterruptHandling {
        &self.inter
    }

    pub(crate) fn i(&self) -> Option<&Instruction> {
        self.i.as_ref()
    }

    pub(crate) fn i_mut(&mut self) -> Option<&mut Instruction> {
        self.i.as_mut()
    }

    pub(crate) fn i_decode_and_set(&mut self, opc: Opcode, optional_operand: Operand) {
        self.i = Some(Instruction::decode_by(opc));
        self.i.as_mut().unwrap().operand = optional_operand;
    }
}

impl std::fmt::Debug for Cpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cpu")
            .field("regset", &self.regset)
            .field("time", &self.time)
            .field("inter", &self.inter)
            .field("is_attached", &self.bus_conn.is_some())
            .field("curr_i", &self.i)
            .finish()
    }
}

impl Cpu {
    /// **new()** - Creates a new instance of a cpu with its default
    /// field values
    pub fn new() -> Self {
        Self {
            regset: RegisterSet::new(),
            time: Timings {
                elapsed: 0,
                residual: 0,
            },
            inter: InterruptHandling {
                pending_irq: false,
                pending_nmi: false,
            },
            bus_conn: None,
            i: None,
        }
    }

    /// **new_connected()** - Creates a new instance of a cpu with a bus interface
    /// supported
    pub(crate) fn new_connected(bus_conn: Option<Rc<RefCell<dyn CommunicationInterface>>>) -> Self {
        Self {
            bus_conn,
            ..Cpu::new()
        }
    }

    pub(crate) fn new_custompc(custom_prog_counter: Address) -> Self {
        Self {
            regset: RegisterSet::new_custompc(custom_prog_counter),
            ..Cpu::new()
        }
    }

    /// **full_instruction()** - Execute one full instruction
    pub fn full_instruction(&mut self) {
        self.clock_cycle();
        while self.time.residual != 0 {
            self.clock_cycle();
        }
    }

    /// **clock_cycle()** - Perform a single cpu cycle
    ///
    /// This implementation is not cycle correct, but
    /// successfully emulates being so. This is done
    /// by keeping the amount of cycles which have to
    /// skipped/wasted after each actual instruction
    /// execution.
    pub fn clock_cycle(&mut self) {
        if self.time.residual() == 0 {
            let opcode = self.fetch();

            self.i = Some(Instruction::decode_by(opcode));
            self.time.residual = self.i.as_ref().unwrap().time;
            load_operand_curr_i(self);

            let address = self.i.as_ref().unwrap().amode_fun;
            if let Ok(amode_output) = address(self) {
                self.i.as_mut().unwrap().amode_output = amode_output;
            } else {
                panic!("Failed addressing");
            }

            let execute = self.i.as_ref().unwrap().fun;
            if let Err(_) = execute(self) {
                panic!("Failed executing");
            }
        }

        self.time_mut().next();
    }

    /// **inthandle()** - Handles any interrupts of the cpu.
    /// The different kinds of intterrupts which the MOS 6502 supports
    /// are BRK (software interrupt), IRQ (interrupt request) and
    /// NMI (non-maskable interrupt).
    /// NMIs cannot be disabled.
    /// In order to allow IRQs, the flag `irq_disabled` in the status
    /// register has to be clear.
    pub fn inthandle(&mut self, int: InterruptKind) -> bool {
        if int == Irq && self.regset().irq_disabled() {
            return false;
        }

        let prog_counter = self.regset().prog_counter();
        self.stk_doublepush(prog_counter);

        self.regset_mut().set_brk(false);
        self.regset_mut().set_unused(true);
        self.regset_mut().set_irq_disabled(true);

        let status = self.regset().status();
        self.stk_push(status);

        let (next_address, time) = match int {
            InterruptKind::Nmi => (0xFFFA, 8),
            InterruptKind::Irq => (0xFFFE, 7),
        };

        let new_pc = self.read_word(next_address);
        *self.regset_mut().set_prog_counter(new_pc);
        *self.time_mut().residual_mut() = time;
        true
    }

    /// **reset()** - Performs a reset to the internal state of the cpu
    /// Among the flags in the status register, only the *UNUSED* flag is set.
    /// Reset takes some time, so 8 cycles are set to be remaining in the
    /// internal "clock" of the cpu
    pub fn reset(&mut self) {
        let mut regset = RegisterSet::new();
        regset.set_status(0x00);
        regset.set_prog_counter(0x8000);
        regset.set_unused(true);
        self.regset = regset;

        self.time = Timings {
            residual: 8,
            elapsed: 0,
        };
    }

    /// **connect()** - Connects the cpu to a bus, providing a context
    /// for read and write operations.
    pub(crate) fn connect_to(&mut self, conn: Rc<RefCell<dyn CommunicationInterface>>) {
        if let None = self.bus_conn {
            self.bus_conn = Some(conn);
        }
    }
}

impl Cpu {
    ///
    /// TODO FIXME:
    /// Consider returning Option<> or Result<> in order to
    /// give better return "code" to the called whether the
    /// value was actually 0 or an error occured. Same
    /// think goes for `writ_byte()` and the other wrapper
    /// functions.
    ///
    /// **read_byte()** - Initiates a read request to the interface
    /// **if one is present**
    pub fn read_byte(&self, address: Address) -> Byte {
        if let Some(bus) = &self.bus_conn {
            if let Some(data) = (*bus.borrow()).read(address) {
                return data;
            }
        }
        0
    }

    ///
    /// **writ_byte()** - Initiates a write request to the interface
    /// **if one is present**
    pub fn writ_byte(&self, address: Address, data: Byte) {
        if let Some(bus) = &self.bus_conn {
            return (*bus.borrow_mut()).write(address, data);
        }
    }

    ///
    /// **read_word()** - Wrapper function for reading two sequential
    /// bytes from the interface **if one is present**.
    pub fn read_word(&self, address: Address) -> Word {
        let lo = self.read_byte(address);
        let hi = self.read_byte(address + 1);
        Word::from_le_bytes([lo, hi])
    }

    ///
    /// **read_some()** - Reads sequence of bytes from the interface
    pub fn read_some(&self, address: Address, len: u16) -> Vec<Byte> {
        if let Some(bus) = &self.bus_conn {
            if let Some(result) = (*bus.borrow()).read_seq(address, len) {
                return result;
            }
        }

        vec![]
    }

    ///
    /// **fetch()** - Reads a byte from addressing the interface
    /// with the value of PC. After that the PC gets updated.
    #[inline]
    fn fetch(&mut self) -> Byte {
        let pc = self.inc_pc();
        self.read_byte(pc)
    }
}

impl Default for Cpu {
    fn default() -> Self {
        Cpu::new_connected(Some(Rc::new(RefCell::new(MainBus::new()))))
    }
}

pub trait CommunicationInterface {
    /// **read()** - Read the value of a given address from the interface
    fn read(&self, address: Address) -> Option<Byte>;

    /// **write()** - Write a value to a given address of the interface
    fn write(&mut self, address: Address, data: Byte);

    /// **read_seq()** - Read sequental from `address` to `address + len`
    /// (or less if the limit is exceeded)
    fn read_seq(&self, address: Address, len: u16) -> Option<Vec<Byte>>;
}

const RAM_SIZE: usize = 0xffff + 1;

/// The "host" of our cpu
/// Contains the contexual environment of the processor, most notably - memory.
pub struct MainBus {
    pub mem: Vec<Byte>,
}

impl MainBus {
    pub(crate) fn new() -> Self {
        Self {
            mem: vec![0x00; RAM_SIZE],
        }
    }
}

impl CommunicationInterface for MainBus {
    fn read(&self, address: Address) -> Option<Byte> {
        let addr = usize::from(address);
        if addr >= self.mem.len() {
            return None;
        }
        Some(self.mem[addr])
    }

    fn write(&mut self, address: Address, data: Byte) {
        let addr = usize::from(address);
        if addr >= self.mem.len() {
            return;
        }

        self.mem[addr] = data;
    }

    fn read_seq(&self, starting_address: Address, len: u16) -> Option<Vec<Byte>> {
        let mut result: Vec<Byte> = Vec::new();

        let limit: Address = starting_address + len;
        for address in starting_address..limit {
            if let Some(data) = self.read(address) {
                result.push(data);
            }
        }

        if result.len() > 0 {
            return Some(result);
        }
        None
    }
}

///
/// **make_instr!** - Makes a new Instruction instance with
/// given field values
///
#[macro_export]
macro_rules! make_instr {
    ($p_amode: ident, $p_fun: ident, $p_time: expr, $p_mnemonic: literal, $p_size: expr) => {
        Instruction {
            amode: $p_amode,
            amode_fun: to_fun($p_amode),
            fun: $p_fun,
            time: $p_time,
            mnemonic: String::from($p_mnemonic),
            size: $p_size,
            amode_output: AddressingOutput::NotExecuted,
            operand: None,
            loaded_from: 0x0000,
        }
    };
}

///
/// **make_illegal!** - Makes a new illegal instruction instance
/// with given field values
///
macro_rules! make_illegal {
    () => {
        unimplemented!();
    };
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum AddressingOutput {
    Fetched { value: Byte, address: Address },
    ValueOnly(Byte),
    AbsoluteAddress(Word),
    NotExecuted,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum AddressingMode {
    Imp,
    Imm,
    Zp0,
    Zpx,
    Zpy,
    Abs,
    Abx,
    Aby,
    Ind,
    Iny,
    Inx,
    Rel,
}

impl Display for AddressingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

///
/// Instruction
///
/// This structure represents a single operatation in
/// the 6502 cpu. It contains meta information about
/// the instruction such as its addressing mode and
/// its time to execute and some information about
/// its surroundings. The context contains information
/// such as the operand used and its final result
/// which might be a value (Fetched) or a address
/// (Absolute Address).
///
pub struct Instruction {
    ///
    /// Meta information
    amode_fun: AddressingModeFn,
    amode: AddressingMode,
    fun: InstructionFn,
    time: u8,
    mnemonic: String,
    size: u16,

    /// Instruction context
    ///
    /// **operand** contains the value of the
    /// operands this instruction is using. They
    /// might be either zero or one element.\
    /// \
    /// It is modified **before** the function
    /// `amode` and used in it in order to produce
    /// `amode_output`.
    operand: Option<Word>,

    /// **amode_output** - The "output" of
    /// the process of addressing this instruction.
    /// It might be either an address or a value
    /// fetched from memory. \
    /// \
    /// It is modified **only from the function `amode`.
    amode_output: AddressingOutput,

    /// **loaded_from** - The address where the first byte of this
    /// instruction is located in memory
    loaded_from: Address,
}

impl Instruction {
    pub(crate) fn amode_output(&self) -> AddressingOutput {
        self.amode_output
    }

    pub(crate) fn set_amode_output(&mut self, amode_output: AddressingOutput) {
        self.amode_output = amode_output;
    }

    pub(crate) fn amode(&self) -> AddressingMode {
        self.amode
    }

    pub(crate) fn operand(&self) -> Operand {
        self.operand
    }

    pub(crate) fn set_operand(&mut self, operand: Operand) {
        self.operand = operand;
    }

    pub(crate) fn loaded_from(&self) -> Address {
        self.loaded_from
    }

    pub(crate) fn mnemonic(&self) -> String {
        self.mnemonic.clone()
    }

    pub(crate) fn size(&self) -> u16 {
        self.size
    }
    pub(crate) fn time(&self) -> u8 {
        self.time
    }

    pub(crate) fn fun(&self) -> InstructionFn {
        self.fun
    }

    pub(crate) fn amode_fun(&self) -> AddressingModeFn {
        self.amode_fun
    }
}

impl Display for Instruction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        use AddressingMode::*;

        let details = match self.amode {
            Imm => ("#", ""),
            Imp | Zp0 | Abs | Rel => ("", ""),
            Ind => ("(", ")"),
            Abx | Zpx => ("", ", X"),
            Aby | Zpy => ("", ", Y"),
            Iny => ("(", "), Y"),
            Inx => ("(", ", X)"),
        };

        let address = format!("{:#6x?}\t", self.loaded_from);
        let operand = match self.operand {
            Some(num) => format!("{:#4x?}", num),
            None => String::new(),
        };
        let addressing_mode = format!("\t; {}", self.amode);

        write!(
            f,
            "{}",
            format!(
                "{address}{mnemonic}\t{prefix}{operand}{suffix}{addressing_mode}\n",
                address = address,
                mnemonic = self.mnemonic,
                prefix = details.0,
                operand = operand,
                suffix = details.1,
                addressing_mode = addressing_mode
            )
        )
    }
}

impl Debug for Instruction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        f.debug_struct("Instruction")
            .field("time", &self.time)
            .field("mnemonic", &self.mnemonic)
            .field("size", &self.size)
            .finish()
    }
}

impl PartialEq for Instruction {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time
            && self.mnemonic == other.mnemonic
            && self.size == other.size
            && self.fun as usize == other.fun as usize
            && self.amode_fun as usize == other.amode_fun as usize
    }
}

impl Clone for Instruction {
    fn clone(&self) -> Self {
        Self {
            amode_fun: self.amode_fun,
            amode: self.amode,
            fun: self.fun,
            time: self.time,
            mnemonic: self.mnemonic.clone(),
            size: self.size,
            operand: self.operand.clone(),
            amode_output: self.amode_output,
            loaded_from: self.loaded_from,
        }
    }
}

pub type Operand = Option<Word>;
pub type InstructionMeta = (Operand, AddressingOutput);

impl Instruction {
    ///
    /// **decode_by** - Match a given opcode value to its corresponding
    /// instruction.
    ///
    /// The table is filled according to [this](https://www.masswerk.at/6502/6502_instruction_set.html) resource.
    ///
    /// **NB:** Illegal opcodes are not supported as of now
    ///
    pub(crate) fn decode_by(opcode: Byte) -> Instruction {
        // use crate::mos6502_addressing_modes::*;
        // use crate::mos6502_intruction_set::*;
        use AddressingMode::*;

        return match opcode {
            // opcode => make_instr! (
            //              addr_mode,
            //              instruction,
            //              cycles,
            //              instruction_name,
            //              size
            //          )
            0x00 => make_instr!(Imp, brk, 7, "brk", 1),
            0x01 => make_instr!(Inx, ora, 6, "ora", 2),
            0x05 => make_instr!(Zp0, ora, 3, "ora", 2),
            0x06 => make_instr!(Zp0, asl, 5, "asl", 2),
            0x08 => make_instr!(Imp, php, 3, "php", 1),
            0x09 => make_instr!(Imm, ora, 2, "ora", 2),
            0x0A => make_instr!(Imp, asl, 2, "asl", 1),
            0x0D => make_instr!(Abs, ora, 4, "ora", 3),
            0x0E => make_instr!(Abs, asl, 6, "asl", 3),

            0x10 => make_instr!(Rel, bpl, 2 /* or 3 */, "bpl", 2),
            0x11 => make_instr!(Iny, ora, 5, "ora", 2),
            0x15 => make_instr!(Zpx, ora, 4, "ora", 2),
            0x16 => make_instr!(Zpx, asl, 6, "asl", 2),
            0x18 => make_instr!(Imp, clc, 2, "clc", 1),
            0x19 => make_instr!(Aby, ora, 4, "ora", 3),
            0x1D => make_instr!(Abx, ora, 4, "ora", 3),
            0x1E => make_instr!(Abx, asl, 7, "asl", 3),

            0x20 => make_instr!(Abs, jsr, 6, "jsr", 3),
            0x21 => make_instr!(Inx, and, 6, "and", 2),
            0x24 => make_instr!(Zp0, bit, 3, "bit", 2),
            0x25 => make_instr!(Zp0, and, 3, "and", 2),
            0x26 => make_instr!(Zp0, rol, 5, "rol", 2),
            0x28 => make_instr!(Imp, plp, 4, "plp", 1),
            0x29 => make_instr!(Imm, and, 2, "and", 2),
            0x2A => make_instr!(Imp, rol, 2, "rol", 1),
            0x2C => make_instr!(Abs, bit, 4, "bit", 3),
            0x2D => make_instr!(Abs, and, 4, "and", 3),
            0x2E => make_instr!(Abs, rol, 6, "rol", 3),

            0x30 => make_instr!(Rel, bmi, 2 /* or 3 */, "bmi", 2),
            0x31 => make_instr!(Iny, and, 5, "and", 2),
            0x35 => make_instr!(Zpx, and, 4, "and", 2),
            0x36 => make_instr!(Zpx, rol, 6, "rol", 2),
            0x38 => make_instr!(Imp, sec, 2, "sec", 1),
            0x39 => make_instr!(Aby, and, 4, "and", 3),
            0x3D => make_instr!(Abx, and, 4, "and", 3),
            0x3E => make_instr!(Abx, rol, 7, "rol", 3),

            0x40 => make_instr!(Imp, rti, 6, "rti", 1),
            0x41 => make_instr!(Inx, eor, 6, "eor", 2),
            0x45 => make_instr!(Zp0, eor, 3, "eor", 2),
            0x46 => make_instr!(Zp0, lsr, 5, "lsr", 2),
            0x48 => make_instr!(Imp, pha, 3, "pha", 1),
            0x49 => make_instr!(Imm, eor, 2, "eor", 2),
            0x4A => make_instr!(Imp, lsr, 2, "lsr", 1),
            0x4C => make_instr!(Abs, jmp, 3, "jmp", 3),
            0x4D => make_instr!(Abs, eor, 4, "eor", 3),
            0x4E => make_instr!(Abs, lsr, 6, "lsr", 3),

            0x50 => make_instr!(Rel, bvc, 2 /* or 3 */, "bvc", 2),
            0x51 => make_instr!(Iny, eor, 5, "eor", 2),
            0x55 => make_instr!(Zpx, eor, 4, "eor", 2),
            0x56 => make_instr!(Zpx, lsr, 6, "lsr", 2),
            0x58 => make_instr!(Imp, cli, 2, "cli", 1),
            0x59 => make_instr!(Aby, eor, 4, "eor", 3),
            0x5D => make_instr!(Abx, eor, 4, "eor", 3),
            0x5E => make_instr!(Abx, lsr, 7, "lsr", 3),

            0x60 => make_instr!(Imp, rts, 6, "rts", 1),
            0x61 => make_instr!(Inx, adc, 6, "adc", 2),
            0x65 => make_instr!(Zp0, adc, 3, "adc", 2),
            0x66 => make_instr!(Zp0, ror, 5, "ror", 2),
            0x68 => make_instr!(Imp, pla, 4, "pla", 1),
            0x69 => make_instr!(Imm, adc, 2, "adc", 2),
            0x6A => make_instr!(Imp, ror, 2, "ror", 1),
            0x6C => make_instr!(Ind, jmp, 5, "jmp", 3),
            0x6D => make_instr!(Abs, adc, 4, "adc", 3),
            0x6E => make_instr!(Abx, ror, 7, "ror", 3),

            0x70 => make_instr!(Rel, bvs, 2 /* or 3 */, "bvs", 2),
            0x71 => make_instr!(Iny, adc, 5, "adc", 2),
            0x75 => make_instr!(Zpx, adc, 4, "adc", 2),
            0x76 => make_instr!(Zpx, ror, 6, "ror", 2),
            0x78 => make_instr!(Imp, sei, 2, "sei", 1),
            0x79 => make_instr!(Aby, adc, 4, "adc", 3),
            0x7D => make_instr!(Abx, adc, 4, "adc", 3),
            0x7E => make_instr!(Abs, ror, 6, "ror", 6),

            0x81 => make_instr!(Inx, sta, 6, "sta", 2),
            0x84 => make_instr!(Zp0, sty, 3, "sty", 2),
            0x85 => make_instr!(Zp0, sta, 3, "sta", 2),
            0x86 => make_instr!(Zp0, stx, 3, "stx", 2),
            0x88 => make_instr!(Imp, dey, 2, "dey", 1),
            0x8A => make_instr!(Imp, txa, 2, "txa", 1),
            0x8C => make_instr!(Abs, sty, 4, "sty", 3),
            0x8D => make_instr!(Abs, sta, 4, "sta", 3),
            0x8E => make_instr!(Abs, stx, 4, "stx", 3),

            0x90 => make_instr!(Rel, bcc, 2 /* or 3 */, "bcc", 2),
            0x91 => make_instr!(Iny, sta, 6, "sta", 2),
            0x94 => make_instr!(Zpx, sty, 4, "sty", 2),
            0x95 => make_instr!(Zpx, sta, 4, "sta", 2),
            0x96 => make_instr!(Zpy, stx, 4, "stx", 2),
            0x98 => make_instr!(Imp, tya, 2, "tya", 1),
            0x99 => make_instr!(Aby, sta, 5, "sta", 3),
            0x9A => make_instr!(Imp, txs, 2, "txs", 1),
            0x9D => make_instr!(Abx, sta, 5, "sta", 3),

            0xA0 => make_instr!(Imm, ldy, 2, "ldy", 2),
            0xA1 => make_instr!(Inx, lda, 6, "lda", 2),
            0xA2 => make_instr!(Imm, ldx, 2, "ldx", 2),
            0xA4 => make_instr!(Zp0, ldy, 3, "ldy", 2),
            0xA5 => make_instr!(Zp0, lda, 3, "lda", 2),
            0xA6 => make_instr!(Zp0, ldx, 3, "lda", 2),
            0xA8 => make_instr!(Imp, tay, 2, "tay", 1),
            0xA9 => make_instr!(Imm, lda, 2, "lda", 2),
            0xAA => make_instr!(Imp, tax, 2, "tax", 1),
            0xAC => make_instr!(Abs, ldy, 4, "ldy", 3),
            0xAD => make_instr!(Abs, lda, 4, "lda", 3),
            0xAE => make_instr!(Abs, ldx, 4, "ldx", 3),

            0xB0 => make_instr!(Rel, bcs, 2 /* or 3 */, "bcs", 2),
            0xB1 => make_instr!(Iny, lda, 5, "lda", 2),
            0xB4 => make_instr!(Zpx, ldy, 4, "ldy", 2),
            0xB5 => make_instr!(Zpx, lda, 4, "lda", 2),
            0xB6 => make_instr!(Zpy, ldx, 4, "ldx", 2),
            0xB8 => make_instr!(Imp, clv, 2, "clv", 1),
            0xB9 => make_instr!(Aby, lda, 4, "lda", 3),
            0xBA => make_instr!(Imp, tsx, 2, "tsx", 1),
            0xBC => make_instr!(Abx, ldy, 4, "ldy", 3),
            0xBD => make_instr!(Abx, lda, 4, "lda", 3),
            0xBE => make_instr!(Aby, ldx, 4, "ldx", 3),

            0xC0 => make_instr!(Imm, cpy, 2, "cpy", 2),
            0xC1 => make_instr!(Inx, cmp, 6, "cmp", 2),
            0xC4 => make_instr!(Zp0, cpy, 3, "cpy", 2),
            0xC5 => make_instr!(Zp0, cmp, 3, "cmp", 2),
            0xC6 => make_instr!(Zp0, dec, 5, "dec", 2),
            0xC8 => make_instr!(Imp, iny, 2, "iny", 1),
            0xC9 => make_instr!(Imm, cmp, 2, "cmp", 2),
            0xCA => make_instr!(Imp, dex, 2, "dex", 1),
            0xCC => make_instr!(Abs, cpy, 4, "cpy", 3),
            0xCD => make_instr!(Abs, cmp, 4, "cmp", 3),
            0xCE => make_instr!(Abs, dec, 6, "dec", 3),

            0xD0 => make_instr!(Rel, bne, 2 /* or 3 */, "bne", 2),
            0xD1 => make_instr!(Iny, cmp, 5, "cmp", 2),
            0xD5 => make_instr!(Zpx, cmp, 4, "cmp", 2),
            0xD6 => make_instr!(Zpx, dec, 6, "dec", 2),
            0xD8 => make_instr!(Imp, cld, 2, "cld", 1),
            0xD9 => make_instr!(Aby, cmp, 4, "cmp", 3),
            0xDD => make_instr!(Abx, cmp, 4, "cmp", 3),
            0xDE => make_instr!(Abx, dec, 7, "dec", 3),

            0xE0 => make_instr!(Imm, cpx, 2, "cpx", 2),
            0xE1 => make_instr!(Inx, sbc, 6, "sbc", 2),
            0xE4 => make_instr!(Zp0, cpx, 3, "cpx", 2),
            0xE5 => make_instr!(Zp0, sbc, 3, "sbc", 2),
            0xE6 => make_instr!(Zp0, inc, 5, "inc", 2),
            0xE8 => make_instr!(Imp, inx, 2, "inx", 1),
            0xE9 => make_instr!(Imm, sbc, 2, "sbc", 2),
            0xEA => make_instr!(Imp, nop, 2, "nop", 1),
            0xEC => make_instr!(Abs, cpx, 4, "cpx", 3),
            0xED => make_instr!(Abs, sbc, 4, "sbc", 3),
            0xEE => make_instr!(Abs, inc, 6, "inc", 3),

            0xF0 => make_instr!(Rel, beq, 2 /* or 3 */, "beq", 2),
            0xF1 => make_instr!(Iny, sbc, 5, "sbc", 2),
            0xF5 => make_instr!(Zpx, sbc, 4, "sbc", 2),
            0xF6 => make_instr!(Zpx, inc, 6, "inc", 2),
            0xF8 => make_instr!(Imp, sed, 2, "sed", 1),
            0xF9 => make_instr!(Aby, sbc, 4, "sbc", 3),
            0xFD => make_instr!(Abx, sbc, 4, "sbc", 3),
            0xFE => make_instr!(Abx, inc, 7, "inc", 3),

            _ => make_illegal!(),
        };
    }

    pub fn load_address(&self) -> Address {
        self.loaded_from
    }
}

///
/// Utilities
///
/// Some additional functions needed for the cpu
///
#[derive(Debug, PartialEq, Default)]
pub struct Asm {
    code: Vec<Instruction>,
}

impl Asm {
    pub fn new(code: Vec<Instruction>) -> Self {
        Self { code }
    }

    pub fn from_addr_range(cpu: &mut Cpu, begin_address: Address, limit: u16) -> Asm {
        let mut code: Vec<Instruction> = Vec::new();

        let end_address = begin_address + limit;
        let mut address = begin_address;
        while address < end_address {
            let opcode = cpu.read_byte(address);
            let mut i = Instruction::decode_by(opcode);
            load_operand(cpu, &mut i, address);
            address += i.size;
            code.push(i);
        }

        Asm { code }
    }

    pub fn code(&self) -> &Vec<Instruction> {
        &self.code
    }

    pub fn stringify_range(
        cpu: &mut Cpu,
        begin_address: Address,
        limit: u16,
    ) -> Result<String, ()> {
        let asm = Asm::from_addr_range(cpu, begin_address, limit);
        asm.stringify(true, true)
    }

    pub fn stringify(&self, address_column: bool, addressing_mode: bool) -> Result<String, ()> {
        let mut res = String::new();
        for i in self.code.iter() {
            res += &i.to_string();
        }

        if res.len() > 0 {
            return Ok(res);
        }

        Err(())
    }
}

const STACK_OFFSET: Address = 0x100;

impl Cpu {
    fn stk_ptr_inc(&mut self) -> u8 {
        let sptr = self.regset_mut().stk_ptr_mut();
        *sptr = sptr.wrapping_add(1);
        *sptr
    }

    fn stk_ptr_dec(&mut self) -> u8 {
        let stk_ptr = self.regset().stk_ptr();
        *self.regset_mut().stk_ptr_mut() = stk_ptr.wrapping_sub(1);
        stk_ptr
    }

    /// **stk_push()** - Pushes a byte to the stack stored in memory with offset `STACK_OFFSET`.
    /// Note that this routine will fail if no interface is connected.
    pub(crate) fn stk_push(&mut self, data: Byte) {
        let stk_ptr = self.stk_ptr_dec();
        let addr = STACK_OFFSET + Address::from(stk_ptr);
        self.writ_byte(addr, data);
    }

    pub(crate) fn stk_doublepush(&mut self, data: Word) {
        self.stk_push((data >> 8) as u8);
        self.stk_push((data) as u8);
    }

    /// **stk_pop()** - Pops a byte from the stack stored in memory with offset `STACK_OFFSET`.
    /// **NB:** This routine will fail if no 2 passed; 0 failinterface is connected.
    pub(crate) fn stk_pop(&mut self) -> Byte {
        let stk_ptr = self.stk_ptr_inc();
        let addr = STACK_OFFSET + Address::from(stk_ptr);
        let data = self.read_byte(addr);
        data
    }

    /// **disassemble()** - Given a beginning address, disassemble `limit` of bytes from memory
    /// matching them to Instruction instances.
    pub fn disassemble(&mut self, begin: Address, limit: Address) -> Option<Asm> {
        if self.bus_conn.is_none() {
            return None;
        }

        let asm = Asm::from_addr_range(self, begin, limit);
        if asm.code.len() > 0 {
            return Some(asm);
        }

        None
    }

    pub fn print_disassembly(&mut self, begin: Address, limit: Address) {
        if let Some(disassembly) = self.disassemble(begin, limit) {
            for i in disassembly.code.iter() {}
        }
    }

    /// **load_program()** - Given a vector of bytes, store `limit` of them into memory
    /// starting from `begin` in memory.
    pub fn load_program(
        &mut self,
        program: &Vec<Byte>,
        begin: Address,
        limit: usize,
        start_it: bool,
    ) -> Result<Address, CpuError> {
        if self.bus_conn.is_none() {
            return Err(CpuError::BusInterfaceMissing);
        }

        let end = begin + limit as u16;
        for address in begin..end {
            let index = usize::from(address - begin);
            self.writ_byte(address, program[index]);
        }

        let saved_pc = self.pc();
        if start_it {
            *self.regset_mut().prog_counter_mut() = begin;
        }

        Ok(saved_pc)
    }

    /// **load_file()**  - Given a filename of a binary source file, load the data in memory
    /// starting from address `begin`. If `start_it` is true, the program counter should
    /// be set to `begin` and execute the code.
    pub fn load_file(
        &mut self,
        filename: &str,
        begin: Address,
        start_it: bool,
    ) -> Result<Address, CpuError> {
        let mut program: Vec<Byte> = Vec::new();
        if let Ok(mut file) = File::open(filename) {
            if let Ok(_) = file.read_to_end(&mut program) {
                return self.load_program(&program, begin, program.len(), start_it);
            }
        }

        Err(CpuError::FailedLoadingProgram)
    }
}

/// **load_operand_curr_i()** - This is called right before
/// the addressing mode specifics are executed in order
/// to fetch the required operand into the operand
/// field in `i`.
pub fn load_operand_curr_i(cpu: &mut Cpu) {
    use AddressingMode::*;

    if cpu.i.is_none() {
        return;
    }

    // The instruction is already loaded since we are looking at it
    // The reason we are updating this value here and not beforehand
    // is sa that a support could be provided for `load_operand()`
    // also a.k.a so that we can load operands for instructions
    // which are not the current one whici is being executed.
    let loaded_from: Address = cpu.pc() - 1;

    let num_fetched = match cpu.i.as_ref().unwrap().amode {
        Imp => 0,
        Imm | Zp0 | Zpx | Zpy | Inx | Iny | Rel => 1,
        Abs | Abx | Aby | Ind => 2,
    };

    let operand = match num_fetched {
        0 => None,
        1 => Some(Word::from(cpu.fetch())),
        2 => {
            let lo = cpu.fetch();
            let hi = cpu.fetch();
            Some(Word::from_le_bytes([lo, hi]))
        }
        _ => unreachable!("Unknown number of bytes for operand"),
    };

    let i = cpu.i.as_mut().unwrap();
    i.loaded_from = loaded_from;
    i.operand = operand;
}

/// **load_operand()** - For any given instruction (only the addressing mode
/// is actually of importance here, fetch any operands that the instruction
/// requires taking into account that the address of the instruction in memory
/// is also provided.
pub fn load_operand(cpu: &mut Cpu, i: &mut Instruction, address: Address) {
    // Store previous state
    let saved_pc = cpu.pc();
    // The instruction has already been fetched
    // so if address is where the instruction is
    // then address + 1 is where the operand is.
    *cpu.regset_mut().prog_counter_mut() = address.wrapping_add(1);

    let saved_i = cpu.i.clone();
    cpu.i.replace(i.clone());

    load_operand_curr_i(cpu);

    i.clone_from(&cpu.i.as_ref().unwrap());

    // Restore previous state
    cpu.i.clone_from(&saved_i);
    *cpu.regset_mut().prog_counter_mut() = saved_pc;
}
