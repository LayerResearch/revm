mod call_helpers;

pub use call_helpers::{calc_call_gas, get_memory_input_and_out_ranges, resize_memory};

use crate::{
    gas::{self, EOF_CREATE_GAS, MIN_CALLEE_GAS},
    instructions::utility::IntoAddress,
    interpreter::Interpreter,
    interpreter_action::FrameInput,
    interpreter_types::{
        EofContainer, Immediates, InputsTr, InterpreterTypes, Jumps, LoopControl, MemoryTr,
        ReturnData, RuntimeFlag, StackTr,
    },
    CallInput, CallInputs, CallScheme, CallValue, CreateInputs, EOFCreateInputs, Host,
    InstructionResult, InterpreterAction, InterpreterResult,
};
use bytecode::eof::{Eof, EofHeader};
use context_interface::CreateScheme;
use core::cmp::max;
use primitives::{eof::new_eof_address, hardfork::SpecId, Address, Bytes, B256, U256};
use std::boxed::Box;

use crate::InstructionContext;

/// EOF Create instruction
pub fn eofcreate<WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    require_eof!(context.interpreter);
    require_non_staticcall!(context.interpreter);
    gas!(context.interpreter, EOF_CREATE_GAS);
    let initcontainer_index = context.interpreter.bytecode.read_u8();

    popn!([salt, input_offset, input_size, value], context.interpreter);

    let container = context
        .interpreter
        .bytecode
        .eof_container(initcontainer_index as usize)
        .expect("valid container")
        .clone();

    // Resize memory and get return range.
    let Some(input_range) = resize_memory(context.interpreter, input_offset, input_size) else {
        return;
    };

    let input = if !input_range.is_empty() {
        context
            .interpreter
            .memory
            .slice(input_range)
            .to_vec()
            .into()
    } else {
        Bytes::new()
    };

    let eof = Eof::decode(container.clone()).expect("Subcontainer is verified");

    if !eof.body.is_data_filled {
        // Should be always false as it is verified by eof verification.
        panic!("Panic if data section is not full");
    }

    // Calculate new address
    let created_address = new_eof_address(
        context.interpreter.input.target_address(),
        salt.to_be_bytes().into(),
    );

    let gas_limit = context.interpreter.control.gas().remaining_63_of_64_parts();
    gas!(context.interpreter, gas_limit);

    // Send container for execution as all deployed containers are preverified to be valid EOF.
    context.interpreter.control.set_next_action(
        InterpreterAction::NewFrame(FrameInput::EOFCreate(Box::new(
            EOFCreateInputs::new_opcode(
                context.interpreter.input.target_address(),
                created_address,
                value,
                eof,
                gas_limit,
                CallInput::Bytes(input),
            ),
        ))),
        InstructionResult::CallOrCreate,
    );

    // jump over initcontainer index.
    context.interpreter.bytecode.relative_jump(1);
}

/// Instruction to create a new EOF contract from a transaction initcode.
pub fn txcreate<WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    // TODO(EOF) only accepted in EOF.
    require_eof!(context.interpreter);
    require_non_staticcall!(context.interpreter);
    gas!(context.interpreter, EOF_CREATE_GAS);

    // pop tx_initcode_hash, salt, input_offset, input_size, value from the operand stack
    popn!(
        [tx_initcode_hash, salt, input_offset, input_size, value],
        context.interpreter
    );
    let tx_initcode_hash = B256::from(tx_initcode_hash);

    // perform (and charge for) memory expansion using [input_offset, input_size]
    let Some(input_range) = resize_memory(context.interpreter, input_offset, input_size) else {
        return;
    };

    // Get validated initcode with all its subcontainers validated recursively.
    let Some(initcode) = context.host.initcode_by_hash(tx_initcode_hash) else {
        // If initcode is not found or not valid, push 0 on the stack.
        push!(context.interpreter, U256::ZERO);
        return;
    };

    // caller’s memory slice [input_offset:input_size] is used as calldata
    let input = if !input_range.is_empty() {
        context
            .interpreter
            .memory
            .slice(input_range)
            .to_vec()
            .into()
    } else {
        Bytes::new()
    };

    // Decode initcode as EOF.
    let eof = Eof::decode(initcode).expect("Subcontainer is verified");

    // Calculate new address
    let created_address = new_eof_address(
        context.interpreter.input.target_address(),
        salt.to_be_bytes().into(),
    );

    let gas_limit = context.interpreter.control.gas().remaining_63_of_64_parts();
    gas!(context.interpreter, gas_limit);

    // Send container for execution as all deployed containers are preverified to be valid EOF.
    context.interpreter.control.set_next_action(
        InterpreterAction::NewFrame(FrameInput::EOFCreate(Box::new(
            EOFCreateInputs::new_opcode(
                context.interpreter.input.target_address(),
                created_address,
                value,
                eof,
                gas_limit,
                CallInput::Bytes(input),
            ),
        ))),
        InstructionResult::CallOrCreate,
    );
}

