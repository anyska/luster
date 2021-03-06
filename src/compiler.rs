use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::mem;

use failure::Error;
use num_traits::cast;

use gc_arena::{Gc, MutationContext};

use function::{FunctionProto, UpValueDescriptor};
use opcode::{Constant, OpCode, Register, UpValueIndex, VarCount};
use operators::{apply_binop, BinaryOperator};
use parser::{
    AssignmentStatement, AssignmentTarget, Block, CallSuffix, Chunk, Expression,
    FunctionCallStatement, FunctionStatement, HeadExpression, LocalStatement, PrimaryExpression,
    ReturnStatement, SimpleExpression, Statement, SuffixedExpression,
};
use string::String;
use value::Value;

pub fn compile_chunk<'gc>(
    mc: MutationContext<'gc, '_>,
    chunk: &Chunk,
) -> Result<FunctionProto<'gc>, Error> {
    Compiler::compile(mc, &chunk)
}

#[derive(Fail, Debug)]
enum CompilerLimit {
    #[fail(display = "insufficient available registers")]
    Registers,
    #[fail(display = "too many upvalues")]
    UpValues,
    #[fail(display = "too many returns")]
    Returns,
    #[fail(display = "too many fixed parameters")]
    FixedParameters,
    #[fail(display = "too many inner functions")]
    Functions,
    #[fail(display = "too many constants")]
    Constants,
}

struct Compiler<'gc, 'a> {
    mutation_context: MutationContext<'gc, 'a>,
    functions: TopStack<CompilerFunction<'gc, 'a>>,
}

impl<'gc, 'a> Compiler<'gc, 'a> {
    fn compile(
        mc: MutationContext<'gc, '_>,
        chunk: &'a Chunk,
    ) -> Result<FunctionProto<'gc>, Error> {
        let mut compiler = Compiler {
            mutation_context: mc,
            functions: TopStack::new(CompilerFunction::default()),
        };

