// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

//! Translates and validates specification language fragments as they are output from the Move
//! compiler's expansion phase and adds them to the environment (which was initialized from the
//! byte code). This includes identifying the Move sub-language supported by the specification
//! system, as well as type checking it and translating it to the spec language ast.

use crate::{
    ast::{Address, Attribute, ModuleName, Operation, QualifiedSymbol, Spec, Value},
    builder::builtins,
    intrinsics::IntrinsicDecl,
    model::{
        FunId, FunctionKind, GlobalEnv, Loc, ModuleId, Parameter, QualifiedId, QualifiedInstId,
        SpecFunId, SpecVarId, StructId, TypeParameter, TypeParameterKind,
    },
    symbol::Symbol,
    ty::{
        gen_get_ty_param_kinds, infer_abilities, infer_and_check_abilities, is_phantom_type_arg,
        Constraint, Type,
    },
};
use codespan_reporting::diagnostic::Severity;
use itertools::Itertools;
use move_binary_format::file_format::{AbilitySet, Visibility};
use move_compiler::{expansion::ast as EA, parser::ast as PA, shared::NumericalAddress};
use move_core_types::account_address::AccountAddress;
use std::collections::{BTreeMap, BTreeSet};

/// A builder is used to enter a sequence of modules in acyclic dependency order into the model. The
/// builder maintains the incremental state of this process, such that the various tables
/// are extended with each module translated. Each table is a mapping from fully qualified names
/// (module names plus item name in the module) to the entity.
#[derive(Debug)]
pub(crate) struct ModelBuilder<'env> {
    /// The global environment we are building.
    pub env: &'env mut GlobalEnv,
    /// A symbol table for specification functions. Because of overloading, an entry can
    /// contain multiple functions.
    pub spec_fun_table: BTreeMap<QualifiedSymbol, Vec<SpecOrBuiltinFunEntry>>,
    /// A symbol table for specification variables.
    pub spec_var_table: BTreeMap<QualifiedSymbol, SpecVarEntry>,
    /// A symbol table for specification schemas.
    pub spec_schema_table: BTreeMap<QualifiedSymbol, SpecSchemaEntry>,
    /// A symbol table storing unused schemas, used later to generate warnings. All schemas
    /// are initially in the table and are removed when they are used in expressions.
    pub unused_schema_set: BTreeSet<QualifiedSymbol>,
    /// A symbol table for structs.
    pub struct_table: BTreeMap<QualifiedSymbol, StructEntry>,
    /// A reverse mapping from ModuleId/StructId pairs to QualifiedSymbol. This
    /// is used for visualization of types in error messages.
    pub reverse_struct_table: BTreeMap<(ModuleId, StructId), QualifiedSymbol>,
    /// A symbol table for functions.
    pub fun_table: BTreeMap<QualifiedSymbol, FunEntry>,
    /// A symbol table for constants.
    pub const_table: BTreeMap<QualifiedSymbol, ConstEntry>,
    /// A list of intrinsic declarations
    pub intrinsics: Vec<IntrinsicDecl>,
    /// A module lookup table from names to their ids.
    pub module_table: BTreeMap<ModuleName, ModuleId>,
}