pub fn return_contract<H: Host + ?Sized>(
    context: InstructionContext<'_, H, impl InterpreterTypes>,
) {
    if !context.interpreter.runtime_flag.is_eof_init() {
        context
            .interpreter
            .control
            .set_instruction_result(InstructionResult::ReturnContractInNotInitEOF);
        return;
    }
    let deploy_container_index = context.interpreter.bytecode.read_u8();
    popn!([aux_data_offset, aux_data_size], context.interpreter);
    let aux_data_size = as_usize_or_fail!(context.interpreter, aux_data_size);
    let container = context
        .interpreter
        .bytecode
        .eof_container(deploy_container_index as usize)
        .expect("valid container")
        .clone();

    // Convert to EOF so we can check data section size.
    let (eof_header, _) = EofHeader::decode(&container).expect("valid EOF header");

    let static_aux_size = eof_header.eof_size() - container.len();

    // Important: Offset must be ignored if len is zeros
    let mut output = if aux_data_size != 0 {
        let aux_data_offset = as_usize_or_fail!(context.interpreter, aux_data_offset);
        resize_memory!(context.interpreter, aux_data_offset, aux_data_size);

        let aux_slice = context
            .interpreter
            .memory
            .slice_len(aux_data_offset, aux_data_size);

        [&container, aux_slice.as_ref()].concat()
    } else {
        container.to_vec()
    };

    // `data_size - static_aux_size` give us current data `container` size.
    // And with `aux_slice` len we can calculate new data size.
    let new_data_size = eof_header.data_size as usize - static_aux_size + aux_data_size;
    if new_data_size > 0xFFFF {
        // Aux data is too big
        context
            .interpreter
            .control
            .set_instruction_result(InstructionResult::EofAuxDataOverflow);
        return;
    }
    if new_data_size < eof_header.data_size as usize {
        // Aux data is too small
        context
            .interpreter
            .control
            .set_instruction_result(InstructionResult::EofAuxDataTooSmall);
        return;
    }
    let new_data_size = (new_data_size as u16).to_be_bytes();

    // Set new data size in eof bytes as we know exact index.
    output[eof_header.data_size_raw_i()..][..2].clone_from_slice(&new_data_size);
    let output: Bytes = output.into();

    let result = InstructionResult::ReturnContract;
    let gas = *context.interpreter.control.gas();
    context.interpreter.control.set_next_action(
        crate::InterpreterAction::Return {
            result: InterpreterResult {
                output,
                gas,
                result,
            },
        },
        result,
    );
}

pub fn extcall_input(interpreter: &mut Interpreter<impl InterpreterTypes>) -> Option<Bytes> {
    popn!([input_offset, input_size], interpreter, None);
    let return_memory_offset = resize_memory(interpreter, input_offset, input_size)?;

    if return_memory_offset.is_empty() {
        return Some(Bytes::new());
    }

    Some(Bytes::copy_from_slice(
        interpreter.memory.slice(return_memory_offset).as_ref(),
    ))
}

#[inline]
pub fn extcall_gas_calc<WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: &mut InstructionContext<'_, H, WIRE>,
    target: Address,
    transfers_value: bool,
) -> Option<u64> {
    let Some(account_load) = context.host.load_account_delegated(target) else {
        context
            .interpreter
            .control
            .set_instruction_result(InstructionResult::FatalExternalError);
        return None;
    };

    // account_load.is_empty will be accounted if there is transfer value
    // Berlin can be hardcoded as extcall came after berlin.
    let call_cost = gas::call_cost(
        context.interpreter.runtime_flag.spec_id(),
        transfers_value,
        account_load,
    );
    gas!(context.interpreter, call_cost, None);

    // Calculate the gas available to callee as caller’s
    // remaining gas reduced by max(ceil(gas/64), MIN_RETAINED_GAS) (MIN_RETAINED_GAS is 5000).
    let gas_reduce = max(context.interpreter.control.gas().remaining() / 64, 5000);
    let gas_limit = context
        .interpreter
        .control
        .gas()
        .remaining()
        .saturating_sub(gas_reduce);

    // The MIN_CALLEE_GAS rule is a replacement for stipend:
    // it simplifies the reasoning about the gas costs and is
    // applied uniformly for all introduced EXT*CALL instructions.
    //
    // If Gas available to callee is less than MIN_CALLEE_GAS trigger light failure (Same as Revert).
    if gas_limit < MIN_CALLEE_GAS {
        // Push 1 to stack to indicate that call light failed.
        // It is safe to ignore stack overflow error as we already popped multiple values from stack.
        let _ = context.interpreter.stack.push(U256::from(1));
        context.interpreter.return_data.clear();
        // Return none to continue execution.
        return None;
    }

    gas!(context.interpreter, gas_limit, None);
    Some(gas_limit)
}