        compiler.block(&chunk.block)?;
        Ok(compiler.functions.top.to_proto(mc))
    }

    fn block(&mut self, block: &'a Block) -> Result<(), Error> {
        for statement in &block.statements {
            self.statement(statement)?;
        }

        if let Some(return_statement) = &block.return_statement {
            self.return_statement(return_statement)?;
        } else {
            self.functions.top.opcodes.push(OpCode::Return {
                start: 0,
                count: VarCount::make_zero(),
            });
        }

        Ok(())
    }

    fn statement(&mut self, statement: &'a Statement) -> Result<(), Error> {
        match statement {
            Statement::LocalStatement(local_statement) => {
                self.local_statement(local_statement)?;
            }
            Statement::LocalFunction(local_function) => {
                self.local_function(local_function)?;
            }
            Statement::FunctionCall(function_call) => {
                self.function_call(function_call)?;
            }
            Statement::Assignment(assignment) => {
                self.assignment(assignment)?;
            }
            _ => bail!("unsupported statement type"),
        }

        Ok(())
    }

    fn return_statement(&mut self, return_statement: &'a ReturnStatement) -> Result<(), Error> {
        let ret_count: u8 = cast(return_statement.returns.len()).ok_or(CompilerLimit::Returns)?;
        let var_count = VarCount::make_constant(ret_count).ok_or(CompilerLimit::Returns)?;

        if ret_count == 0 {
            self.functions.top.opcodes.push(OpCode::Return {
                start: 0,
                count: var_count,
            });
        } else if ret_count == 1 {
            let mut expr = self.expression(&return_statement.returns[0])?;
            let reg = self.expr_any_register(&mut expr)?;
            self.functions.top.opcodes.push(OpCode::Return {
                start: reg,
                count: var_count,
            });
            self.free_expr(expr);
        } else {
            let ret_start = self.functions.top.register_allocator.stack_top;
            for i in 0..ret_count {
                let expr = self.expression(&return_statement.returns[i as usize])?;
                self.expr_push_register(expr)?;
            }
            self.functions.top.opcodes.push(OpCode::Return {
                start: ret_start as u8,
                count: var_count,
            });
            self.functions.top.register_allocator.pop(ret_count);
        }

        Ok(())
    }

    fn local_statement(&mut self, local_statement: &'a LocalStatement) -> Result<(), Error> {
        for (i, expr) in local_statement.values.iter().enumerate() {
            if local_statement.names.len() > i {
                let expr = self.expression(expr)?;
                let reg = self.expr_allocate_register(expr)?;
                self.functions
                    .top
                    .locals
                    .push((&local_statement.names[i], reg));
            } else {
                let expr = self.expression(expr)?;
                self.free_expr(expr);
            }
        }
        for i in local_statement.values.len()..local_statement.names.len() {
            let reg = self
                .functions
                .top
                .register_allocator
                .allocate()
                .ok_or(CompilerLimit::Registers)?;
            self.load_nil(reg)?;
            self.functions
                .top
                .locals
                .push((&local_statement.names[i], reg));
        }
        Ok(())
    }

    fn function_call(&mut self, function_call: &'a FunctionCallStatement) -> Result<(), Error> {
        let func_expr = self.suffixed_expression(&function_call.head)?;
        let f_reg = self.expr_push_register(func_expr)?;

        match &function_call.call {
            CallSuffix::Function(exprs) => {
                let arg_count: u8 = cast(exprs.len()).ok_or(CompilerLimit::FixedParameters)?;
                for expr in exprs {
                    let expr = self.expression(expr)?;
                    self.expr_push_register(expr)?;
                }
                self.functions.top.opcodes.push(OpCode::Call {
                    func: f_reg,
                    args: VarCount::make_constant(arg_count).ok_or(CompilerLimit::FixedParameters)?,
                    returns: VarCount::make_zero(),
                });
                self.functions.top.register_allocator.pop(arg_count);
            }
            CallSuffix::Method(_, _) => bail!("method unsupported"),
        }

        self.functions.top.register_allocator.pop(1);

        Ok(())
    }

    fn assignment(&mut self, assignment: &'a AssignmentStatement) -> Result<(), Error> {
        for (i, target) in assignment.targets.iter().enumerate() {
            let mut expr = if i < assignment.values.len() {
                self.expression(&assignment.values[i])?
            } else {
                ExprDescriptor::Value(Value::Nil)
            };

            match target {
                AssignmentTarget::Name(name) => match self.find_variable(name)? {
                    VariableDescriptor::Local(dest) => {
                        self.expr_to_register(expr, dest)?;
                    }
                    VariableDescriptor::UpValue(dest) => {
                        let source = self.expr_any_register(&mut expr)?;
                        self.functions
                            .top
                            .opcodes
                            .push(OpCode::SetUpValue { source, dest });
                        self.free_expr(expr);
                    }
                    VariableDescriptor::Global(_) => bail!("global variables unsupported"),
                },
                AssignmentTarget::Field(_, _) => bail!("unimplemented assignment target"),
            }
        }

        Ok(())
    }

    fn local_function(&mut self, local_function: &'a FunctionStatement) -> Result<(), Error> {
        if local_function.definition.has_varargs {
            bail!("no varargs support");
        }
        if !local_function.name.fields.is_empty() {
            bail!("no function name fields support");
        }
        if local_function.name.method.is_some() {
            bail!("no method support");
        }

        self.functions.push(CompilerFunction::default());

        let fixed_params: u8 =
            cast(local_function.definition.parameters.len()).ok_or(CompilerLimit::FixedParameters)?;
        {
            self.functions.top.register_allocator.push(fixed_params);
            self.functions.top.fixed_params = fixed_params;
            for (i, name) in local_function.definition.parameters.iter().enumerate() {
                self.functions.top.locals.push((name, cast(i).unwrap()));
            }
        }

        self.block(&local_function.definition.body)?;

        let new_function = self.functions.pop();
        self.functions
            .top
            .prototypes
            .push(new_function.to_proto(self.mutation_context));
        let dest = self
            .functions
            .top
            .register_allocator
            .allocate()
            .ok_or(CompilerLimit::Registers)?;

        {
            let proto =
                cast(self.functions.top.prototypes.len() - 1).ok_or(CompilerLimit::Functions)?;
            self.functions
                .top
                .opcodes
                .push(OpCode::Closure { proto, dest });
            self.functions
                .top
                .locals
                .push((&local_function.name.name, dest));
        }

        Ok(())
    }

    fn expression(&mut self, expression: &'a Expression) -> Result<ExprDescriptor<'gc>, Error> {
        let mut expr = self.head_expression(&expression.head)?;
        for (binop, right) in &expression.tail {
            let right = self.expression(&right)?;
            expr = self.binop(expr, *binop, right)?;
        }
        Ok(expr)
    }

    fn head_expression(
        &mut self,
        head_expression: &'a HeadExpression,
    ) -> Result<ExprDescriptor<'gc>, Error> {
        match head_expression {
            HeadExpression::Simple(simple_expression) => self.simple_expression(simple_expression),
            HeadExpression::UnaryOperator(_, _) => bail!("no unary operator support yet"),
        }
    }

    fn simple_expression(
        &mut self,
        simple_expression: &'a SimpleExpression,
    ) -> Result<ExprDescriptor<'gc>, Error> {
        Ok(match simple_expression {
            SimpleExpression::Float(f) => ExprDescriptor::Value(Value::Number(*f)),
            SimpleExpression::Integer(i) => ExprDescriptor::Value(Value::Integer(*i)),
            SimpleExpression::String(s) => {
                let string = String::new(self.mutation_context, &*s);
                ExprDescriptor::Value(Value::String(string))
            }
            SimpleExpression::Nil => ExprDescriptor::Value(Value::Nil),
            SimpleExpression::True => ExprDescriptor::Value(Value::Boolean(true)),
            SimpleExpression::False => ExprDescriptor::Value(Value::Boolean(false)),
            SimpleExpression::Suffixed(suffixed) => self.suffixed_expression(suffixed)?,
            _ => bail!("unsupported simple expression"),
        })
    }

    fn suffixed_expression(
        &mut self,
        suffixed_expression: &'a SuffixedExpression,
    ) -> Result<ExprDescriptor<'gc>, Error> {
        if !suffixed_expression.suffixes.is_empty() {
            bail!("no support for expression suffixes yet");
        }
        self.primary_expression(&suffixed_expression.primary)
    }

    fn primary_expression(
        &mut self,
        primary_expression: &'a PrimaryExpression,
    ) -> Result<ExprDescriptor<'gc>, Error> {
        match primary_expression {
            PrimaryExpression::Name(name) => Ok(match self.find_variable(name)? {
                VariableDescriptor::Local(reg) => ExprDescriptor::Local(reg),
                VariableDescriptor::UpValue(upvalue) => ExprDescriptor::UpValue(upvalue),
                VariableDescriptor::Global(_) => {
                    bail!("no support for globals");
                }
            }),
            PrimaryExpression::GroupedExpression(expr) => self.expression(expr),
        }
    }

    fn binop(
        &mut self,
        left: ExprDescriptor<'gc>,
        binop: BinaryOperator,
        right: ExprDescriptor<'gc>,
    ) -> Result<ExprDescriptor<'gc>, Error> {
        if let (ExprDescriptor::Value(a), ExprDescriptor::Value(b)) = (left, right) {
            if let Some(v) = apply_binop(binop, a, b) {
                return Ok(ExprDescriptor::Value(v));
            }
        }

        match binop {
            BinaryOperator::Add => match (left, right) {
                (mut left_expr, ExprDescriptor::Value(right_const)) => {
                    let left = self.expr_any_register(&mut left_expr)?;
                    let right = self.get_constant(right_const)?;
                    self.free_expr(left_expr);
                    let dest = self
                        .functions
                        .top
                        .register_allocator
                        .allocate()
                        .ok_or(CompilerLimit::Registers)?;
                    self.functions
                        .top
                        .opcodes
                        .push(OpCode::AddRC { dest, left, right });
                    Ok(ExprDescriptor::Temporary(dest))
                }
                (ExprDescriptor::Value(left_const), mut right_expr) => {
                    let left = self.get_constant(left_const)?;
                    let right = self.expr_any_register(&mut right_expr)?;
                    self.free_expr(right_expr);
                    let dest = self
                        .functions
                        .top
                        .register_allocator
                        .allocate()
                        .ok_or(CompilerLimit::Registers)?;
                    self.functions
                        .top
                        .opcodes
                        .push(OpCode::AddCR { dest, left, right });
                    Ok(ExprDescriptor::Temporary(dest))
                }
                (mut left_expr, mut right_expr) => {
                    let left = self.expr_any_register(&mut left_expr)?;
                    let right = self.expr_any_register(&mut right_expr)?;
                    self.free_expr(right_expr);
                    self.free_expr(left_expr);
                    let dest = self
                        .functions
                        .top
                        .register_allocator
                        .allocate()
                        .ok_or(CompilerLimit::Registers)?;
                    self.functions
                        .top
                        .opcodes
                        .push(OpCode::AddRR { dest, left, right });
                    Ok(ExprDescriptor::Temporary(dest))
                }
            },
            _ => bail!("unsupported binary operator"),
        }
    }

    fn find_variable(&mut self, name: &'a [u8]) -> Result<VariableDescriptor<'a>, Error> {
        let function_len = self.functions.len();

        for i in (0..function_len).rev() {
            for j in (0..self.functions.get(i).locals.len()).rev() {
                let (local_name, register) = self.functions.get(i).locals[j];
                if name == local_name {
                    if i == function_len - 1 {
                        return Ok(VariableDescriptor::Local(register));
                    } else {
                        self.functions
                            .get_mut(i + 1)
                            .upvalues
                            .push((name, UpValueDescriptor::ParentLocal(register)));
                        let mut upvalue_index = cast(self.functions.get(i + 1).upvalues.len() - 1)
                            .ok_or(CompilerLimit::UpValues)?;
                        for k in i + 2..function_len {
                            self.functions
                                .get_mut(k)
                                .upvalues
                                .push((name, UpValueDescriptor::Outer(upvalue_index)));
                            upvalue_index = cast(self.functions.get(k).upvalues.len() - 1)
                                .ok_or(CompilerLimit::UpValues)?;
                        }
                        return Ok(VariableDescriptor::UpValue(upvalue_index));
                    }
                }
            }

            for j in 0..self.functions.get(i).upvalues.len() {
                if name == self.functions.get(i).upvalues[j].0 {
                    if i == function_len - 1 {
                        return Ok(VariableDescriptor::UpValue(cast(j).unwrap()));
                    } else {
                        let mut upvalue_index = cast(j).unwrap();
                        for k in i + 1..function_len {
                            self.functions
                                .get_mut(k)
                                .upvalues
                                .push((name, UpValueDescriptor::Outer(upvalue_index)));
                            upvalue_index = cast(self.functions.get(k).upvalues.len() - 1)
                                .ok_or(CompilerLimit::UpValues)?;
                        }
                        return Ok(VariableDescriptor::UpValue(upvalue_index));
                    }
                }
            }
        }

        Ok(VariableDescriptor::Global(name))
    }

    // Emit a LoadNil opcode, possibly combining several sequential LoadNil opcodes into one.
    fn load_nil(&mut self, dest: Register) -> Result<(), Error> {
        match self.functions.top.opcodes.last().cloned() {
            Some(OpCode::LoadNil {
                dest: prev_dest,
                count: prev_count,
            }) if prev_dest + prev_count == dest =>
            {
                self.functions.top.opcodes.push(OpCode::LoadNil {
                    dest: prev_dest,
                    count: prev_count + 1,
                });
            }
            _ => {
                self.functions
                    .top
                    .opcodes
                    .push(OpCode::LoadNil { dest, count: 1 });
            }
        }
        Ok(())
    }

    fn get_constant(&mut self, constant: Value<'gc>) -> Result<Constant, Error> {
        if let Some(constant) = self
            .functions
            .top
            .constant_table
            .get(&ConstantValue(constant))
            .cloned()
        {
            Ok(constant)
        } else {
            let c: Constant =
                cast(self.functions.top.constants.len()).ok_or(CompilerLimit::Constants)?;
            self.functions.top.constants.push(constant);
            self.functions
                .top
                .constant_table
                .insert(ConstantValue(constant), c);
            Ok(c)
        }
    }

    // Modify an expression to contain its result in any register, and return that register
    fn expr_any_register(&mut self, expr: &mut ExprDescriptor<'gc>) -> Result<Register, Error> {
        let (new_expr, reg) = match *expr {
            ExprDescriptor::Temporary(reg) => (ExprDescriptor::Temporary(reg), reg),
            ExprDescriptor::Local(reg) => (ExprDescriptor::Local(reg), reg),
            ExprDescriptor::UpValue(source) => {
                let dest = self
                    .functions
                    .top
                    .register_allocator
                    .allocate()
                    .ok_or(CompilerLimit::Registers)?;
                self.functions
                    .top
                    .opcodes
                    .push(OpCode::GetUpValue { source, dest });
                (ExprDescriptor::Temporary(dest), dest)
            }
            ExprDescriptor::Value(value) => {
                let dest = self
                    .functions
                    .top
                    .register_allocator
                    .allocate()
                    .ok_or(CompilerLimit::Registers)?;
                match value {
                    Value::Nil => {
                        self.load_nil(dest)?;
                    }
                    Value::Boolean(value) => {
                        self.functions.top.opcodes.push(OpCode::LoadBool {
                            dest,
                            value,
                            skip_next: false,
                        });
                    }
                    val => {
                        let constant = self.get_constant(val)?;
                        self.functions
                            .top
                            .opcodes
                            .push(OpCode::LoadConstant { dest, constant });
                    }
                }
                (ExprDescriptor::Temporary(dest), dest)
            }
        };

        *expr = new_expr;
        Ok(reg)
    }

    // Consume an expression and store the result in the given register
    fn expr_to_register(&mut self, expr: ExprDescriptor<'gc>, dest: Register) -> Result<(), Error> {
        match expr {
            ExprDescriptor::Temporary(source) => {
                self.functions
                    .top
                    .opcodes
                    .push(OpCode::Move { dest, source });
            }
            ExprDescriptor::Local(source) => {
                self.functions
                    .top
                    .opcodes
                    .push(OpCode::Move { dest, source });
            }
            ExprDescriptor::UpValue(source) => {
                self.functions
                    .top
                    .opcodes
                    .push(OpCode::GetUpValue { source, dest });
            }
            ExprDescriptor::Value(value) => match value {
                Value::Nil => {
                    self.load_nil(dest)?;
                }
                Value::Boolean(value) => {
                    self.functions.top.opcodes.push(OpCode::LoadBool {
                        dest,
                        value,
                        skip_next: false,
                    });
                }
                val => {
                    let constant = self.get_constant(val)?;
                    self.functions
                        .top
                        .opcodes
                        .push(OpCode::LoadConstant { dest, constant });
                }
            },
        }

        self.free_expr(expr);

        Ok(())
    }

    // Consume an expression, ensuring that its result is stored in a newly allocated register
    fn expr_allocate_register(&mut self, expr: ExprDescriptor<'gc>) -> Result<Register, Error> {
        match expr {
            ExprDescriptor::Temporary(register) => Ok(register),
            expr => {
                let dest = self
                    .functions
                    .top
                    .register_allocator
                    .allocate()
                    .ok_or(CompilerLimit::Registers)?;
                self.expr_to_register(expr, dest)?;
                Ok(dest)
            }
        }
    }

    // Consume an expression, ensuring that its result is stored in a newly allocated register at
    // the top of the stack.
    fn expr_push_register(&mut self, expr: ExprDescriptor<'gc>) -> Result<Register, Error> {
        if let ExprDescriptor::Temporary(register) = expr {
            if register as u16 + 1 == self.functions.top.register_allocator.stack_top {
                return Ok(register);
            }
        }

        let dest = self
            .functions
            .top
            .register_allocator
            .push(1)
            .ok_or(CompilerLimit::Registers)?;
        self.expr_to_register(expr, dest)?;
        Ok(dest)
    }

    fn free_expr(&mut self, expr: ExprDescriptor<'gc>) {
        if let ExprDescriptor::Temporary(r) = expr {
            self.functions.top.register_allocator.free(r);
        }
    }
}

