use rustc::middle::const_val;
use rustc::hir::def_id::DefId;
use rustc::mir::mir_map::MirMap;
use rustc::mir::repr as mir;
use rustc::traits::{self, ProjectionMode};
use rustc::ty::fold::TypeFoldable;
use rustc::ty::layout::{self, Layout, Size};
use rustc::ty::subst::{self, Subst, Substs};
use rustc::ty::{self, TyCtxt};
use rustc::util::nodemap::DefIdMap;
use std::cell::RefCell;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::{iter, mem};
use syntax::ast;
use syntax::attr;
use syntax::codemap::{self, DUMMY_SP};

use error::{EvalError, EvalResult};
use memory::{Memory, Pointer};
use primval::{self, PrimVal};

const TRACE_EXECUTION: bool = true;

struct GlobalEvalContext<'a, 'tcx: 'a> {
    /// The results of the type checker, from rustc.
    tcx: TyCtxt<'a, 'tcx, 'tcx>,

    /// A mapping from NodeIds to Mir, from rustc. Only contains MIR for crate-local items.
    mir_map: &'a MirMap<'tcx>,

    /// A local cache from DefIds to Mir for non-crate-local items.
    mir_cache: RefCell<DefIdMap<Rc<mir::Mir<'tcx>>>>,

    /// The virtual memory system.
    memory: Memory,

    /// Another stack containing the type substitutions for the current function invocation. It
    /// exists separately from `stack` because it must contain the `Substs` for a function while
    /// *creating* the `Frame` for that same function.
    substs_stack: Vec<&'tcx Substs<'tcx>>,

    // TODO(solson): Merge with `substs_stack`. Also try restructuring `Frame` to accomodate.
    /// A stack of the things necessary to print good strack traces:
    ///   * Function DefIds and Substs to print proper substituted function names.
    ///   * Spans pointing to specific function calls in the source.
    name_stack: Vec<(DefId, &'tcx Substs<'tcx>, codemap::Span)>,
}

struct FnEvalContext<'a, 'b: 'a + 'mir, 'mir, 'tcx: 'b> {
    gecx: &'a mut GlobalEvalContext<'b, 'tcx>,

    /// The virtual call stack.
    stack: Vec<Frame<'mir, 'tcx>>,
}

impl<'a, 'b, 'mir, 'tcx> Deref for FnEvalContext<'a, 'b, 'mir, 'tcx> {
    type Target = GlobalEvalContext<'b, 'tcx>;
    fn deref(&self) -> &Self::Target {
        self.gecx
    }
}

impl<'a, 'b, 'mir, 'tcx> DerefMut for FnEvalContext<'a, 'b, 'mir, 'tcx> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.gecx
    }
}