/// A declaration of a specification function or operator in the builders state.
/// TODO(wrwg): we should unify this type with `FunEntry` using a new `FunctionKind::Spec` kind.
#[derive(Debug, Clone)]
pub(crate) struct SpecOrBuiltinFunEntry {
    #[allow(dead_code)]
    pub loc: Loc,
    pub oper: Operation,
    pub type_params: Vec<TypeParameter>,
    pub type_param_constraints: BTreeMap<usize, Constraint>,
    pub params: Vec<Parameter>,
    pub result_type: Type,
    pub visibility: EntryVisibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum EntryVisibility {
    Spec,
    Impl,
    SpecAndImpl,
}

/// A declaration of a specification variable in the builders state.
#[derive(Debug, Clone)]
pub(crate) struct SpecVarEntry {
    pub loc: Loc,
    pub module_id: ModuleId,
    #[allow(dead_code)]
    pub var_id: SpecVarId,
    pub type_params: Vec<TypeParameter>,
    pub type_: Type,
}

/// A declaration of a schema in the builders state.
#[derive(Debug)]
pub(crate) struct SpecSchemaEntry {
    pub loc: Loc,
    #[allow(dead_code)]
    pub name: QualifiedSymbol,
    pub module_id: ModuleId,
    pub type_params: Vec<TypeParameter>,
    // The local variables declared in the schema.
    pub vars: Vec<Parameter>,
    // The specifications in in this schema.
    pub spec: Spec,
    // All variables in scope of this schema, including those introduced by included schemas.
    pub all_vars: BTreeMap<Symbol, LocalVarEntry>,
    // The specification included from other schemas, after renaming and type instantiation.
    pub included_spec: Spec,
}

/// A declaration of a struct.
#[derive(Debug, Clone)]
pub(crate) struct StructEntry {
    pub loc: Loc,
    pub module_id: ModuleId,
    pub struct_id: StructId,
    pub type_params: Vec<TypeParameter>,
    pub abilities: AbilitySet,
    pub fields: Option<BTreeMap<Symbol, (Loc, usize, Type)>>,
    pub attributes: Vec<Attribute>,
}

/// A declaration of a function.
#[derive(Debug, Clone)]
pub(crate) struct FunEntry {
    pub loc: Loc,      // location of the entire function span
    pub name_loc: Loc, // location of just the function name
    pub module_id: ModuleId,
    pub fun_id: FunId,
    pub visibility: Visibility,
    pub is_native: bool,
    pub kind: FunctionKind,
    pub type_params: Vec<TypeParameter>,
    pub params: Vec<Parameter>,
    pub result_type: Type,
    pub attributes: Vec<Attribute>,
    pub inline_specs: BTreeMap<EA::SpecId, EA::SpecBlock>,
}

#[derive(Debug, Clone)]
pub(crate) enum AnyFunEntry {
    SpecOrBuiltin(SpecOrBuiltinFunEntry),
    UserFun(FunEntry),
}

impl AnyFunEntry {
    pub fn get_signature(&self) -> (&[TypeParameter], &[Parameter], &Type) {
        match self {
            AnyFunEntry::SpecOrBuiltin(e) => (&e.type_params, &e.params, &e.result_type),
            AnyFunEntry::UserFun(e) => (&e.type_params, &e.params, &e.result_type),
        }
    }

    pub fn get_operation(&self) -> Operation {
        match self {
            AnyFunEntry::SpecOrBuiltin(e) => e.oper.clone(),
            AnyFunEntry::UserFun(e) => Operation::MoveFunction(e.module_id, e.fun_id),
        }
    }

    pub fn is_equality_on_ref(&self) -> bool {
        matches!(self.get_operation(), Operation::Eq | Operation::Neq)
            && self.get_signature().1[0].1.is_reference()
    }