#[derive(Default)]
struct CompilerFunction<'gc, 'a> {
    constants: Vec<Value<'gc>>,
    constant_table: HashMap<ConstantValue<'gc>, Constant>,

    upvalues: Vec<(&'a [u8], UpValueDescriptor)>,
    prototypes: Vec<FunctionProto<'gc>>,

    register_allocator: RegisterAllocator,

    fixed_params: u8,
    locals: Vec<(&'a [u8], Register)>,

    opcodes: Vec<OpCode>,
}

impl<'gc, 'a> CompilerFunction<'gc, 'a> {
    fn to_proto(self, mc: MutationContext<'gc, 'a>) -> FunctionProto<'gc> {
        assert_eq!(
            self.register_allocator.stack_top as usize,
            self.locals.len(),
            "register leak detected"
        );
        FunctionProto {
            fixed_params: self.fixed_params,
            has_varargs: false,
            stack_size: self.register_allocator.stack_size,
            constants: self.constants,
            opcodes: self.opcodes,
            upvalues: self.upvalues.iter().map(|(_, d)| *d).collect(),
            prototypes: self
                .prototypes
                .into_iter()
                .map(|f| Gc::allocate(mc, f))
                .collect(),
        }
    }
}

enum VariableDescriptor<'a> {
    Local(Register),
    UpValue(UpValueIndex),
    Global(&'a [u8]),
}

#[derive(Copy, Clone)]
enum ExprDescriptor<'gc> {
    Temporary(Register),
    Local(Register),
    UpValue(UpValueIndex),
    Value(Value<'gc>),
}

struct RegisterAllocator {
    // The total array of registers, marking whether they are allocated
    registers: [bool; 256],
    // The first free register
    first_free: u16,
    // The free register after the last used register
    stack_top: u16,
    // The index of the largest used register + 1 (e.g. the stack size required for the function)
    stack_size: u16,
}

impl Default for RegisterAllocator {
    fn default() -> RegisterAllocator {
        RegisterAllocator {
            registers: [false; 256],
            first_free: 0,
            stack_top: 0,
            stack_size: 0,
        }
    }
}

impl RegisterAllocator {
    // Allocates any single available register, returns it if one is available.
    fn allocate(&mut self) -> Option<Register> {
        if self.first_free < 256 {
            let register = self.first_free as u8;
            self.registers[register as usize] = true;

            if self.first_free == self.stack_top {
                self.stack_top += 1;
            }
            self.stack_size = self.stack_size.max(self.stack_top);

            let mut i = self.first_free;
            self.first_free = loop {
                if i == 256 || !self.registers[i as usize] {
                    break i;
                }
                i += 1;
            };

            Some(register)
        } else {
            None
        }
    }