/// Pop target address from stack and check if it is valid.
///
/// Valid address has first 12 bytes as zeroes.
#[inline]
pub fn pop_extcall_target_address(
    interpreter: &mut Interpreter<impl InterpreterTypes>,
) -> Option<Address> {
    popn!([target_address], interpreter, None);
    let target_address = B256::from(target_address);
    // Check if target is left padded with zeroes.
    if target_address[..12].iter().any(|i| *i != 0) {
        interpreter
            .control
            .set_instruction_result(InstructionResult::InvalidEXTCALLTarget);
        return None;
    }
    // Discard first 12 bytes.
    Some(Address::from_word(target_address))
}

pub fn extcall<WIRE: InterpreterTypes, H: Host + ?Sized>(
    mut context: InstructionContext<'_, H, WIRE>,
) {
    require_eof!(context.interpreter);

    // Pop target address
    let Some(target_address) = pop_extcall_target_address(context.interpreter) else {
        return;
    };

    // Input call
    let Some(input) = extcall_input(context.interpreter) else {
        return;
    };

    popn!([value], context.interpreter);
    let has_transfer = !value.is_zero();
    if context.interpreter.runtime_flag.is_static() && has_transfer {
        context
            .interpreter
            .control
            .set_instruction_result(InstructionResult::CallNotAllowedInsideStatic);
        return;
    }

    let Some(gas_limit) = extcall_gas_calc(&mut context, target_address, has_transfer) else {
        return;
    };

    // Call host to interact with target contract
    context.interpreter.control.set_next_action(
        InterpreterAction::NewFrame(FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(input),
            gas_limit,
            target_address,
            caller: context.interpreter.input.target_address(),
            bytecode_address: target_address,
            value: CallValue::Transfer(value),
            scheme: CallScheme::ExtCall,
            is_static: context.interpreter.runtime_flag.is_static(),
            is_eof: true,
            return_memory_offset: 0..0,
        }))),
        InstructionResult::CallOrCreate,
    );
}

pub fn extdelegatecall<WIRE: InterpreterTypes, H: Host + ?Sized>(
    mut context: InstructionContext<'_, H, WIRE>,
) {
    require_eof!(context.interpreter);

    // Pop target address
    let Some(target_address) = pop_extcall_target_address(context.interpreter) else {
        return;
    };

    // Input call
    let Some(input) = extcall_input(context.interpreter) else {
        return;
    };

    let Some(gas_limit) = extcall_gas_calc(&mut context, target_address, false) else {
        return;
    };

    // Call host to interact with target contract
    context.interpreter.control.set_next_action(
        InterpreterAction::NewFrame(FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(input),
            gas_limit,
            target_address: context.interpreter.input.target_address(),
            caller: context.interpreter.input.caller_address(),
            bytecode_address: target_address,
            value: CallValue::Apparent(context.interpreter.input.call_value()),
            scheme: CallScheme::ExtDelegateCall,
            is_static: context.interpreter.runtime_flag.is_static(),
            is_eof: true,
            return_memory_offset: 0..0,
        }))),
        InstructionResult::CallOrCreate,
    );
}

pub fn extstaticcall<WIRE: InterpreterTypes, H: Host + ?Sized>(
    mut context: InstructionContext<'_, H, WIRE>,
) {
    require_eof!(context.interpreter);

    // Pop target address
    let Some(target_address) = pop_extcall_target_address(context.interpreter) else {
        return;
    };

    // Input call
    let Some(input) = extcall_input(context.interpreter) else {
        return;
    };

    let Some(gas_limit) = extcall_gas_calc(&mut context, target_address, false) else {
        return;
    };

    // Call host to interact with target contract
    context.interpreter.control.set_next_action(
        InterpreterAction::NewFrame(FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(input),
            gas_limit,
            target_address,
            caller: context.interpreter.input.target_address(),
            bytecode_address: target_address,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::ExtStaticCall,
            is_static: true,
            is_eof: true,
            return_memory_offset: 0..0,
        }))),
        InstructionResult::CallOrCreate,
    );
}

