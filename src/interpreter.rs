use rustc::middle::{const_eval, def_id, ty};
use rustc_mir::mir_map::MirMap;
use rustc_mir::repr::{self as mir, Mir};
use syntax::ast::Attribute;
use syntax::attr::AttrMetaMethods;

use std::iter;

const TRACE_EXECUTION: bool = false;

#[derive(Clone, Debug, PartialEq)]
enum Value {
    Uninit,
    Bool(bool),
    Int(i64), // FIXME: Should be bit-width aware.
    Func(def_id::DefId),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Pointer {
    Stack(usize),
    // TODO(tsion): Heap
}

/// A stack frame:
///
/// ```text
/// +-----------------------+
/// | ReturnPointer         | return value
/// + - - - - - - - - - - - +
/// | Arg(0)                |
/// | Arg(1)                | arguments
/// | ...                   |
/// | Arg(num_args - 1)     |
/// + - - - - - - - - - - - +
/// | Var(0)                |
/// | Var(1)                | variables
/// | ...                   |
/// | Var(num_vars - 1)     |
/// + - - - - - - - - - - - +
/// | Temp(0)               |
/// | Temp(1)               | temporaries
/// | ...                   |
/// | Temp(num_temps - 1)   |
/// + - - - - - - - - - - - +
/// | Aggregates            | aggregates
/// +-----------------------+
/// ```
#[derive(Debug)]
struct Frame {
    offset: usize,
    num_args: usize,
    num_vars: usize,
    num_temps: usize,
    // aggregates
}

impl Frame {
    fn size(&self) -> usize {
        1 + self.num_args + self.num_vars + self.num_temps
    }

    fn return_val_offset(&self) -> usize {
        self.offset
    }

    fn arg_offset(&self, i: u32) -> usize {
        self.offset + 1 + i as usize
    }

    fn var_offset(&self, i: u32) -> usize {
        self.offset + 1 + self.num_args + i as usize
    }

    fn temp_offset(&self, i: u32) -> usize {
        self.offset + 1 + self.num_args + self.num_vars + i as usize
    }
}

struct Interpreter<'a, 'tcx: 'a> {
    tcx: &'a ty::ctxt<'tcx>,
    mir_map: &'a MirMap<'tcx>,
    value_stack: Vec<Value>,
    call_stack: Vec<Frame>,
}

impl<'a, 'tcx> Interpreter<'a, 'tcx> {
    fn new(tcx: &'a ty::ctxt<'tcx>, mir_map: &'a MirMap<'tcx>) -> Self {
        Interpreter {
            tcx: tcx,
            mir_map: mir_map,
            value_stack: Vec::new(),
            call_stack: Vec::new(),
        }
    }

    fn push_stack_frame(&mut self, mir: &Mir, args: &[Value]) {
        self.call_stack.push(Frame {
            offset: self.value_stack.len(),
            num_args: mir.arg_decls.len(),
            num_vars: mir.var_decls.len(),
            num_temps: mir.temp_decls.len(),
        });

        let frame = self.call_stack.last().unwrap();
        self.value_stack.extend(iter::repeat(Value::Uninit).take(frame.size()));

        for (i, arg) in args.iter().enumerate() {
            self.value_stack[frame.offset + 1 + i] = arg.clone();
        }
    }

    fn pop_stack_frame(&mut self) {
        let frame = self.call_stack.pop().expect("tried to pop stack frame, but there were none");
        self.value_stack.truncate(frame.offset);
    }

    fn call(&mut self, mir: &Mir, args: &[Value]) -> Value {
        self.push_stack_frame(mir, args);
        let mut block = mir::START_BLOCK;

        loop {
            use rustc_mir::repr::Terminator::*;

            let block_data = mir.basic_block_data(block);

            for stmt in &block_data.statements {
                use rustc_mir::repr::StatementKind::*;

                if TRACE_EXECUTION { println!("{:?}", stmt); }

                match stmt.kind {
                    Assign(ref lvalue, ref rvalue) => {
                        let ptr = self.eval_lvalue(lvalue);
                        let value = self.eval_rvalue(rvalue);
                        self.write_pointer(ptr, value);
                    }

                    Drop(_kind, ref _lv) => {
                        // TODO
                    },
                }
            }

            if TRACE_EXECUTION { println!("{:?}", block_data.terminator); }

            match block_data.terminator {
                Return => break,
                Goto { target } => block = target,

                Call { data: mir::CallData { ref destination, ref func, ref args }, targets } => {
                    let ptr = self.eval_lvalue(destination);
                    let func_val = self.eval_operand(func);

                    if let Value::Func(def_id) = func_val {
                        let node_id = self.tcx.map.as_local_node_id(def_id).unwrap();
                        let mir = &self.mir_map[&node_id];
                        let arg_vals: Vec<Value> =
                            args.iter().map(|arg| self.eval_operand(arg)).collect();

                        // FIXME: Pass the destination lvalue such that the ReturnPointer inside
                        // the function call will point to the destination.
                        let return_val = self.call(mir, &arg_vals);
                        self.write_pointer(ptr, return_val);
                        block = targets[0];
                    } else {
                        panic!("tried to call a non-function value: {:?}", func_val);
                    }
                }

                If { ref cond, targets } => {
                    match self.eval_operand(cond) {
                        Value::Bool(true) => block = targets[0],
                        Value::Bool(false) => block = targets[1],
                        cond_val => panic!("Non-boolean `if` condition value: {:?}", cond_val),
                    }
                }

                SwitchInt { ref discr, switch_ty: _, ref values, ref targets } => {
                    let discr_val = self.read_lvalue(discr);

                    let index = values.iter().position(|v| discr_val == self.eval_constant(v))
                        .expect("discriminant matched no values");

                    block = targets[index];
                }

                // Diverge => unimplemented!(),
                // Panic { target } => unimplemented!(),
                // Switch { ref discr, adt_def, ref targets } => unimplemented!(),
                _ => unimplemented!(),
            }
        }

        let ret_val = self.read_lvalue(&mir::Lvalue::ReturnPointer);
        self.pop_stack_frame();
        ret_val
    }

