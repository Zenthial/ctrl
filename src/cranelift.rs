use std::collections::HashMap;

use cranelift::codegen::entity::EntityRef;
use cranelift::codegen::ir::types::*;
use cranelift::codegen::ir::{AbiParam, Block, Function, InstBuilder, UserFuncName};
use cranelift::codegen::settings;
use cranelift::codegen::verifier::verify_function;
use cranelift::frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift::prelude::{Imm64, Value};
use cranelift_module::{default_libcall_names, Linkage, Module};
use cranelift_native::builder;
use cranelift_object::{ObjectBuilder, ObjectModule};

use crate::parse::{
    Block as BlockExpr, Bop, BuiltinType, Expression, Function as Func, Literal, Type as _, T,
};
use anyhow::Result;

pub struct Ctx {
    variables: HashMap<String, Variable>,
    variable_counter: usize,
}

impl Ctx {
    fn new() -> Self {
        Self {
            variables: HashMap::new(),
            variable_counter: 0,
        }
    }

    fn declare_variable(
        &mut self,
        name: &str,
        builder: &mut FunctionBuilder,
        ty: Type,
    ) -> Variable {
        let var = Variable::new(self.variable_counter);
        self.variable_counter += 1;
        builder.declare_var(var, ty);
        self.variables.insert(name.to_string(), var);
        var
    }

    fn get_variable(&self, name: &str) -> Option<Variable> {
        self.variables.get(name).cloned()
    }
}

// converts a T to a cranelift Type
// option represents the unit type
fn type_to_cranelift(ty: &T) -> Option<Type> {
    use T::*;

    match ty {
        Hole => panic!("Hit bottom type when translating to IR"),
        Unit => None,
        BuiltIn(b) => match b {
            BuiltinType::Int => Some(I32),
            BuiltinType::Float => Some(F64),
            BuiltinType::String | BuiltinType::Array => Some(I64), // ptr
            BuiltinType::Char | BuiltinType::Bool => Some(I8),
        },
        Record(_)
        | Function {
            param_tys: _,
            return_ty: _,
        } => Some(I64), // ptr
    }
}

fn translate_literal(literal: &Literal, builder: &mut FunctionBuilder<'_>) -> Value {
    match literal {
        Literal::Bool(b) => builder.ins().iconst(I8, *b as i64),
        Literal::Int(i) => builder.ins().iconst(I32, *i as i64),
    }
}

fn translate_assignment(
    ident: &str,
    binding: &Expression,
    ty: T,
    builder: &mut FunctionBuilder<'_>,
    ctx: &mut Ctx,
) {
    let val = translate_expression(binding, builder, ctx);
    let var = ctx.declare_variable(ident, builder, type_to_cranelift(&ty).unwrap());
    builder.def_var(var, val);
}

fn translate_infix(
    operation: &Bop,
    lhs: &Expression,
    rhs: &Expression,
    builder: &mut FunctionBuilder<'_>,
    ctx: &mut Ctx,
) -> Value {
    let left_val = translate_expression(lhs, builder, ctx);
    let right_val = translate_expression(rhs, builder, ctx);

    match operation {
        Bop::Plus => builder.ins().iadd(left_val, right_val),
        Bop::Min => builder.ins().isub(left_val, right_val),
        Bop::Mul => builder.ins().imul(left_val, right_val),
        Bop::Div => builder.ins().sdiv(left_val, right_val),
        _ => unimplemented!(),
    }
}

fn translate_block(b: &BlockExpr, builder: &mut FunctionBuilder<'_>, ctx: &mut Ctx) -> Block {
    let object_block = builder.create_block();
    for inst in &b.instructions {
        let _ = translate_expression(inst, builder, ctx);
    }

    object_block
}

fn translate_expression(
    expr: &Expression,
    builder: &mut FunctionBuilder<'_>,
    ctx: &mut Ctx,
) -> Value {
    match expr {
        Expression::Literal(literal) => translate_literal(literal, builder),
        Expression::Assignment { ident, binding } => {
            let ty = expr.type_of(&HashMap::new());
            translate_assignment(ident, binding, ty, builder, ctx);
            builder.ins().iconst(I64, 0) // placeholder nullptr
        }
        Expression::Identifier(name) => {
            if let Some(var) = ctx.get_variable(name) {
                builder.use_var(var)
            } else {
                panic!("undefined identifier {}", name);
            }
        }
        Expression::Infix {
            operation,
            lhs,
            rhs,
        } => translate_infix(operation, lhs, rhs, builder, ctx),
        Expression::Return(expr) => {
            let return_val = translate_expression(expr, builder, ctx);
            builder.ins().return_(&[return_val]);
            return_val
        }
        Expression::Block(b) => {
            translate_block(b, builder, ctx);
            builder.ins().iconst(I64, 0) // placeholder nullptr
        }
        _ => unimplemented!(),
    }
}