pub fn create<WIRE: InterpreterTypes, const IS_CREATE2: bool, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    require_non_staticcall!(context.interpreter);

    // EIP-1014: Skinny CREATE2
    if IS_CREATE2 {
        check!(context.interpreter, PETERSBURG);
    }

    popn!([value, code_offset, len], context.interpreter);
    let len = as_usize_or_fail!(context.interpreter, len);

    let mut code = Bytes::new();
    if len != 0 {
        // EIP-3860: Limit and meter initcode
        if context
            .interpreter
            .runtime_flag
            .spec_id()
            .is_enabled_in(SpecId::SHANGHAI)
        {
            // Limit is set as double of max contract bytecode size
            if len > context.host.max_initcode_size() {
                context
                    .interpreter
                    .control
                    .set_instruction_result(InstructionResult::CreateInitCodeSizeLimit);
                return;
            }
            gas!(context.interpreter, gas::initcode_cost(len));
        }

        let code_offset = as_usize_or_fail!(context.interpreter, code_offset);
        resize_memory!(context.interpreter, code_offset, len);
        code = Bytes::copy_from_slice(
            context
                .interpreter
                .memory
                .slice_len(code_offset, len)
                .as_ref(),
        );
    }

    // EIP-1014: Skinny CREATE2
    let scheme = if IS_CREATE2 {
        popn!([salt], context.interpreter);
        // SAFETY: `len` is reasonable in size as gas for it is already deducted.
        gas_or_fail!(context.interpreter, gas::create2_cost(len));
        CreateScheme::Create2 { salt }
    } else {
        gas!(context.interpreter, gas::CREATE);
        CreateScheme::Create
    };

    let mut gas_limit = context.interpreter.control.gas().remaining();

    // EIP-150: Gas cost changes for IO-heavy operations
    if context
        .interpreter
        .runtime_flag
        .spec_id()
        .is_enabled_in(SpecId::TANGERINE)
    {
        // Take remaining gas and deduce l64 part of it.
        gas_limit -= gas_limit / 64
    }
    gas!(context.interpreter, gas_limit);

    // Call host to interact with target contract
    context.interpreter.control.set_next_action(
        InterpreterAction::NewFrame(FrameInput::Create(Box::new(CreateInputs {
            caller: context.interpreter.input.target_address(),
            scheme,
            value,
            init_code: code,
            gas_limit,
        }))),
        InstructionResult::CallOrCreate,
    );
}

pub fn call<WIRE: InterpreterTypes, H: Host + ?Sized>(context: InstructionContext<'_, H, WIRE>) {
    popn!([local_gas_limit, to, value], context.interpreter);
    let to = to.into_address();
    // Max gas limit is not possible in real ethereum situation.
    let local_gas_limit = u64::try_from(local_gas_limit).unwrap_or(u64::MAX);

    let has_transfer = !value.is_zero();
    if context.interpreter.runtime_flag.is_static() && has_transfer {
        context
            .interpreter
            .control
            .set_instruction_result(InstructionResult::CallNotAllowedInsideStatic);
        return;
    }

    let Some((input, return_memory_offset)) = get_memory_input_and_out_ranges(context.interpreter)
    else {
        return;
    };

    let Some(account_load) = context.host.load_account_delegated(to) else {
        context
            .interpreter
            .control
            .set_instruction_result(InstructionResult::FatalExternalError);
        return;
    };

    let Some(mut gas_limit) = calc_call_gas(
        context.interpreter,
        account_load,
        has_transfer,
        local_gas_limit,
    ) else {
        return;
    };

    gas!(context.interpreter, gas_limit);

    // Add call stipend if there is value to be transferred.
    if has_transfer {
        gas_limit = gas_limit.saturating_add(gas::CALL_STIPEND);
    }

    // Call host to interact with target contract
    context.interpreter.control.set_next_action(
        InterpreterAction::NewFrame(FrameInput::Call(Box::new(CallInputs {
            input: CallInput::SharedBuffer(input),
            gas_limit,
            target_address: to,
            caller: context.interpreter.input.target_address(),
            bytecode_address: to,
            value: CallValue::Transfer(value),
            scheme: CallScheme::Call,
            is_static: context.interpreter.runtime_flag.is_static(),
            is_eof: false,
            return_memory_offset,
        }))),
        InstructionResult::CallOrCreate,
    );
}