    pub fn is_equality_on_non_ref(&self) -> bool {
        matches!(self.get_operation(), Operation::Eq | Operation::Neq)
            && !self.get_signature().1[0].1.is_reference()
    }
}

impl From<SpecOrBuiltinFunEntry> for AnyFunEntry {
    fn from(value: SpecOrBuiltinFunEntry) -> Self {
        Self::SpecOrBuiltin(value)
    }
}

impl From<FunEntry> for AnyFunEntry {
    fn from(value: FunEntry) -> Self {
        Self::UserFun(value)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ConstEntry {
    pub loc: Loc,
    pub ty: Type,
    pub value: Value,
    pub visibility: EntryVisibility,
}

impl<'env> ModelBuilder<'env> {
    /// Creates a builders.
    pub fn new(env: &'env mut GlobalEnv) -> Self {
        let mut translator = ModelBuilder {
            env,
            spec_fun_table: BTreeMap::new(),
            spec_var_table: BTreeMap::new(),
            spec_schema_table: BTreeMap::new(),
            unused_schema_set: BTreeSet::new(),
            struct_table: BTreeMap::new(),
            reverse_struct_table: BTreeMap::new(),
            fun_table: BTreeMap::new(),
            const_table: BTreeMap::new(),
            intrinsics: Vec::new(),
            module_table: BTreeMap::new(),
        };
        builtins::declare_builtins(&mut translator);
        translator
    }

    /// Shortcut for translating a Move AST location into ours.
    pub fn to_loc(&self, loc: &move_ir_types::location::Loc) -> Loc {
        self.env.to_loc(loc)
    }

    /// Reports a type checking error.
    pub fn error(&self, at: &Loc, msg: &str) {
        self.env.error(at, msg)
    }

    /// Reports a type checking error with notes.
    pub fn error_with_notes(&self, at: &Loc, msg: &str, notes: Vec<String>) {
        self.env.error_with_notes(at, msg, notes)
    }

    /// Shortcut for a diagnosis note.
    pub fn note(&mut self, loc: &Loc, msg: &str) {
        self.env.diag(Severity::Note, loc, msg)
    }

    /// Defines a spec function, adding it to the spec fun table.
    pub fn define_spec_or_builtin_fun(
        &mut self,
        name: QualifiedSymbol,
        entry: SpecOrBuiltinFunEntry,
    ) {
        if self.fun_table.contains_key(&name) {
            self.env.error(
                &entry.loc,
                &format!(
                    "name clash between specification and Move function `{}`",
                    name.symbol.display(self.env.symbol_pool())
                ),
            );
        }
        // TODO: check whether overloads are distinguishable
        self.spec_fun_table.entry(name).or_default().push(entry);
    }

    /// Defines a spec variable.
    pub fn define_spec_var(
        &mut self,
        loc: &Loc,
        name: QualifiedSymbol,
        module_id: ModuleId,
        var_id: SpecVarId,
        type_params: Vec<TypeParameter>,
        type_: Type,
    ) {
        let entry = SpecVarEntry {
            loc: loc.clone(),
            module_id,
            var_id,
            type_params,
            type_,
        };
        if let Some(old) = self.spec_var_table.insert(name.clone(), entry) {
            let var_name = name.display(self.env);
            self.error(loc, &format!("duplicate declaration of `{}`", var_name));
            self.note(&old.loc, &format!("previous declaration of `{}`", var_name));
        }
    }

    /// Defines a spec schema.
    pub fn define_spec_schema(
        &mut self,
        loc: &Loc,
        name: QualifiedSymbol,
        module_id: ModuleId,
        type_params: Vec<TypeParameter>,
        vars: Vec<Parameter>,
    ) {
        let entry = SpecSchemaEntry {
            loc: loc.clone(),
            name: name.clone(),
            module_id,
            type_params,
            vars,
            spec: Spec::default(),
            all_vars: BTreeMap::new(),
            included_spec: Spec::default(),
        };
        if let Some(old) = self.spec_schema_table.insert(name.clone(), entry) {
            let schema_display = name.display(self.env);
            self.error(
                loc,
                &format!("duplicate declaration of `{}`", schema_display),
            );
            self.error(
                &old.loc,
                &format!("previous declaration of `{}`", schema_display),
            );
        }
        self.unused_schema_set.insert(name);
    }

    /// Defines a struct type.
    pub fn define_struct(
        &mut self,
        loc: Loc,
        attributes: Vec<Attribute>,
        name: QualifiedSymbol,
        module_id: ModuleId,
        struct_id: StructId,
        abilities: AbilitySet,
        type_params: Vec<TypeParameter>,
        fields: Option<BTreeMap<Symbol, (Loc, usize, Type)>>,
    ) {
        let entry = StructEntry {
            loc,
            attributes,
            module_id,
            struct_id,
            abilities,
            type_params,
            fields,
        };
        self.struct_table.insert(name.clone(), entry);
        self.reverse_struct_table
            .insert((module_id, struct_id), name);
    }

    /// Defines a function.
    pub fn define_fun(&mut self, name: QualifiedSymbol, entry: FunEntry) {
        self.fun_table.insert(name, entry);
    }

    /// Defines a constant.
    pub fn define_const(&mut self, name: QualifiedSymbol, entry: ConstEntry) {
        self.const_table.insert(name, entry);
    }

    pub fn resolve_address(&self, loc: &Loc, addr: &EA::Address) -> NumericalAddress {
        match addr {
            EA::Address::Numerical(_, bytes) => bytes.value,
            EA::Address::NamedUnassigned(name) => {
                self.error(loc, &format!("Undeclared address `{}`", name));
                NumericalAddress::DEFAULT_ERROR_ADDRESS
            },
        }
    }

    /// Looks up a type (struct), reporting an error if it is not found.
    pub fn lookup_type(&self, loc: &Loc, name: &QualifiedSymbol) -> Type {
        self.struct_table
            .get(name)
            .cloned()
            .map(|e| {
                Type::Struct(
                    e.module_id,
                    e.struct_id,
                    TypeParameter::vec_to_formals(&e.type_params),
                )
            })
            .unwrap_or_else(|| {
                self.error(
                    loc,
                    &format!("undeclared `{}`", name.display_full(self.env)),
                );
                Type::Error
            })
    }

    /// Looks up the fields of a structure, with instantiated field types.
    pub fn lookup_struct_fields(&self, id: QualifiedInstId<StructId>) -> BTreeMap<Symbol, Type> {
        let entry = self.lookup_struct_entry(id.to_qualified_id());
        entry
            .fields
            .as_ref()
            .map(|f| {
                f.iter()
                    .map(|(n, (_, _, field_ty))| (*n, field_ty.instantiate(&id.inst)))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default()
    }

    /// Looks up the abilities of a struct.
    /// TODO(#12437): get rid of this once we have new UnificationContext
    pub fn lookup_struct_abilities(&self, id: QualifiedId<StructId>) -> AbilitySet {
        let entry = self.lookup_struct_entry(id);
        entry.abilities
    }

    /// Get all the structs which have been build so far.
    pub fn get_struct_ids(&self) -> impl Iterator<Item = QualifiedId<StructId>> + '_ {
        self.struct_table
            .values()
            .map(|e| e.module_id.qualified(e.struct_id))
    }

    /// Looks up the StructEntry for a qualified id.
    pub fn lookup_struct_entry(&self, id: QualifiedId<StructId>) -> &StructEntry {
        let struct_name = self
            .reverse_struct_table
            .get(&(id.module_id, id.id))
            .expect("invalid Type::Struct");
        self.struct_table
            .get(struct_name)
            .expect("invalid Type::Struct")
    }

    /// returns the type parameter kinds and the abilities of the struct
    fn get_struct_sig(&self, mid: ModuleId, sid: StructId) -> (Vec<TypeParameterKind>, AbilitySet) {
        let struct_entry = self.lookup_struct_entry(mid.qualified(sid));
        let struct_abilities = struct_entry.abilities;
        let ty_param_kinds = struct_entry
            .type_params
            .iter()
            .map(|tp| tp.1.clone())
            .collect_vec();
        (ty_param_kinds, struct_abilities)
    }

    fn gen_get_struct_sig(
        &self,
    ) -> impl Fn(ModuleId, StructId) -> (Vec<TypeParameterKind>, AbilitySet) + Copy + '_ {
        |mid, sid| self.get_struct_sig(mid, sid)
    }

    /// Specialized `ty::infer_and_check_abilities`
    /// where the abilities of type arguments are given by `ty_params`
    pub fn check_instantiation(&self, ty: &Type, ty_params: &[TypeParameter], loc: &Loc) {
        infer_and_check_abilities(
            ty,
            gen_get_ty_param_kinds(ty_params),
            self.gen_get_struct_sig(),
            loc,
            |loc, _, err| self.error(loc, err),
        );
    }

    /// Infers the abilities the given type may have,
    /// if all type params have all abilities.
    pub fn infer_abilities_may_have(&self, ty: &Type) -> AbilitySet {
        // since all type params have all abilities, it doesn't matter whether it's phantom or not
        infer_abilities(
            ty,
            |_| TypeParameterKind {
                abilities: AbilitySet::ALL,
                is_phantom: false,
            },
            self.gen_get_struct_sig(),
        )
    }

    /// Checks whether a struct is well defined.
    pub fn ability_check_struct_def(&self, struct_entry: &StructEntry) {
        if let Some(fields) = &struct_entry.fields {
            let ty_params = &struct_entry.type_params;
            for (_field_name, (loc, _field_idx, field_ty)) in fields.iter() {
                // check fields are properly instantiated
                self.check_instantiation(field_ty, ty_params, loc);
                if is_phantom_type_arg(gen_get_ty_param_kinds(ty_params), field_ty) {
                    self.error(loc, "phantom type arguments cannot be used")
                }
            }
        }
    }

    // Generate warnings about unused schemas.
    pub fn warn_unused_schemas(&self) {
        for name in &self.unused_schema_set {
            let entry = self.spec_schema_table.get(name).expect("schema defined");
            let schema_name = name.display_simple(self.env).to_string();
            let module_env = self.env.get_module(entry.module_id);
            // Warn about unused schema only if the module is a target and schema name
            // does not start with 'UNUSED'
            if module_env.is_target() && !schema_name.starts_with("UNUSED") {
                self.env.diag(
                    Severity::Note,
                    &entry.loc,
                    &format!("unused schema {}", name.display(self.env)),
                );
            }
        }
    }

    /// Returns the symbol for a binary op.
    pub fn bin_op_symbol(&self, op: &PA::BinOp_) -> QualifiedSymbol {
        QualifiedSymbol {
            module_name: self.builtin_module(),
            symbol: self.env.symbol_pool().make(op.symbol()),
        }
    }

    /// Returns the symbol for a unary op.
    pub fn unary_op_symbol(&self, op: &PA::UnaryOp_) -> QualifiedSymbol {
        QualifiedSymbol {
            module_name: self.builtin_module(),
            symbol: self.env.symbol_pool().make(op.symbol()),
        }
    }

    /// Returns the symbol for a name in the builtin module.
    pub fn builtin_qualified_symbol(&self, name: &str) -> QualifiedSymbol {
        QualifiedSymbol {
            module_name: self.builtin_module(),
            symbol: self.env.symbol_pool().make(name),
        }
    }

    /// Returns the symbol for the builtin function `old`.
    pub fn old_symbol(&self) -> Symbol {
        self.env.symbol_pool().make("old")
    }

    /// Returns the name for the pseudo builtin module.
    pub fn builtin_module(&self) -> ModuleName {
        ModuleName::new(
            Address::Numerical(AccountAddress::ZERO),
            self.env.symbol_pool().make("$$"),
        )
    }

    /// Adds a spec function to used_spec_funs set.
    pub fn add_used_spec_fun(&mut self, qid: QualifiedId<SpecFunId>) {
        self.env.used_spec_funs.insert(qid);
    }

    /// Pass model-level information to the global env
    pub fn populate_env(&mut self) {
        // register all intrinsic declarations
        for decl in &self.intrinsics {
            self.env.intrinsics.add_decl(decl);
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LocalVarEntry {
    pub loc: Loc,
    pub type_: Type,
    /// If this local is associated with an operation, this is set.
    pub operation: Option<Operation>,
    /// If this a temporary from Move code, this is it's index.
    pub temp_index: Option<usize>,
}