fn translate_function(func: &Func, module: &mut ObjectModule) -> Result<()> {
    let param_tys: Vec<Type> = func
        .params
        .iter()
        .filter_map(|(_, ty)| type_to_cranelift(ty))
        .collect();

    let mut func_sig = module.make_signature();
    for ty in &param_tys {
        func_sig.params.push(AbiParam::new(*ty))
    }

    if let Some(ty) = type_to_cranelift(&func.return_ty) {
        func_sig.returns.push(AbiParam::new(ty));
    }

    let func_id = module.declare_function(&func.name, Linkage::Export, &func_sig)?;
    // individual context for the function
    let mut func_ctx = module.make_context();
    func_ctx.func.signature = func_sig.clone();

    // create the function builder context
    let mut fb_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut func_ctx.func, &mut fb_ctx);

    let block = builder.create_block();
    builder.switch_to_block(block);
    builder.seal_block(block);

    // translation level context to track variables inside the function
    let mut ctx = Ctx::new();

    for (idx, (name, _)) in func.params.iter().enumerate() {
        let param_var = ctx.declare_variable(name, &mut builder, param_tys[idx]);
        let param_val = builder.block_params(block)[idx];
        builder.def_var(param_var, param_val);
    }

    for expr in &func.body.instructions {
        let _ = translate_expression(expr, &mut builder, &mut ctx);
    }

    builder.finalize();
    module.define_function(func_id, &mut func_ctx)?;

    Ok(())
}

pub fn translate(ast: Vec<Expression>, module_name: &str) -> Result<()> {
    let flags = settings::Flags::new(settings::builder());
    let isa_builder = cranelift_native::builder().expect("arch isnt supported");
    let isa = isa_builder.finish(flags).expect("isa builder not finished");

    let object_builder = ObjectBuilder::new(isa, "test", default_libcall_names())
        .expect("object builder not supported");

    let mut module = ObjectModule::new(object_builder);

    for expr in ast {
        match expr {
            Expression::Function(func) => translate_function(&func, &mut module)?,
            t => panic!("top level must be function, got {t:?}"),
        }
    }

    let object = module.finish();
    std::fs::write(format!("{module_name}.o"), object.emit()?)?;
    Ok(())
}

pub fn generate() -> Result<()> {
    let flags = settings::Flags::new(settings::builder());
    let isa_builder = cranelift_native::builder().expect("arch isnt supported");
    let isa = isa_builder.finish(flags).expect("isa builder not finished");

    let object_builder = ObjectBuilder::new(isa, "test", default_libcall_names())
        .expect("object builder not supported");

    let mut module = ObjectModule::new(object_builder);

    let mut sig = module.make_signature();
    sig.returns.push(AbiParam::new(I32));

    let func_id = module.declare_function("tester", Linkage::Export, &sig)?;

    {
        let mut ctx = module.make_context();
        ctx.func.signature = sig.clone();

        let mut fb_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);

        let block0 = builder.create_block();
        let block1 = builder.create_block();

        let x = Variable::new(0);
        let z = Variable::new(1);

        builder.declare_var(x, I32);
        builder.declare_var(z, I32);
        builder.append_block_params_for_function_params(block0);

        builder.switch_to_block(block0);
        builder.seal_block(block0);

        builder.switch_to_block(block1);
        let arg1 = builder.use_var(x);
        let arg2 = builder.ins().iconst(I32, 5);
        let ret = builder.ins().iadd(arg1, arg2);
        builder.def_var(z, ret);

        let ret_arg = builder.use_var(z);
        builder.ins().return_(&[ret_arg]);
        builder.seal_block(block1);

        builder.finalize();

        module.define_function(func_id, &mut ctx)?;
    }

    let object = module.finish();
    std::fs::write("example.o", object.emit()?)?;
    println!("Object file 'example.o' generated successfully.");

    // let flags = settings::Flags::new(settings::builder());
    // let res = verify_function(&func, &flags);
    // println!("{}", func.display());
    // if let Err(errors) = res {
    //     panic!("{}", errors);
    // }

    Ok(())
}