/// A stack frame.
struct Frame<'a, 'tcx: 'a> {
    /// The MIR for the function called on this frame.
    mir: CachedMir<'a, 'tcx>,

    /// The block this frame will execute when a function call returns back to this frame.
    next_block: mir::BasicBlock,

    /// A pointer for writing the return value of the current call if it's not a diverging call.
    return_ptr: Option<Pointer>,

    /// The list of locals for the current function, stored in order as
    /// `[arguments..., variables..., temporaries...]`. The variables begin at `self.var_offset`
    /// and the temporaries at `self.temp_offset`.
    locals: Vec<Pointer>,

    /// The offset of the first variable in `self.locals`.
    var_offset: usize,

    /// The offset of the first temporary in `self.locals`.
    temp_offset: usize,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Lvalue {
    ptr: Pointer,
    extra: LvalueExtra,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum LvalueExtra {
    None,
    Length(u64),
    // TODO(solson): Vtable(memory::AllocId),
    DowncastVariant(usize),
}

#[derive(Clone)]
enum CachedMir<'mir, 'tcx: 'mir> {
    Ref(&'mir mir::Mir<'tcx>),
    Owned(Rc<mir::Mir<'tcx>>)
}

/// Represents the action to be taken in the main loop as a result of executing a terminator.
enum TerminatorTarget {
    /// Make a local jump to the given block.
    Block(mir::BasicBlock),

    /// Start executing from the new current frame. (For function calls.)
    Call,

    /// Stop executing the current frame and resume the previous frame.
    Return,
}

impl<'a, 'tcx> GlobalEvalContext<'a, 'tcx> {
    fn new(tcx: TyCtxt<'a, 'tcx, 'tcx>, mir_map: &'a MirMap<'tcx>) -> Self {
        GlobalEvalContext {
            tcx: tcx,
            mir_map: mir_map,
            mir_cache: RefCell::new(DefIdMap()),
            memory: Memory::new(),
            substs_stack: Vec::new(),
            name_stack: Vec::new(),
        }
    }
}

impl<'a, 'b, 'mir, 'tcx> FnEvalContext<'a, 'b, 'mir, 'tcx> {
    fn new(gecx: &'a mut GlobalEvalContext<'b, 'tcx>) -> Self {
        FnEvalContext {
            gecx: gecx,
            stack: Vec::new(),
        }
    }

    fn maybe_report<T>(&self, span: codemap::Span, r: EvalResult<T>) -> EvalResult<T> {
        if let Err(ref e) = r {
            let mut err = self.tcx.sess.struct_span_err(span, &e.to_string());
            for &(def_id, substs, span) in self.name_stack.iter().rev() {
                // FIXME(solson): Find a way to do this without this Display impl hack.
                use rustc::util::ppaux;
                use std::fmt;
                struct Instance<'tcx>(DefId, &'tcx Substs<'tcx>);
                impl<'tcx> fmt::Display for Instance<'tcx> {
                    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        ppaux::parameterized(f, self.1, self.0, ppaux::Ns::Value, &[],
                            |tcx| tcx.lookup_item_type(self.0).generics)
                    }
                }
                err.span_note(span, &format!("inside call to {}", Instance(def_id, substs)));
            }
            err.emit();
        }
        r
    }

    fn log<F>(&self, extra_indent: usize, f: F) where F: FnOnce() {
        let indent = self.stack.len() + extra_indent;
        if !TRACE_EXECUTION { return; }
        for _ in 0..indent { print!("    "); }
        f();
        println!("");
    }

    fn run(&mut self) -> EvalResult<()> {
        'outer: while !self.stack.is_empty() {
            let mut current_block = self.frame().next_block;

            loop {
                self.log(0, || print!("// {:?}", current_block));
                let current_mir = self.mir().clone(); // Cloning a reference.
                let block_data = current_mir.basic_block_data(current_block);

                for stmt in &block_data.statements {
                    self.log(0, || print!("{:?}", stmt));
                    let mir::StatementKind::Assign(ref lvalue, ref rvalue) = stmt.kind;
                    let result = self.eval_assignment(lvalue, rvalue);
                    self.maybe_report(stmt.span, result)?;
                }

                let terminator = block_data.terminator();
                self.log(0, || print!("{:?}", terminator.kind));

                let result = self.eval_terminator(terminator);
                match self.maybe_report(terminator.span, result)? {
                    TerminatorTarget::Block(block) => current_block = block,
                    TerminatorTarget::Return => {
                        self.pop_stack_frame();
                        self.name_stack.pop();
                        continue 'outer;
                    }
                    TerminatorTarget::Call => continue 'outer,
                }
            }
        }

        Ok(())
    }

    fn call_nested(&mut self, mir: &mir::Mir<'tcx>) -> EvalResult<Option<Pointer>> {
        let mut nested_fecx = FnEvalContext::new(self.gecx);

        let return_ptr = match mir.return_ty {
            ty::FnConverging(ty) => {
                let size = nested_fecx.type_size(ty);
                Some(nested_fecx.memory.allocate(size))
            }
            ty::FnDiverging => None,
        };

        let substs = nested_fecx.substs();
        nested_fecx.push_stack_frame(CachedMir::Ref(mir), substs, return_ptr);
        nested_fecx.run()?;
        Ok(return_ptr)
    }

    fn push_stack_frame(&mut self, mir: CachedMir<'mir, 'tcx>, substs: &'tcx Substs<'tcx>,
        return_ptr: Option<Pointer>)
    {
        self.substs_stack.push(substs);

        let arg_tys = mir.arg_decls.iter().map(|a| a.ty);
        let var_tys = mir.var_decls.iter().map(|v| v.ty);
        let temp_tys = mir.temp_decls.iter().map(|t| t.ty);

        let locals: Vec<Pointer> = arg_tys.chain(var_tys).chain(temp_tys).map(|ty| {
            let size = self.type_size(ty);
            self.memory.allocate(size)
        }).collect();

        let num_args = mir.arg_decls.len();
        let num_vars = mir.var_decls.len();

        self.stack.push(Frame {
            mir: mir.clone(),
            next_block: mir::START_BLOCK,
            return_ptr: return_ptr,
            locals: locals,
            var_offset: num_args,
            temp_offset: num_args + num_vars,
        });
    }

    fn pop_stack_frame(&mut self) {
        let _frame = self.stack.pop().expect("tried to pop a stack frame, but there were none");
        // TODO(solson): Deallocate local variables.
        self.substs_stack.pop();
    }

    fn eval_terminator(&mut self, terminator: &mir::Terminator<'tcx>)
            -> EvalResult<TerminatorTarget> {
        use rustc::mir::repr::TerminatorKind::*;
        let target = match terminator.kind {
            Return => TerminatorTarget::Return,

            Goto { target } => TerminatorTarget::Block(target),

            If { ref cond, targets: (then_target, else_target) } => {
                let cond_ptr = self.eval_operand(cond)?;
                let cond_val = self.memory.read_bool(cond_ptr)?;
                TerminatorTarget::Block(if cond_val { then_target } else { else_target })
            }

            SwitchInt { ref discr, ref values, ref targets, .. } => {
                let discr_ptr = self.eval_lvalue(discr)?.to_ptr();
                let discr_size = self
                    .type_layout(self.lvalue_ty(discr))
                    .size(&self.tcx.data_layout)
                    .bytes() as usize;
                let discr_val = self.memory.read_uint(discr_ptr, discr_size)?;

                // Branch to the `otherwise` case by default, if no match is found.
                let mut target_block = targets[targets.len() - 1];

                for (index, val_const) in values.iter().enumerate() {
                    let ptr = self.const_to_ptr(val_const)?;
                    let val = self.memory.read_uint(ptr, discr_size)?;
                    if discr_val == val {
                        target_block = targets[index];
                        break;
                    }
                }

                TerminatorTarget::Block(target_block)
            }

            Switch { ref discr, ref targets, adt_def } => {
                let adt_ptr = self.eval_lvalue(discr)?.to_ptr();
                let adt_ty = self.lvalue_ty(discr);
                let discr_val = self.read_discriminant_value(adt_ptr, adt_ty)?;
                let matching = adt_def.variants.iter()
                    .position(|v| discr_val == v.disr_val.to_u64_unchecked());

                match matching {
                    Some(i) => TerminatorTarget::Block(targets[i]),
                    None => return Err(EvalError::InvalidDiscriminant),
                }
            }

            Call { ref func, ref args, ref destination, .. } => {
                let mut return_ptr = None;
                if let Some((ref lv, target)) = *destination {
                    self.frame_mut().next_block = target;
                    return_ptr = Some(self.eval_lvalue(lv)?.to_ptr());
                }

                let func_ty = self.operand_ty(func);
                match func_ty.sty {
                    ty::TyFnDef(def_id, substs, fn_ty) => {
                        use syntax::abi::Abi;
                        match fn_ty.abi {
                            Abi::RustIntrinsic => {
                                let name = self.tcx.item_name(def_id).as_str();
                                match fn_ty.sig.0.output {
                                    ty::FnConverging(ty) => {
                                        let size = self.type_size(ty);
                                        let ret = return_ptr.unwrap();
                                        self.call_intrinsic(&name, substs, args, ret, size)?
                                    }
                                    ty::FnDiverging => unimplemented!(),
                                }
                            }

                            Abi::C => {
                                match fn_ty.sig.0.output {
                                    ty::FnConverging(ty) => {
                                        let size = self.type_size(ty);
                                        self.call_c_abi(def_id, args, return_ptr.unwrap(), size)?
                                    }
                                    ty::FnDiverging => unimplemented!(),
                                }
                            }

                            Abi::Rust | Abi::RustCall => {
                                // TODO(solson): Adjust the first argument when calling a Fn or
                                // FnMut closure via FnOnce::call_once.

                                // Only trait methods can have a Self parameter.
                                let (resolved_def_id, resolved_substs) = if substs.self_ty().is_some() {
                                    self.trait_method(def_id, substs)
                                } else {
                                    (def_id, substs)
                                };

                                let mut arg_srcs = Vec::new();
                                for arg in args {
                                    let src = self.eval_operand(arg)?;
                                    let src_ty = self.operand_ty(arg);
                                    arg_srcs.push((src, src_ty));
                                }

                                if fn_ty.abi == Abi::RustCall && !args.is_empty() {
                                    arg_srcs.pop();
                                    let last_arg = args.last().unwrap();
                                    let last = self.eval_operand(last_arg)?;
                                    let last_ty = self.operand_ty(last_arg);
                                    let last_layout = self.type_layout(last_ty);
                                    match (&last_ty.sty, last_layout) {
                                        (&ty::TyTuple(fields),
                                         &Layout::Univariant { ref variant, .. }) => {
                                            let offsets = iter::once(0)
                                                .chain(variant.offset_after_field.iter()
                                                    .map(|s| s.bytes()));
                                            for (offset, ty) in offsets.zip(fields) {
                                                let src = last.offset(offset as isize);
                                                arg_srcs.push((src, ty));
                                            }
                                        }
                                        ty => panic!("expected tuple as last argument in function with 'rust-call' ABI, got {:?}", ty),
                                    }
                                }

                                let mir = self.load_mir(resolved_def_id);
                                self.name_stack.push((def_id, substs, terminator.span));
                                self.push_stack_frame(mir, resolved_substs, return_ptr);

                                for (i, (src, src_ty)) in arg_srcs.into_iter().enumerate() {
                                    let dest = self.frame().locals[i];
                                    self.move_(src, dest, src_ty)?;
                                }

                                TerminatorTarget::Call
                            }

                            abi => panic!("can't handle function with {:?} ABI", abi),
                        }
                    }

                    _ => panic!("can't handle callee of type {:?}", func_ty),
                }
            }

            Drop { ref value, target, .. } => {
                let ptr = self.eval_lvalue(value)?.to_ptr();
                let ty = self.lvalue_ty(value);
                self.drop(ptr, ty)?;
                TerminatorTarget::Block(target)
            }

            Resume => unimplemented!(),
        };

        Ok(target)
    }

    fn drop(&mut self, ptr: Pointer, ty: ty::Ty<'tcx>) -> EvalResult<()> {
        if !self.type_needs_drop(ty) {
            self.log(1, || print!("no need to drop {:?}", ty));
            return Ok(());
        }
        self.log(1, || print!("need to drop {:?}", ty));

        // TODO(solson): Call user-defined Drop::drop impls.

        match ty.sty {
            ty::TyBox(contents_ty) => {
                match self.memory.read_ptr(ptr) {
                    Ok(contents_ptr) => {
                        self.drop(contents_ptr, contents_ty)?;
                        self.log(1, || print!("deallocating box"));
                        self.memory.deallocate(contents_ptr)?;
                    }
                    Err(EvalError::ReadBytesAsPointer) => {
                        let size = self.memory.pointer_size;
                        let possible_drop_fill = self.memory.read_bytes(ptr, size)?;
                        if possible_drop_fill.iter().all(|&b| b == mem::POST_DROP_U8) {
                            return Ok(());
                        } else {
                            return Err(EvalError::ReadBytesAsPointer);
                        }
                    }
                    Err(e) => return Err(e),
                }
            }

            // TODO(solson): Implement drop for other relevant types (e.g. aggregates).
            _ => {}
        }

        // Filling drop.
        // FIXME(solson): Trait objects (with no static size) probably get filled, too.
        let size = self.type_size(ty);
        self.memory.drop_fill(ptr, size)?;

        Ok(())
    }

    fn read_discriminant_value(&self, adt_ptr: Pointer, adt_ty: ty::Ty<'tcx>) -> EvalResult<u64> {
        use rustc::ty::layout::Layout::*;
        let adt_layout = self.type_layout(adt_ty);

        let discr_val = match *adt_layout {
            General { discr, .. } | CEnum { discr, .. } => {
                let discr_size = discr.size().bytes();
                self.memory.read_uint(adt_ptr, discr_size as usize)?
            }

            RawNullablePointer { nndiscr, .. } => {
                let not_null = match self.memory.read_usize(adt_ptr) {
                    Ok(0) => false,
                    Ok(_) | Err(EvalError::ReadPointerAsBytes) => true,
                    Err(e) => return Err(e),
                };
                assert!(nndiscr == 0 || nndiscr == 1);
                if not_null { nndiscr } else { 1 - nndiscr }
            }

            StructWrappedNullablePointer { .. } => unimplemented!(),

            // The discriminant_value intrinsic returns 0 for non-sum types.
            Array { .. } | FatPointer { .. } | Scalar { .. } | Univariant { .. } |
            Vector { .. } => 0,
        };

        Ok(discr_val)
    }

    fn call_intrinsic(
        &mut self,
        name: &str,
        substs: &'tcx Substs<'tcx>,
        args: &[mir::Operand<'tcx>],
        dest: Pointer,
        dest_size: usize
    ) -> EvalResult<TerminatorTarget> {
        let args_res: EvalResult<Vec<Pointer>> = args.iter()
            .map(|arg| self.eval_operand(arg))
            .collect();
        let args = args_res?;

        match name {
            // FIXME(solson): Handle different integer types correctly.
            "add_with_overflow" => {
                let ty = *substs.types.get(subst::FnSpace, 0);
                let size = self.type_size(ty);
                let left = self.memory.read_int(args[0], size)?;
                let right = self.memory.read_int(args[1], size)?;
                let (n, overflowed) = unsafe {
                    ::std::intrinsics::add_with_overflow::<i64>(left, right)
                };
                self.memory.write_int(dest, n, size)?;
                self.memory.write_bool(dest.offset(size as isize), overflowed)?;
            }

            "assume" => {}

            "copy_nonoverlapping" => {
                let elem_ty = *substs.types.get(subst::FnSpace, 0);
                let elem_size = self.type_size(elem_ty);
                let src = self.memory.read_ptr(args[0])?;
                let dest = self.memory.read_ptr(args[1])?;
                let count = self.memory.read_isize(args[2])?;
                self.memory.copy(src, dest, count as usize * elem_size)?;
            }

            "discriminant_value" => {
                let ty = *substs.types.get(subst::FnSpace, 0);
                let adt_ptr = self.memory.read_ptr(args[0])?;
                let discr_val = self.read_discriminant_value(adt_ptr, ty)?;
                self.memory.write_uint(dest, discr_val, dest_size)?;
            }

            "forget" => {
                let arg_ty = *substs.types.get(subst::FnSpace, 0);
                let arg_size = self.type_size(arg_ty);
                self.memory.drop_fill(args[0], arg_size)?;
            }

            "init" => self.memory.write_repeat(dest, 0, dest_size)?,

            "min_align_of" => {
                self.memory.write_int(dest, 1, dest_size)?;
            }

            "move_val_init" => {
                let ty = *substs.types.get(subst::FnSpace, 0);
                let ptr = self.memory.read_ptr(args[0])?;
                self.move_(args[1], ptr, ty)?;
            }

            // FIXME(solson): Handle different integer types correctly.
            "mul_with_overflow" => {
                let ty = *substs.types.get(subst::FnSpace, 0);
                let size = self.type_size(ty);
                let left = self.memory.read_int(args[0], size)?;
                let right = self.memory.read_int(args[1], size)?;
                let (n, overflowed) = unsafe {
                    ::std::intrinsics::mul_with_overflow::<i64>(left, right)
                };
                self.memory.write_int(dest, n, size)?;
                self.memory.write_bool(dest.offset(size as isize), overflowed)?;
            }

            "offset" => {
                let pointee_ty = *substs.types.get(subst::FnSpace, 0);
                let pointee_size = self.type_size(pointee_ty) as isize;
                let ptr_arg = args[0];
                let offset = self.memory.read_isize(args[1])?;

                match self.memory.read_ptr(ptr_arg) {
                    Ok(ptr) => {
                        let result_ptr = ptr.offset(offset as isize * pointee_size);
                        self.memory.write_ptr(dest, result_ptr)?;
                    }
                    Err(EvalError::ReadBytesAsPointer) => {
                        let addr = self.memory.read_isize(ptr_arg)?;
                        let result_addr = addr + offset * pointee_size as i64;
                        self.memory.write_isize(dest, result_addr)?;
                    }
                    Err(e) => return Err(e),
                }
            }

            // FIXME(solson): Handle different integer types correctly. Use primvals?
            "overflowing_sub" => {
                let ty = *substs.types.get(subst::FnSpace, 0);
                let size = self.type_size(ty);
                let left = self.memory.read_int(args[0], size)?;
                let right = self.memory.read_int(args[1], size)?;
                let n = left.wrapping_sub(right);
                self.memory.write_int(dest, n, size)?;
            }

            "size_of" => {
                let ty = *substs.types.get(subst::FnSpace, 0);
                let size = self.type_size(ty) as u64;
                self.memory.write_uint(dest, size, dest_size)?;
            }

            "size_of_val" => {
                let ty = *substs.types.get(subst::FnSpace, 0);
                if self.type_is_sized(ty) {
                    let size = self.type_size(ty) as u64;
                    self.memory.write_uint(dest, size, dest_size)?;
                } else {
                    match ty.sty {
                        ty::TySlice(_) | ty::TyStr => {
                            let elem_ty = ty.sequence_element_type(self.tcx);
                            let elem_size = self.type_size(elem_ty) as u64;
                            let ptr_size = self.memory.pointer_size as isize;
                            let n = self.memory.read_usize(args[0].offset(ptr_size))?;
                            self.memory.write_uint(dest, n * elem_size, dest_size)?;
                        }

                        _ => panic!("unimplemented: size_of_val::<{:?}>", ty),
                    }
                }
            }

            "transmute" => {
                let ty = *substs.types.get(subst::FnSpace, 0);
                self.move_(args[0], dest, ty)?;
            }
            "uninit" => self.memory.mark_definedness(dest, dest_size, false)?,

            name => panic!("can't handle intrinsic: {}", name),
        }

        // Since we pushed no stack frame, the main loop will act
        // as if the call just completed and it's returning to the
        // current frame.
        Ok(TerminatorTarget::Call)
    }

    fn call_c_abi(
        &mut self,
        def_id: DefId,
        args: &[mir::Operand<'tcx>],
        dest: Pointer,
        dest_size: usize,
    ) -> EvalResult<TerminatorTarget> {
        let name = self.tcx.item_name(def_id);
        let attrs = self.tcx.get_attrs(def_id);
        let link_name = match attr::first_attr_value_str_by_name(&attrs, "link_name") {
            Some(ln) => ln.clone(),
            None => name.as_str(),
        };

        let args_res: EvalResult<Vec<Pointer>> = args.iter()
            .map(|arg| self.eval_operand(arg))
            .collect();
        let args = args_res?;

        match &link_name[..] {
            "__rust_allocate" => {
                let size = self.memory.read_usize(args[0])?;
                let ptr = self.memory.allocate(size as usize);
                self.memory.write_ptr(dest, ptr)?;
            }

            "__rust_reallocate" => {
                let ptr = self.memory.read_ptr(args[0])?;
                let size = self.memory.read_usize(args[2])?;
                self.memory.reallocate(ptr, size as usize)?;
                self.memory.write_ptr(dest, ptr)?;
            }

            "memcmp" => {
                let left = self.memory.read_ptr(args[0])?;
                let right = self.memory.read_ptr(args[1])?;
                let n = self.memory.read_usize(args[2])? as usize;

                let result = {
                    let left_bytes = self.memory.read_bytes(left, n)?;
                    let right_bytes = self.memory.read_bytes(right, n)?;

                    use std::cmp::Ordering::*;
                    match left_bytes.cmp(right_bytes) {
                        Less => -1,
                        Equal => 0,
                        Greater => 1,
                    }
                };

                self.memory.write_int(dest, result, dest_size)?;
            }

            _ => panic!("can't call C ABI function: {}", link_name),
        }

        // Since we pushed no stack frame, the main loop will act
        // as if the call just completed and it's returning to the
        // current frame.
        Ok(TerminatorTarget::Call)
    }

    fn assign_fields<I: IntoIterator<Item = u64>>(
        &mut self,
        dest: Pointer,
        offsets: I,
        operands: &[mir::Operand<'tcx>],
    ) -> EvalResult<()> {
        for (offset, operand) in offsets.into_iter().zip(operands) {
            let src = self.eval_operand(operand)?;
            let src_ty = self.operand_ty(operand);
            let field_dest = dest.offset(offset as isize);
            self.move_(src, field_dest, src_ty)?;
        }
        Ok(())
    }

    fn eval_assignment(&mut self, lvalue: &mir::Lvalue<'tcx>, rvalue: &mir::Rvalue<'tcx>)
        -> EvalResult<()>
    {
        let dest = self.eval_lvalue(lvalue)?.to_ptr();
        let dest_ty = self.lvalue_ty(lvalue);
        let dest_layout = self.type_layout(dest_ty);

        use rustc::mir::repr::Rvalue::*;
        match *rvalue {
            Use(ref operand) => {
                let src = self.eval_operand(operand)?;
                self.move_(src, dest, dest_ty)?;
            }

            BinaryOp(bin_op, ref left, ref right) => {
                let left_ptr = self.eval_operand(left)?;
                let left_ty = self.operand_ty(left);
                let left_val = self.read_primval(left_ptr, left_ty)?;

                let right_ptr = self.eval_operand(right)?;
                let right_ty = self.operand_ty(right);
                let right_val = self.read_primval(right_ptr, right_ty)?;

                let val = primval::binary_op(bin_op, left_val, right_val)?;
                self.memory.write_primval(dest, val)?;
            }

            UnaryOp(un_op, ref operand) => {
                let ptr = self.eval_operand(operand)?;
                let ty = self.operand_ty(operand);
                let val = self.read_primval(ptr, ty)?;
                self.memory.write_primval(dest, primval::unary_op(un_op, val))?;
            }

            Aggregate(ref kind, ref operands) => {
                use rustc::ty::layout::Layout::*;
                match *dest_layout {
                    Univariant { ref variant, .. } => {
                        let offsets = iter::once(0)
                            .chain(variant.offset_after_field.iter().map(|s| s.bytes()));
                        self.assign_fields(dest, offsets, operands)?;
                    }

                    Array { .. } => {
                        let elem_size = match dest_ty.sty {
                            ty::TyArray(elem_ty, _) => self.type_size(elem_ty) as u64,
                            _ => panic!("tried to assign {:?} to non-array type {:?}",
                                        kind, dest_ty),
                        };
                        let offsets = (0..).map(|i| i * elem_size);
                        self.assign_fields(dest, offsets, operands)?;
                    }

                    General { discr, ref variants, .. } => {
                        if let mir::AggregateKind::Adt(adt_def, variant, _) = *kind {
                            let discr_val = adt_def.variants[variant].disr_val.to_u64_unchecked();
                            let discr_size = discr.size().bytes() as usize;
                            self.memory.write_uint(dest, discr_val, discr_size)?;

                            let offsets = variants[variant].offset_after_field.iter()
                                .map(|s| s.bytes());
                            self.assign_fields(dest, offsets, operands)?;
                        } else {
                            panic!("tried to assign {:?} to Layout::General", kind);
                        }
                    }

                    RawNullablePointer { nndiscr, .. } => {
                        if let mir::AggregateKind::Adt(_, variant, _) = *kind {
                            if nndiscr == variant as u64 {
                                assert_eq!(operands.len(), 1);
                                let operand = &operands[0];
                                let src = self.eval_operand(operand)?;
                                let src_ty = self.operand_ty(operand);
                                self.move_(src, dest, src_ty)?;
                            } else {
                                assert_eq!(operands.len(), 0);
                                self.memory.write_isize(dest, 0)?;
                            }
                        } else {
                            panic!("tried to assign {:?} to Layout::RawNullablePointer", kind);
                        }
                    }

                    CEnum { discr, signed, .. } => {
                        assert_eq!(operands.len(), 0);
                        if let mir::AggregateKind::Adt(adt_def, variant, _) = *kind {
                            let val = adt_def.variants[variant].disr_val.to_u64_unchecked();
                            let size = discr.size().bytes() as usize;

                            if signed {
                                self.memory.write_int(dest, val as i64, size)?;
                            } else {
                                self.memory.write_uint(dest, val, size)?;
                            }
                        } else {
                            panic!("tried to assign {:?} to Layout::CEnum", kind);
                        }
                    }

                    _ => panic!("can't handle destination layout {:?} when assigning {:?}",
                                dest_layout, kind),
                }
            }

            Repeat(ref operand, _) => {
                let (elem_size, length) = match dest_ty.sty {
                    ty::TyArray(elem_ty, n) => (self.type_size(elem_ty), n),
                    _ => panic!("tried to assign array-repeat to non-array type {:?}", dest_ty),
                };

                let src = self.eval_operand(operand)?;
                for i in 0..length {
                    let elem_dest = dest.offset((i * elem_size) as isize);
                    self.memory.copy(src, elem_dest, elem_size)?;
                }
            }

            Len(ref lvalue) => {
                let src = self.eval_lvalue(lvalue)?;
                let ty = self.lvalue_ty(lvalue);
                let len = match ty.sty {
                    ty::TyArray(_, n) => n as u64,
                    ty::TySlice(_) => if let LvalueExtra::Length(n) = src.extra {
                        n
                    } else {
                        panic!("Rvalue::Len of a slice given non-slice pointer: {:?}", src);
                    },
                    _ => panic!("Rvalue::Len expected array or slice, got {:?}", ty),
                };
                self.memory.write_usize(dest, len)?;
            }

            Ref(_, _, ref lvalue) => {
                let lv = self.eval_lvalue(lvalue)?;
                self.memory.write_ptr(dest, lv.ptr)?;
                match lv.extra {
                    LvalueExtra::None => {},
                    LvalueExtra::Length(len) => {
                        let len_ptr = dest.offset(self.memory.pointer_size as isize);
                        self.memory.write_usize(len_ptr, len)?;
                    }
                    LvalueExtra::DowncastVariant(..) =>
                        panic!("attempted to take a reference to an enum downcast lvalue"),
                }
            }

            Box(ty) => {
                let size = self.type_size(ty);
                let ptr = self.memory.allocate(size);
                self.memory.write_ptr(dest, ptr)?;
            }

            Cast(kind, ref operand, dest_ty) => {
                let src = self.eval_operand(operand)?;
                let src_ty = self.operand_ty(operand);

                use rustc::mir::repr::CastKind::*;
                match kind {
                    Unsize => {
                        self.move_(src, dest, src_ty)?;
                        let src_pointee_ty = pointee_type(src_ty).unwrap();
                        let dest_pointee_ty = pointee_type(dest_ty).unwrap();

                        match (&src_pointee_ty.sty, &dest_pointee_ty.sty) {
                            (&ty::TyArray(_, length), &ty::TySlice(_)) => {
                                let len_ptr = dest.offset(self.memory.pointer_size as isize);
                                self.memory.write_usize(len_ptr, length as u64)?;
                            }

                            _ => panic!("can't handle cast: {:?}", rvalue),
                        }
                    }

                    Misc => {
                        // FIXME(solson): Wrong for almost everything.
                        let size = dest_layout.size(&self.tcx.data_layout).bytes() as usize;
                        self.memory.copy(src, dest, size)?;
                    }

                    _ => panic!("can't handle cast: {:?}", rvalue),
                }
            }

            Slice { .. } => unimplemented!(),
            InlineAsm { .. } => unimplemented!(),
        }

        Ok(())
    }

    fn eval_operand(&mut self, op: &mir::Operand<'tcx>) -> EvalResult<Pointer> {
        use rustc::mir::repr::Operand::*;
        match *op {
            Consume(ref lvalue) => Ok(self.eval_lvalue(lvalue)?.to_ptr()),
            Constant(mir::Constant { ref literal, .. }) => {
                use rustc::mir::repr::Literal::*;
                match *literal {
                    Value { ref value } => Ok(self.const_to_ptr(value)?),
                    Item { .. } => unimplemented!(),
                    Promoted { index } => {
                        // TODO(solson): Mark constants and statics as read-only and cache their
                        // values.
                        let current_mir = self.mir();
                        let mir = &current_mir.promoted[index];
                        self.call_nested(mir).map(Option::unwrap)
                    }
                }
            }
        }
    }

    fn eval_lvalue(&mut self, lvalue: &mir::Lvalue<'tcx>) -> EvalResult<Lvalue> {
        use rustc::mir::repr::Lvalue::*;
        let ptr = match *lvalue {
            ReturnPointer => self.frame().return_ptr
                .expect("ReturnPointer used in a function with no return value"),
            Arg(i) => self.frame().locals[i as usize],
            Var(i) => self.frame().locals[self.frame().var_offset + i as usize],
            Temp(i) => self.frame().locals[self.frame().temp_offset + i as usize],

            Static(def_id) => {
                // TODO(solson): Mark constants and statics as read-only and cache their values.
                let mir = self.load_mir(def_id);
                self.call_nested(&mir)?.unwrap()
            }

            Projection(ref proj) => {
                let base = self.eval_lvalue(&proj.base)?;
                let base_ty = self.lvalue_ty(&proj.base);
                let base_layout = self.type_layout(base_ty);

                use rustc::mir::repr::ProjectionElem::*;
                match proj.elem {
                    Field(field, _) => {
                        let variant = match *base_layout {
                            Layout::Univariant { ref variant, .. } => variant,
                            Layout::General { ref variants, .. } => {
                                if let LvalueExtra::DowncastVariant(variant_idx) = base.extra {
                                    &variants[variant_idx]
                                } else {
                                    panic!("field access on enum had no variant index");
                                }
                            }
                            Layout::RawNullablePointer { .. } => {
                                assert_eq!(field.index(), 0);
                                return Ok(base);
                            }
                            _ => panic!("field access on non-product type: {:?}", base_layout),
                        };

                        let offset = variant.field_offset(field.index()).bytes();
                        base.ptr.offset(offset as isize)
                    },

                    Downcast(_, variant) => match *base_layout {
                        Layout::General { discr, .. } => {
                            return Ok(Lvalue {
                                ptr: base.ptr.offset(discr.size().bytes() as isize),
                                extra: LvalueExtra::DowncastVariant(variant),
                            });
                        }
                        Layout::RawNullablePointer { .. } => return Ok(base),
                        _ => panic!("variant downcast on non-aggregate type: {:?}", base_layout),
                    },

                    Deref => {
                        let pointee_ty = pointee_type(base_ty).expect("Deref of non-pointer");
                        let ptr = self.memory.read_ptr(base.ptr)?;
                        let extra = match pointee_ty.sty {
                            ty::TySlice(_) | ty::TyStr => {
                                let len_ptr = base.ptr.offset(self.memory.pointer_size as isize);
                                let len = self.memory.read_usize(len_ptr)?;
                                LvalueExtra::Length(len)
                            }
                            ty::TyTrait(_) => unimplemented!(),
                            _ => LvalueExtra::None,
                        };
                        return Ok(Lvalue { ptr: ptr, extra: extra });
                    }

                    Index(ref operand) => {
                        let elem_size = match base_ty.sty {
                            ty::TyArray(elem_ty, _) |
                            ty::TySlice(elem_ty) => self.type_size(elem_ty),
                            _ => panic!("indexing expected an array or slice, got {:?}", base_ty),
                        };
                        let n_ptr = self.eval_operand(operand)?;
                        let n = self.memory.read_usize(n_ptr)?;
                        base.ptr.offset(n as isize * elem_size as isize)
                    }

                    ConstantIndex { .. } => unimplemented!(),
                }
            }
        };

        Ok(Lvalue { ptr: ptr, extra: LvalueExtra::None })
    }

    // TODO(solson): Try making const_to_primval instead.
    fn const_to_ptr(&mut self, const_val: &const_val::ConstVal) -> EvalResult<Pointer> {
        use rustc::middle::const_val::ConstVal::*;
        match *const_val {
            Float(_f) => unimplemented!(),
            Integral(int) => {
                // TODO(solson): Check int constant type.
                let ptr = self.memory.allocate(8);
                self.memory.write_uint(ptr, int.to_u64_unchecked(), 8)?;
                Ok(ptr)
            }
            Str(ref s) => {
                let psize = self.memory.pointer_size;
                let static_ptr = self.memory.allocate(s.len());
                let ptr = self.memory.allocate(psize * 2);
                self.memory.write_bytes(static_ptr, s.as_bytes())?;
                self.memory.write_ptr(ptr, static_ptr)?;
                self.memory.write_usize(ptr.offset(psize as isize), s.len() as u64)?;
                Ok(ptr)
            }
            ByteStr(ref bs) => {
                let psize = self.memory.pointer_size;
                let static_ptr = self.memory.allocate(bs.len());
                let ptr = self.memory.allocate(psize);
                self.memory.write_bytes(static_ptr, bs)?;
                self.memory.write_ptr(ptr, static_ptr)?;
                Ok(ptr)
            }
            Bool(b) => {
                let ptr = self.memory.allocate(1);
                self.memory.write_bool(ptr, b)?;
                Ok(ptr)
            }
            Char(_c)          => unimplemented!(),
            Struct(_node_id)  => unimplemented!(),
            Tuple(_node_id)   => unimplemented!(),
            Function(_def_id) => unimplemented!(),
            Array(_, _)       => unimplemented!(),
            Repeat(_, _)      => unimplemented!(),
            Dummy             => unimplemented!(),
        }
    }

    fn lvalue_ty(&self, lvalue: &mir::Lvalue<'tcx>) -> ty::Ty<'tcx> {
        self.monomorphize(self.mir().lvalue_ty(self.tcx, lvalue).to_ty(self.tcx))
    }

    fn operand_ty(&self, operand: &mir::Operand<'tcx>) -> ty::Ty<'tcx> {
        self.monomorphize(self.mir().operand_ty(self.tcx, operand))
    }

    fn monomorphize(&self, ty: ty::Ty<'tcx>) -> ty::Ty<'tcx> {
        let substituted = ty.subst(self.tcx, self.substs());
        self.tcx.normalize_associated_type(&substituted)
    }

    fn type_needs_drop(&self, ty: ty::Ty<'tcx>) -> bool {
        self.tcx.type_needs_drop_given_env(ty, &self.tcx.empty_parameter_environment())
    }

    fn move_(&mut self, src: Pointer, dest: Pointer, ty: ty::Ty<'tcx>) -> EvalResult<()> {
        let size = self.type_size(ty);
        self.memory.copy(src, dest, size)?;
        if self.type_needs_drop(ty) {
            self.memory.drop_fill(src, size)?;
        }
        Ok(())
    }

    fn type_is_sized(&self, ty: ty::Ty<'tcx>) -> bool {
        ty.is_sized(self.tcx, &self.tcx.empty_parameter_environment(), DUMMY_SP)
    }

    fn type_size(&self, ty: ty::Ty<'tcx>) -> usize {
        self.type_layout(ty).size(&self.tcx.data_layout).bytes() as usize
    }

    fn type_layout(&self, ty: ty::Ty<'tcx>) -> &'tcx Layout {
        // TODO(solson): Is this inefficient? Needs investigation.
        let ty = self.monomorphize(ty);

        self.tcx.normalizing_infer_ctxt(ProjectionMode::Any).enter(|infcx| {
            // TODO(solson): Report this error properly.
            ty.layout(&infcx).unwrap()
        })
    }

    pub fn read_primval(&mut self, ptr: Pointer, ty: ty::Ty<'tcx>) -> EvalResult<PrimVal> {
        use syntax::ast::{IntTy, UintTy};
        let val = match ty.sty {
            ty::TyBool              => PrimVal::Bool(self.memory.read_bool(ptr)?),
            ty::TyInt(IntTy::I8)    => PrimVal::I8(self.memory.read_int(ptr, 1)? as i8),
            ty::TyInt(IntTy::I16)   => PrimVal::I16(self.memory.read_int(ptr, 2)? as i16),
            ty::TyInt(IntTy::I32)   => PrimVal::I32(self.memory.read_int(ptr, 4)? as i32),
            ty::TyInt(IntTy::I64)   => PrimVal::I64(self.memory.read_int(ptr, 8)? as i64),
            ty::TyUint(UintTy::U8)  => PrimVal::U8(self.memory.read_uint(ptr, 1)? as u8),
            ty::TyUint(UintTy::U16) => PrimVal::U16(self.memory.read_uint(ptr, 2)? as u16),
            ty::TyUint(UintTy::U32) => PrimVal::U32(self.memory.read_uint(ptr, 4)? as u32),
            ty::TyUint(UintTy::U64) => PrimVal::U64(self.memory.read_uint(ptr, 8)? as u64),

            // TODO(solson): Pick the PrimVal dynamically.
            ty::TyInt(IntTy::Is)   => PrimVal::I64(self.memory.read_isize(ptr)?),
            ty::TyUint(UintTy::Us) => PrimVal::U64(self.memory.read_usize(ptr)?),

            ty::TyRef(_, ty::TypeAndMut { ty, .. }) |
            ty::TyRawPtr(ty::TypeAndMut { ty, .. }) => {
                if self.type_is_sized(ty) {
                    match self.memory.read_ptr(ptr) {
                        Ok(p) => PrimVal::AbstractPtr(p),
                        Err(EvalError::ReadBytesAsPointer) => {
                            PrimVal::IntegerPtr(self.memory.read_usize(ptr)?)
                        }
                        Err(e) => return Err(e),
                    }
                } else {
                    panic!("unimplemented: primitive read of fat pointer type: {:?}", ty);
                }
            }

            _ => panic!("primitive read of non-primitive type: {:?}", ty),
        };
        Ok(val)
    }

    fn frame(&self) -> &Frame<'mir, 'tcx> {
        self.stack.last().expect("no call frames exist")
    }

    fn frame_mut(&mut self) -> &mut Frame<'mir, 'tcx> {
        self.stack.last_mut().expect("no call frames exist")
    }

    fn mir(&self) -> CachedMir<'mir, 'tcx> {
        self.frame().mir.clone()
    }

    fn substs(&self) -> &'tcx Substs<'tcx> {
        self.substs_stack.last().cloned().unwrap_or_else(|| self.tcx.mk_substs(Substs::empty()))
    }

    fn load_mir(&self, def_id: DefId) -> CachedMir<'mir, 'tcx> {
        match self.tcx.map.as_local_node_id(def_id) {
            Some(node_id) => CachedMir::Ref(self.mir_map.map.get(&node_id).unwrap()),
            None => {
                let mut mir_cache = self.mir_cache.borrow_mut();
                if let Some(mir) = mir_cache.get(&def_id) {
                    return CachedMir::Owned(mir.clone());
                }

                let cs = &self.tcx.sess.cstore;
                let mir = cs.maybe_get_item_mir(self.tcx, def_id).unwrap_or_else(|| {
                    panic!("no mir for {:?}", def_id);
                });
                let cached = Rc::new(mir);
                mir_cache.insert(def_id, cached.clone());
                CachedMir::Owned(cached)
            }
        }
    }

    fn fulfill_obligation(&self, trait_ref: ty::PolyTraitRef<'tcx>) -> traits::Vtable<'tcx, ()> {
        // Do the initial selection for the obligation. This yields the shallow result we are
        // looking for -- that is, what specific impl.
        self.tcx.normalizing_infer_ctxt(ProjectionMode::Any).enter(|infcx| {
            let mut selcx = traits::SelectionContext::new(&infcx);

            let obligation = traits::Obligation::new(
                traits::ObligationCause::misc(DUMMY_SP, ast::DUMMY_NODE_ID),
                trait_ref.to_poly_trait_predicate(),
            );
            let selection = selcx.select(&obligation).unwrap().unwrap();

            // Currently, we use a fulfillment context to completely resolve all nested obligations.
            // This is because they can inform the inference of the impl's type parameters.
            let mut fulfill_cx = traits::FulfillmentContext::new();
            let vtable = selection.map(|predicate| {
                fulfill_cx.register_predicate_obligation(&infcx, predicate);
            });
            infcx.drain_fulfillment_cx_or_panic(DUMMY_SP, &mut fulfill_cx, &vtable)
        })
    }

    /// Trait method, which has to be resolved to an impl method.
    pub fn trait_method(
        &self,
        def_id: DefId,
        substs: &'tcx Substs<'tcx>
    ) -> (DefId, &'tcx Substs<'tcx>) {
        let method_item = self.tcx.impl_or_trait_item(def_id);
        let trait_id = method_item.container().id();
        let trait_ref = ty::Binder(substs.to_trait_ref(self.tcx, trait_id));
        match self.fulfill_obligation(trait_ref) {
            traits::VtableImpl(vtable_impl) => {
                let impl_did = vtable_impl.impl_def_id;
                let mname = self.tcx.item_name(def_id);
                // Create a concatenated set of substitutions which includes those from the impl
                // and those from the method:
                let impl_substs = vtable_impl.substs.with_method_from(substs);
                let substs = self.tcx.mk_substs(impl_substs);
                let mth = get_impl_method(self.tcx, impl_did, substs, mname);

                (mth.method.def_id, mth.substs)
            }

            traits::VtableClosure(vtable_closure) =>
                (vtable_closure.closure_def_id, vtable_closure.substs.func_substs),

            traits::VtableFnPointer(_fn_ty) => {
                let _trait_closure_kind = self.tcx.lang_items.fn_trait_kind(trait_id).unwrap();
                unimplemented!()
                // let llfn = trans_fn_pointer_shim(ccx, trait_closure_kind, fn_ty);

                // let method_ty = def_ty(tcx, def_id, substs);
                // let fn_ptr_ty = match method_ty.sty {
                //     ty::TyFnDef(_, _, fty) => tcx.mk_ty(ty::TyFnPtr(fty)),
                //     _ => unreachable!("expected fn item type, found {}",
                //                       method_ty)
                // };
                // Callee::ptr(immediate_rvalue(llfn, fn_ptr_ty))
            }

            traits::VtableObject(ref _data) => {
                unimplemented!()
                // Callee {
                //     data: Virtual(traits::get_vtable_index_of_object_method(
                //                   tcx, data, def_id)),
                //                   ty: def_ty(tcx, def_id, substs)
                // }
            }
            vtable => unreachable!("resolved vtable bad vtable {:?} in trans", vtable),
        }
    }
}

fn pointee_type(ptr_ty: ty::Ty) -> Option<ty::Ty> {
    match ptr_ty.sty {
        ty::TyRef(_, ty::TypeAndMut { ty, .. }) |
        ty::TyRawPtr(ty::TypeAndMut { ty, .. }) |
        ty::TyBox(ty) => {
            Some(ty)
        }
        _ => None,
    }
}

impl Lvalue {
    fn to_ptr(self) -> Pointer {
        assert_eq!(self.extra, LvalueExtra::None);
        self.ptr
    }
}

impl<'mir, 'tcx: 'mir> Deref for CachedMir<'mir, 'tcx> {
    type Target = mir::Mir<'tcx>;
    fn deref(&self) -> &mir::Mir<'tcx> {
        match *self {
            CachedMir::Ref(r) => r,
            CachedMir::Owned(ref rc) => &rc,
        }
    }
}

#[derive(Debug)]
pub struct ImplMethod<'tcx> {
    pub method: Rc<ty::Method<'tcx>>,
    pub substs: &'tcx Substs<'tcx>,
    pub is_provided: bool,
}

/// Locates the applicable definition of a method, given its name.
pub fn get_impl_method<'a, 'tcx>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    impl_def_id: DefId,
    substs: &'tcx Substs<'tcx>,
    name: ast::Name,
) -> ImplMethod<'tcx> {
    assert!(!substs.types.needs_infer());

    let trait_def_id = tcx.trait_id_of_impl(impl_def_id).unwrap();
    let trait_def = tcx.lookup_trait_def(trait_def_id);

    match trait_def.ancestors(impl_def_id).fn_defs(tcx, name).next() {
        Some(node_item) => {
            let substs = tcx.normalizing_infer_ctxt(ProjectionMode::Any).enter(|infcx| {
                let substs = traits::translate_substs(&infcx, impl_def_id,
                                                      substs, node_item.node);
                tcx.lift(&substs).unwrap_or_else(|| {
                    bug!("trans::meth::get_impl_method: translate_substs \
                          returned {:?} which contains inference types/regions",
                         substs);
                })
            });
            ImplMethod {
                method: node_item.item,
                substs: substs,
                is_provided: node_item.node.is_from_trait(),
            }
        }
        None => {
            bug!("method {:?} not found in {:?}", name, impl_def_id)
        }
    }
}

pub fn interpret_start_points<'a, 'tcx>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    mir_map: &MirMap<'tcx>,
) {
    for (&id, mir) in &mir_map.map {
        for attr in tcx.map.attrs(id) {
            use syntax::attr::AttrMetaMethods;
            if attr.check_name("miri_run") {
                let item = tcx.map.expect_item(id);

                println!("Interpreting: {}", item.name);

                let mut gecx = GlobalEvalContext::new(tcx, mir_map);
                let mut fecx = FnEvalContext::new(&mut gecx);
                match fecx.call_nested(mir) {
                    Ok(Some(return_ptr)) => fecx.memory.dump(return_ptr.alloc_id),
                    Ok(None) => println!("(diverging function returned)"),
                    Err(_e) => {
                        // TODO(solson): Detect whether the error was already reported or not.
                        // tcx.sess.err(&e.to_string());
                    }
                }

                println!("");
            }
        }
    }
}

// TODO(solson): Upstream these methods into rustc::ty::layout.

trait IntegerExt {
    fn size(self) -> Size;
}

impl IntegerExt for layout::Integer {
    fn size(self) -> Size {
        use rustc::ty::layout::Integer::*;
        match self {
            I1 | I8 => Size::from_bits(8),
            I16 => Size::from_bits(16),
            I32 => Size::from_bits(32),
            I64 => Size::from_bits(64),
        }
    }
}

trait StructExt {
    fn field_offset(&self, index: usize) -> Size;
}

impl StructExt for layout::Struct {
    fn field_offset(&self, index: usize) -> Size {
        if index == 0 {
            Size::from_bytes(0)
        } else {
            self.offset_after_field[index - 1]
        }
    }
}