    // Free a single register.
    fn free(&mut self, register: Register) {
        assert!(
            self.registers[register as usize],
            "cannot free unallocated register"
        );
        self.registers[register as usize] = false;
        self.first_free = self.first_free.min(register as u16);
        if register as u16 + 1 == self.stack_top {
            self.stack_top -= 1;
        }
    }

    // Allocates a block of registers of the given size (which must be > 0) always at the end of the
    // allocated area.  If successful, returns the starting register of the block.
    fn push(&mut self, size: u8) -> Option<Register> {
        if size == 0 {
            None
        } else if size as u16 <= 256 - self.stack_top {
            let rbegin = self.stack_top as u8;
            for i in rbegin..rbegin + size {
                self.registers[i as usize] = true;
            }
            if self.first_free == self.stack_top {
                self.first_free += size as u16;
            }
            self.stack_top += size as u16;
            self.stack_size = self.stack_size.max(self.stack_top);
            Some(rbegin)
        } else {
            None
        }
    }

    // Free a contiguous block of registers which have been just been pushed (so only a block at the
    // end of the allocated area).
    fn pop(&mut self, size: u8) {
        if size > 0 {
            assert!(
                self.stack_top >= size as u16,
                "cannot pop more registers than were allocated"
            );
            self.stack_top = self.stack_top - size as u16;
            for i in self.stack_top..self.stack_top + size as u16 {
                assert!(
                    self.registers[i as usize],
                    "not all popped registers were allocated"
                );
                self.registers[i as usize] = false;
            }
            self.first_free = self.first_free.min(self.stack_top);
        }
    }
}

// A stack which is guaranteed always to have a top value
struct TopStack<T> {
    top: T,
    lower: Vec<T>,
}

impl<T> TopStack<T> {
    fn new(top: T) -> TopStack<T> {
        TopStack {
            top,
            lower: Vec::new(),
        }
    }