pub fn call_code<WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    popn!([local_gas_limit, to, value], context.interpreter);
    let to = Address::from_word(B256::from(to));
    // Max gas limit is not possible in real ethereum situation.
    let local_gas_limit = u64::try_from(local_gas_limit).unwrap_or(u64::MAX);

    //pop!(context.interpreter, value);
    let Some((input, return_memory_offset)) = get_memory_input_and_out_ranges(context.interpreter)
    else {
        return;
    };

    let Some(mut load) = context.host.load_account_delegated(to) else {
        context
            .interpreter
            .control
            .set_instruction_result(InstructionResult::FatalExternalError);
        return;
    };

    // Set `is_empty` to false as we are not creating this account.
    load.is_empty = false;
    let Some(mut gas_limit) =
        calc_call_gas(context.interpreter, load, !value.is_zero(), local_gas_limit)
    else {
        return;
    };

    gas!(context.interpreter, gas_limit);

    // Add call stipend if there is value to be transferred.
    if !value.is_zero() {
        gas_limit = gas_limit.saturating_add(gas::CALL_STIPEND);
    }

    // Call host to interact with target contract
    context.interpreter.control.set_next_action(
        InterpreterAction::NewFrame(FrameInput::Call(Box::new(CallInputs {
            input: CallInput::SharedBuffer(input),
            gas_limit,
            target_address: context.interpreter.input.target_address(),
            caller: context.interpreter.input.target_address(),
            bytecode_address: to,
            value: CallValue::Transfer(value),
            scheme: CallScheme::CallCode,
            is_static: context.interpreter.runtime_flag.is_static(),
            is_eof: false,
            return_memory_offset,
        }))),
        InstructionResult::CallOrCreate,
    );
}

pub fn delegate_call<WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    check!(context.interpreter, HOMESTEAD);
    popn!([local_gas_limit, to], context.interpreter);
    let to = Address::from_word(B256::from(to));
    // Max gas limit is not possible in real ethereum situation.
    let local_gas_limit = u64::try_from(local_gas_limit).unwrap_or(u64::MAX);

    let Some((input, return_memory_offset)) = get_memory_input_and_out_ranges(context.interpreter)
    else {
        return;
    };

    let Some(mut load) = context.host.load_account_delegated(to) else {
        context
            .interpreter
            .control
            .set_instruction_result(InstructionResult::FatalExternalError);
        return;
    };

    // Set is_empty to false as we are not creating this account.
    load.is_empty = false;
    let Some(gas_limit) = calc_call_gas(context.interpreter, load, false, local_gas_limit) else {
        return;
    };

    gas!(context.interpreter, gas_limit);

    // Call host to interact with target contract
    context.interpreter.control.set_next_action(
        InterpreterAction::NewFrame(FrameInput::Call(Box::new(CallInputs {
            input: CallInput::SharedBuffer(input),
            gas_limit,
            target_address: context.interpreter.input.target_address(),
            caller: context.interpreter.input.caller_address(),
            bytecode_address: to,
            value: CallValue::Apparent(context.interpreter.input.call_value()),
            scheme: CallScheme::DelegateCall,
            is_static: context.interpreter.runtime_flag.is_static(),
            is_eof: false,
            return_memory_offset,
        }))),
        InstructionResult::CallOrCreate,
    );
}

pub fn static_call<WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    check!(context.interpreter, BYZANTIUM);
    popn!([local_gas_limit, to], context.interpreter);
    let to = Address::from_word(B256::from(to));
    // Max gas limit is not possible in real ethereum situation.
    let local_gas_limit = u64::try_from(local_gas_limit).unwrap_or(u64::MAX);

    let Some((input, return_memory_offset)) = get_memory_input_and_out_ranges(context.interpreter)
    else {
        return;
    };

    let Some(mut load) = context.host.load_account_delegated(to) else {
        context
            .interpreter
            .control
            .set_instruction_result(InstructionResult::FatalExternalError);
        return;
    };
    // Set `is_empty` to false as we are not creating this account.
    load.is_empty = false;
    let Some(gas_limit) = calc_call_gas(context.interpreter, load, false, local_gas_limit) else {
        return;
    };
    gas!(context.interpreter, gas_limit);

    // Call host to interact with target contract
    context.interpreter.control.set_next_action(
        InterpreterAction::NewFrame(FrameInput::Call(Box::new(CallInputs {
            input: CallInput::SharedBuffer(input),
            gas_limit,
            target_address: to,
            caller: context.interpreter.input.target_address(),
            bytecode_address: to,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::StaticCall,
            is_static: true,
            is_eof: false,
            return_memory_offset,
        }))),
        InstructionResult::CallOrCreate,
    );
}