    fn eval_lvalue(&self, lvalue: &mir::Lvalue) -> Pointer {
        use rustc_mir::repr::Lvalue::*;

        let frame = self.call_stack.last().expect("missing call frame");

        match *lvalue {
            ReturnPointer => Pointer::Stack(frame.return_val_offset()),
            Arg(i)  => Pointer::Stack(frame.arg_offset(i)),
            Var(i)  => Pointer::Stack(frame.var_offset(i)),
            Temp(i) => Pointer::Stack(frame.temp_offset(i)),
            _ => unimplemented!(),
        }
    }

    fn eval_rvalue(&mut self, rvalue: &mir::Rvalue) -> Value {
        use rustc_mir::repr::Rvalue::*;
        use rustc_mir::repr::BinOp::*;
        use rustc_mir::repr::UnOp::*;

        match *rvalue {
            Use(ref operand) => self.eval_operand(operand),

            BinaryOp(bin_op, ref left, ref right) => {
                match (self.eval_operand(left), self.eval_operand(right)) {
                    (Value::Int(l), Value::Int(r)) => {
                        match bin_op {
                            Add => Value::Int(l + r),
                            Sub => Value::Int(l - r),
                            Mul => Value::Int(l * r),
                            Div => Value::Int(l / r),
                            Rem => Value::Int(l % r),
                            BitXor => Value::Int(l ^ r),
                            BitAnd => Value::Int(l & r),
                            BitOr => Value::Int(l | r),
                            Shl => Value::Int(l << r),
                            Shr => Value::Int(l >> r),
                            Eq => Value::Bool(l == r),
                            Lt => Value::Bool(l < r),
                            Le => Value::Bool(l <= r),
                            Ne => Value::Bool(l != r),
                            Ge => Value::Bool(l >= r),
                            Gt => Value::Bool(l > r),
                        }
                    }
                    _ => unimplemented!(),
                }
            }

            UnaryOp(un_op, ref operand) => {
                match (un_op, self.eval_operand(operand)) {
                    (Not, Value::Int(n)) => Value::Int(!n),
                    (Neg, Value::Int(n)) => Value::Int(-n),
                    _ => unimplemented!(),
                }
            }

            // Aggregate(mir::AggregateKind::Adt(ref adt_def, variant, substs), ref operands) => {
            //     let num_fields = adt_def.variants[variant].fields.len();
            //     debug_assert_eq!(num_fields, operands.len());

            //     let data = operands.iter().map(|op| self.eval_operand(op)).collect();
            //     Value::Adt(variant, data)
            // }

            _ => unimplemented!(),
        }
    }

    fn eval_operand(&mut self, op: &mir::Operand) -> Value {
        use rustc_mir::repr::Operand::*;

        match *op {
            Consume(ref lvalue) => self.read_lvalue(lvalue),

            Constant(ref constant) => {
                match constant.literal {
                    mir::Literal::Value { ref value } => self.eval_constant(value),

                    mir::Literal::Item { def_id, substs: _ } => {
                        Value::Func(def_id)
                    }
                }
            }
        }
    }

    fn eval_constant(&self, const_val: &const_eval::ConstVal) -> Value {
        use rustc::middle::const_eval::ConstVal::*;

        match *const_val {
            Float(_f) => unimplemented!(),
            Int(i) => Value::Int(i),
            Uint(_u) => unimplemented!(),
            Str(ref _s) => unimplemented!(),
            ByteStr(ref _bs) => unimplemented!(),
            Bool(b) => Value::Bool(b),
            Struct(_node_id) => unimplemented!(),
            Tuple(_node_id) => unimplemented!(),
            Function(_def_id) => unimplemented!(),
        }
    }

    fn read_lvalue(&self, lvalue: &mir::Lvalue) -> Value {
        self.read_pointer(self.eval_lvalue(lvalue))
    }

    fn read_pointer(&self, p: Pointer) -> Value {
        match p {
            Pointer::Stack(offset) => self.value_stack[offset].clone(),
        }
    }

    fn write_pointer(&mut self, p: Pointer, val: Value) {
        match p {
            Pointer::Stack(offset) => self.value_stack[offset] = val,
        }
    }
}

pub fn interpret_start_points<'tcx>(tcx: &ty::ctxt<'tcx>, mir_map: &MirMap<'tcx>) {
    for (&id, mir) in mir_map {
        for attr in tcx.map.attrs(id) {
            if attr.check_name("miri_run") {
                let item = tcx.map.expect_item(id);

                println!("Interpreting: {}", item.name);
                let mut interpreter = Interpreter::new(tcx, mir_map);
                let val = interpreter.call(mir, &[]);
                let val_str = format!("{:?}", val);

                if !check_expected(&val_str, attr) {
                    println!("=> {}\n", val_str);
                }
            }
        }
    }
}

fn check_expected(actual: &str, attr: &Attribute) -> bool {
    if let Some(meta_items) = attr.meta_item_list() {
        for meta_item in meta_items {
            if meta_item.check_name("expected") {
                let expected = meta_item.value_str().unwrap();

                if actual == &expected[..] {
                    println!("Test passed!\n");
                } else {
                    println!("Actual value:\t{}\nExpected value:\t{}\n", actual, expected);
                }

                return true;
            }
        }
    }

    false
}