use iced_x86::{Decoder, DecoderOptions, FlowControl, Formatter, Instruction, MasmFormatter};

pub struct DecodedInstruction {
    pub length: usize,
    pub is_call: bool,
}

pub fn decode_one(bytes: &[u8], ip: u64) -> Option<DecodedInstruction> {
    let mut decoder = Decoder::with_ip(64, bytes, ip, DecoderOptions::NONE);
    if !decoder.can_decode() {
        return None;
    }
    let instruction = decoder.decode();
    let is_call = matches!(
        instruction.flow_control(),
        FlowControl::Call | FlowControl::IndirectCall
    );
    Some(DecodedInstruction {
        length: instruction.len(),
        is_call,
    })
}

pub fn disassemble(bytes: &[u8], base_address: u64, count: usize) -> Vec<String> {
    let mut decoder = Decoder::with_ip(64, bytes, base_address, DecoderOptions::NONE);
    let mut formatter = MasmFormatter::new();
    let mut instruction = Instruction::default();
    let mut output = String::new();
    let mut lines = Vec::new();

    while decoder.can_decode() && lines.len() < count {
        decoder.decode_out(&mut instruction);
        output.clear();
        formatter.format(&instruction, &mut output);
        lines.push(format!("{:#018x}: {}", instruction.ip(), output));
    }

    lines
}