    fn push(&mut self, t: T) {
        self.lower.push(mem::replace(&mut self.top, t));
    }

    fn pop(&mut self) -> T {
        mem::replace(
            &mut self.top,
            self.lower
                .pop()
                .expect("TopStack must always have one entry"),
        )
    }

    fn len(&self) -> usize {
        self.lower.len() + 1
    }

    fn get(&self, i: usize) -> &T {
        let lower_len = self.lower.len();
        if i < lower_len {
            &self.lower[i]
        } else if i == lower_len {
            &self.top
        } else {
            panic!("TopStack index {} out of range", i);
        }
    }

    fn get_mut(&mut self, i: usize) -> &mut T {
        let lower_len = self.lower.len();
        if i < lower_len {
            &mut self.lower[i]
        } else if i == lower_len {
            &mut self.top
        } else {
            panic!("TopStack index {} out of range", i);
        }
    }
}

// Value which implements Hash and Eq, where values are equal only when they are bit for bit
// identical.
struct ConstantValue<'gc>(Value<'gc>);

impl<'gc> PartialEq for ConstantValue<'gc> {
    fn eq(&self, other: &ConstantValue<'gc>) -> bool {
        match (self.0, other.0) {
            (Value::Nil, Value::Nil) => true,
            (Value::Nil, _) => false,

            (Value::Boolean(a), Value::Boolean(b)) => a == b,
            (Value::Boolean(_), _) => false,

            (Value::Integer(a), Value::Integer(b)) => a == b,
            (Value::Integer(_), _) => false,

            (Value::Number(a), Value::Number(b)) => float_bytes(a) == float_bytes(b),
            (Value::Number(_), _) => false,

            (Value::String(a), Value::String(b)) => a == b,
            (Value::String(_), _) => false,

            (Value::Table(a), Value::Table(b)) => a == b,
            (Value::Table(_), _) => false,

            (Value::Closure(a), Value::Closure(b)) => a == b,
            (Value::Closure(_), _) => false,
        }
    }
}

impl<'gc> Eq for ConstantValue<'gc> {}

impl<'gc> Hash for ConstantValue<'gc> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match &self.0 {
            Value::Nil => {
                Hash::hash(&0, state);
            }
            Value::Boolean(b) => {
                Hash::hash(&1, state);
                b.hash(state);
            }
            Value::Integer(i) => {
                Hash::hash(&2, state);
                i.hash(state);
            }
            Value::Number(n) => {
                Hash::hash(&3, state);
                float_bytes(*n).hash(state);
            }
            Value::String(s) => {
                Hash::hash(&4, state);
                s.hash(state);
            }
            Value::Table(t) => {
                Hash::hash(&5, state);
                t.hash(state);
            }
            Value::Closure(c) => {
                Hash::hash(&6, state);
                c.hash(state);
            }
        }
    }
}

fn float_bytes(f: f64) -> u64 {
    unsafe { mem::transmute(f) }
}
